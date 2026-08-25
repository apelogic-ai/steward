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
    GetProviderRequest, GetSandboxRequest, ListSandboxProvidersRequest, Sandbox as RawSandbox,
    SandboxPhase as RawSandboxPhase,
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
    SandboxObservation, SandboxRequest, SandboxRuntime, SandboxTaskOutput, SandboxTaskRequest,
    SandboxTaskRuntime,
};
#[cfg(feature = "runtime")]
use steward_types::{AgentType, RuntimeRefs};
#[cfg(feature = "runtime")]
use tokio::sync::Mutex;
#[cfg(feature = "runtime")]
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};

pub const IMPLEMENTED_PORTS: [&str; 0] = [];
const NAME_LENGTH: usize = 19;
const HASH_CHARACTERS: usize = NAME_LENGTH - 2;
const LOWER_BASE36: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
#[cfg(feature = "runtime")]
const TAR_BLOCK_BYTES: usize = 512;
#[cfg(feature = "runtime")]
const KUBERNETES_DNS_SUBDOMAIN_MAX_LENGTH: usize = 253;
#[cfg(feature = "runtime")]
const KUBERNETES_DNS_LABEL_MAX_LENGTH: usize = 63;
#[cfg(feature = "runtime")]
const RUNTIME_UID_LABEL: &str = "agents.apelogic.ai/runtime-uid";
/// Server-authored agent type for the one-shot Connections bridge operation.
pub const CONNECTIONS_BRIDGE_AGENT_TYPE: &str = "connections-bridge";
/// The sole immutable Workflow agent supported by this slice.
pub const WORKFLOW_CODEX_AGENT_TYPE: &str = "codex@0.117.0";
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
struct OpenShellProjection {
    workspace: String,
    workspace_key: String,
    sandbox: String,
    providers: Vec<String>,
    runtime_uid: String,
    image: Option<String>,
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
fn bridge_image_for_agent_type(
    agent_type: &AgentType,
    bridge_image: Option<&str>,
) -> Result<Option<String>, PortError> {
    match agent_type.name.as_str() {
        "base" | WORKFLOW_CODEX_AGENT_TYPE => Ok(None),
        CONNECTIONS_BRIDGE_AGENT_TYPE => bridge_image
            .filter(|image| is_digest_pinned_image(image))
            .map(str::to_owned)
            .map(Some)
            .ok_or_else(|| PortError::Rejected {
                reason: "Connections bridge requires a provenance-verified digest-pinned image"
                    .to_owned(),
            }),
        other => Err(PortError::Rejected {
            reason: format!("unsupported agent type: {other}"),
        }),
    }
}

#[cfg(feature = "runtime")]
fn output_archive_command(agent_type: &AgentType) -> &'static str {
    if agent_type.name == CONNECTIONS_BRIDGE_AGENT_TYPE {
        "set -eu; test -f /sandbox/steward-output/response.json; tar -cf - -C /sandbox/steward-output response.json"
    } else if agent_type.name == WORKFLOW_CODEX_AGENT_TYPE {
        "set -eu; test -s /sandbox/steward-output/result.txt; tar -cf - -C /sandbox/steward-output result.txt"
    } else {
        "set -eu; tar -cf - -C /sandbox/steward-output ."
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
    bridge_image: Option<&str>,
) -> Result<OpenShellProjection, PortError> {
    let image = bridge_image_for_agent_type(&request.agent_type, bridge_image)?;
    Ok(OpenShellProjection {
        workspace: stable_name(NameKind::Workspace, request.workspace_key.as_bytes()),
        workspace_key: request.workspace_key.clone(),
        sandbox: stable_name(NameKind::Sandbox, request.runtime.0.as_bytes()),
        providers: [
            (!request.tools.is_empty()).then(|| TOOL_PROVIDER.to_owned()),
            (!request.models.is_empty()).then(|| INFERENCE_PROVIDER.to_owned()),
        ]
        .into_iter()
        .flatten()
        .collect(),
        runtime_uid: request.runtime.0.clone(),
        image,
    })
}

#[cfg(feature = "runtime")]
fn sandbox_spec(projection: &OpenShellProjection) -> SandboxSpec {
    let mut labels = HashMap::new();
    labels.insert(RUNTIME_UID_LABEL.to_owned(), projection.runtime_uid.clone());
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
    /// Digest-pinned bridge image only. Its provenance is verified by the controller process
    /// before this adapter is constructed.
    pub bridge_image: Option<String>,
    /// Server-configured gateway origin passed only to the fixed bridge executable.
    pub bridge_gateway_origin: Option<String>,
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
            self.bridge_image.as_deref(),
            self.bridge_gateway_origin.as_deref(),
        ) {
            (None, None) => {}
            (Some(image), Some(origin)) => {
                if !is_digest_pinned_image(image) {
                    return Err(PortError::Rejected {
                        reason: "Connections bridge image must be an immutable sha256 reference"
                            .to_owned(),
                    });
                }
                validate_connections_bridge_gateway_origin(origin)?;
            }
            (Some(_), None) | (None, Some(_)) => {
                return Err(PortError::Rejected {
                    reason:
                        "Connections bridge image and gateway origin must be configured together"
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
    bridge_image: Option<String>,
    bridge_gateway_origin: Option<String>,
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
            bridge_image: config.bridge_image,
            bridge_gateway_origin: config.bridge_gateway_origin,
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

    async fn verify_raw_sandbox_binding(
        &self,
        workspace: &str,
        sandbox: &str,
        runtime_uid: &str,
        expected_image: Option<&str>,
        require_ready: bool,
    ) -> Result<(), PortError> {
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
        validate_raw_sandbox_binding(&snapshot, runtime_uid, expected_image, require_ready)
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
    async fn ensure(&self, request: &SandboxRequest) -> Result<SandboxObservation, PortError> {
        let projection = project_request(request, self.bridge_image.as_deref())?;
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
        self.verify_raw_sandbox_binding(
            &projection.workspace,
            &projection.sandbox,
            &projection.runtime_uid,
            projection.image.as_deref(),
            false,
        )
        .await?;
        for provider_name in [TOOL_PROVIDER, INFERENCE_PROVIDER] {
            self.reconcile_provider(
                &projection.workspace,
                &projection.sandbox,
                provider_name,
                projection
                    .providers
                    .iter()
                    .any(|provider| provider == provider_name),
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
        let expected_image =
            bridge_image_for_agent_type(&request.agent_type, self.bridge_image.as_deref())?;
        self.verify_raw_sandbox_binding(
            workspace,
            sandbox,
            &request.runtime.0,
            expected_image.as_deref(),
            true,
        )
        .await?;
        let stage = self
            .authenticated_client()
            .await?
            .workspace(workspace)
            .exec(
                sandbox,
                &[
                    "/bin/sh".to_owned(),
                    "-c".to_owned(),
                    "set -eu; rm -rf /sandbox/steward-input /sandbox/steward-output; mkdir -p /sandbox/steward-input /sandbox/steward-output; tar -xf - -C /sandbox/steward-input".to_owned(),
                ],
                ExecOptions {
                    timeout: Some(StdDuration::from_secs(120)),
                    stdin: Some(input_archive.to_vec()),
                    ..ExecOptions::default()
                },
            )
            .await
            .map_err(port_failure)?;
        if stage.exit_code != 0 {
            return Err(PortError::Rejected {
                reason: "task input archive could not be staged".to_owned(),
            });
        }
        let mut environment = HashMap::new();
        environment.insert(
            "STEWARD_OUTPUT_DIR".to_owned(),
            "/sandbox/steward-output".to_owned(),
        );
        if is_connections_bridge {
            let origin =
                self.bridge_gateway_origin
                    .as_deref()
                    .ok_or_else(|| PortError::Rejected {
                        reason: "Connections bridge gateway origin is not configured".to_owned(),
                    })?;
            environment.insert("STEWARD_MCP_GW_ORIGIN".to_owned(), origin.to_owned());
        }
        let executed = self
            .authenticated_client()
            .await?
            .workspace(workspace)
            .exec(
                sandbox,
                &request.command,
                ExecOptions {
                    workdir: Some("/sandbox/steward-input".to_owned()),
                    environment,
                    timeout: Some(StdDuration::from_secs(30 * 60)),
                    ..ExecOptions::default()
                },
            )
            .await
            .map_err(port_failure)?;
        if executed.exit_code != 0 {
            return Err(PortError::Failed {
                reason: format!("task agent exited with code {}", executed.exit_code),
            });
        }
        let output_archive_command = output_archive_command(&request.agent_type);
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
    use std::collections::HashMap;
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
        Sandbox as RawSandbox, SandboxPhase as RawSandboxPhase, SandboxSpec as RawSandboxSpec,
        SandboxStatus as RawSandboxStatus, SandboxTemplate as RawSandboxTemplate,
    };
    #[cfg(feature = "runtime")]
    use reqwest::{Client as HttpClient, Url};
    #[cfg(feature = "runtime")]
    use tokio::sync::Mutex;

    #[cfg(feature = "runtime")]
    use steward_ports::PortError;
    #[cfg(feature = "runtime")]
    use steward_ports::SandboxRequest;
    #[cfg(feature = "runtime")]
    use steward_types::{AgentType, RuntimeId, RuntimeRefs, ToolGrant};

    #[cfg(feature = "runtime")]
    use super::RUNTIME_UID_LABEL;
    #[cfg(feature = "runtime")]
    use super::validate_connections_bridge_archive;
    #[cfg(feature = "runtime")]
    use super::{
        CONNECTIONS_BRIDGE_AGENT_TYPE, OpenShellConnectionConfig, ProviderReconciliation,
        SandboxDeleteClient, WorkloadExchangeTokenProvider, delete_owned_sandbox, deletion_names,
        load_source_credential, output_archive_command, project_request, provider_reconciliation,
        sandbox_spec, validate_raw_sandbox_binding, validate_workload_exchange_endpoint,
    };
    #[cfg(feature = "identity")]
    use super::{IdentityResolutionError, SANDBOX_ID_LABEL, binding_from_sandbox};
    use super::{NameKind, stable_name};

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
            bridge_image: None,
            bridge_gateway_origin: None,
        }
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
                agent_type: AgentType {
                    name: "base".to_owned(),
                },
                models: Vec::new(),
                tools: Vec::new(),
                refs: RuntimeRefs::default(),
            },
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
    fn approved_codex_workflow_uses_the_gateway_default_image_and_unknown_agents_fail_closed()
    -> Result<(), String> {
        let request = |agent_type: &str| SandboxRequest {
            runtime: RuntimeId("runtime-uid-a".to_owned()),
            workspace_key: "team-a".to_owned(),
            agent_type: AgentType {
                name: agent_type.to_owned(),
            },
            models: Vec::new(),
            tools: Vec::new(),
            refs: RuntimeRefs::default(),
        };

        let projection = project_request(&request("codex@0.117.0"), None)
            .map_err(|error| format!("approved Workflow agent was rejected: {error:?}"))?;
        assert!(
            sandbox_spec(&projection).image.is_none(),
            "the approved agent must not let Steward select an OpenShell image"
        );
        assert!(
            project_request(&request("codex@latest"), None).is_err(),
            "an unpinned agent reference must fail closed"
        );
        assert!(
            project_request(&request("other-agent@1"), None).is_err(),
            "an unpublished agent reference must fail closed"
        );
        Ok(())
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn approved_codex_workflow_requires_one_non_empty_standard_result_artifact() {
        let command = output_archive_command(&AgentType {
            name: "codex@0.117.0".to_owned(),
        });
        assert!(
            command.contains("test -s /sandbox/steward-output/result.txt"),
            "the runtime must reject a missing or empty standard result"
        );
        assert!(
            command.ends_with("result.txt"),
            "the runtime must archive only the standard Workflow result"
        );
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn connections_bridge_requires_a_digest_pinned_verified_image() -> Result<(), String> {
        let request = SandboxRequest {
            runtime: RuntimeId("runtime-uid-a".to_owned()),
            workspace_key: "team-a".to_owned(),
            agent_type: AgentType {
                name: CONNECTIONS_BRIDGE_AGENT_TYPE.to_owned(),
            },
            models: Vec::new(),
            tools: Vec::new(),
            refs: RuntimeRefs::default(),
        };
        assert!(
            project_request(&request, None).is_err(),
            "the bridge must not create a sandbox without a verified image"
        );
        assert!(
            project_request(
                &request,
                Some("registry.example.test/steward-bridge:latest")
            )
            .is_err(),
            "the bridge must reject mutable image tags"
        );
        let projection = project_request(
            &request,
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
        Ok(())
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn connections_bridge_configuration_requires_an_exact_server_origin() {
        let mut config = valid_connection_config();
        config.bridge_image = Some(
            "registry.example.test/steward-bridge@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
        );
        assert!(
            matches!(config.validate(), Err(PortError::Rejected { .. })),
            "a bridge image without its controller-owned gateway origin must fail before a sandbox can be created"
        );
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
            },
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
                agent_type: AgentType {
                    name: "base".to_owned(),
                },
                models: vec![steward_types::ModelRef {
                    provider: "openai".to_owned(),
                    model: "priced-model".to_owned(),
                }],
                tools: Vec::new(),
                refs: RuntimeRefs::default(),
            },
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
