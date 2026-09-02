use std::env;
use std::error::Error;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::serve::Listener;
use kube::Client;
use steward_adapter_github_artifact::GitHubArtifactVerifier;
use steward_adapter_litellm::{LiteLlmAdapter, LiteLlmConfig};
use steward_adapter_openshell::{
    OpenShellConnectionConfig, OpenShellRuntime, OpenShellTaskLogMode,
    validate_connections_bridge_gateway_origin,
};
use steward_store::PgStore;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{JoinError, JoinSet};
use tokio::time::{sleep, timeout};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio_rustls::server::TlsStream;

#[cfg(not(test))]
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(test)]
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_millis(25);
const MAX_PENDING_TLS_HANDSHAKES: usize = 64;
const GITHUB_ATTESTATION_TRUST_MODE: &str = "github-attestation";
const OPERATOR_PINNED_TRUST_MODE: &str = "operator-pinned";

#[derive(Clone, Debug, Eq, PartialEq)]
struct VerifiedConnectionsBridgeArtifact {
    image_reference: String,
    trust_mode: String,
    digest: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    install_rustls_crypto_provider()?;
    let openshell_config = openshell_connection_config()?;
    let client = Client::try_default().await?;
    let sandbox_runtime = OpenShellRuntime::connect(openshell_config)
        .await
        .map_err(|error| io::Error::other(format!("OpenShell connection failed: {error:?}")))?;
    if env::var("STEWARD_S0_BOOTSTRAP").as_deref() == Ok("1") {
        steward_controller::run_controller(client, sandbox_runtime).await;
        return Ok(());
    }

    let database_url = required("STEWARD_DATABASE_URL")?;
    let store = PgStore::connect(&database_url).await?;
    store.migrate().await?;
    let inference = LiteLlmAdapter::new(LiteLlmConfig {
        base_url: required("STEWARD_LITELLM_URL")?,
        master_key: required("STEWARD_LITELLM_MASTER_KEY")?,
    })
    .map_err(|error| {
        io::Error::other(format!("inference plane configuration failed: {error:?}"))
    })?;
    let listener = tls_listener(
        &env::var("STEWARD_WEBHOOK_BIND").unwrap_or_else(|_| "0.0.0.0:8443".to_owned()),
        &required("STEWARD_TLS_CERT_DER")?,
        &required("STEWARD_TLS_KEY_DER")?,
    )
    .await?;
    let webhook = axum::serve(
        listener,
        steward_controller::webhook_router_for_controller_with_catalog(
            store.clone(),
            inference.clone(),
            required("STEWARD_CONTROLLER_USERNAME")?,
            required("STEWARD_APISERVER_USERNAME")?,
        ),
    );
    let controller =
        steward_controller::run_controller_with_planes(client, sandbox_runtime, inference, store);
    tokio::select! {
        result = webhook => result?,
        () = controller => return Err(io::Error::other("controller exited").into()),
    }
    Ok(())
}

fn install_rustls_crypto_provider() -> Result<(), io::Error> {
    use tokio_rustls::rustls::crypto::{CryptoProvider, ring};

    if CryptoProvider::get_default().is_none() {
        let _ = ring::default_provider().install_default();
    }
    if CryptoProvider::get_default().is_some() {
        Ok(())
    } else {
        Err(io::Error::other(
            "install the Steward Rustls crypto provider",
        ))
    }
}

fn required(name: &str) -> Result<String, io::Error> {
    env::var(name).map_err(|_| io::Error::other(format!("{name} is required")))
}

fn required_file(name: &str) -> Result<Vec<u8>, io::Error> {
    let path = required(name)?;
    fs::read(&path).map_err(|error| io::Error::other(format!("failed to read {name}: {error}")))
}

fn openshell_connection_config() -> Result<OpenShellConnectionConfig, io::Error> {
    let task_log_mode = openshell_task_log_mode(env::var("STEWARD_OPENSHELL_TASK_LOG_MODE"))?;
    let stable_bridge_image = verified_stable_bridge_image_configuration()?;
    let bridge_artifact = verified_connections_bridge_image_configuration()?;
    if let Some(artifact) = bridge_artifact.as_ref() {
        eprintln!("{}", connections_bridge_startup_log(artifact));
    }
    let bridge_image = bridge_artifact
        .as_ref()
        .map(|artifact| artifact.image_reference.clone());
    let bridge_artifact_trust_mode = bridge_artifact
        .as_ref()
        .map(|artifact| artifact.trust_mode.clone());
    Ok(OpenShellConnectionConfig {
        endpoint: required("STEWARD_OPENSHELL_ENDPOINT")?,
        ca_certificate_pem: required_file("STEWARD_OPENSHELL_CA_CERTIFICATE_FILE")?,
        client_certificate_pem: required_file("STEWARD_OPENSHELL_CLIENT_CERTIFICATE_FILE")?,
        client_private_key_pem: required_file("STEWARD_OPENSHELL_CLIENT_PRIVATE_KEY_FILE")?,
        workload_exchange_endpoint: required("STEWARD_WORKLOAD_EXCHANGE_ENDPOINT")?,
        workload_exchange_server_name: required("STEWARD_WORKLOAD_EXCHANGE_SERVER_NAME")?,
        workload_exchange_ca_certificate_pem: required_file(
            "STEWARD_WORKLOAD_EXCHANGE_CA_CERTIFICATE_FILE",
        )?,
        workload_source_credential_file: PathBuf::from(required(
            "STEWARD_WORKLOAD_SOURCE_CREDENTIAL_FILE",
        )?),
        server_name: required("STEWARD_OPENSHELL_SERVER_NAME")?,
        runtime_class_name: required("STEWARD_OPENSHELL_RUNTIME_CLASS_NAME")?,
        task_log_mode,
        stable_bridge_gateway_origin: bridge_gateway_origin_for_image(
            &stable_bridge_image,
            env::var("STEWARD_STABLE_BRIDGE_MCP_GW_ORIGIN").ok(),
            "STEWARD_STABLE_BRIDGE_MCP_GW_ORIGIN",
        )?,
        stable_bridge_image,
        bridge_gateway_origin: bridge_gateway_origin_for_image(
            &bridge_image,
            env::var("STEWARD_CONNECTIONS_MCP_GW_ORIGIN").ok(),
            "STEWARD_CONNECTIONS_MCP_GW_ORIGIN",
        )?,
        bridge_gateway_version: bridge_gateway_version_for_image(
            &bridge_image,
            env::var("STEWARD_CONNECTIONS_MCP_GW_VERSION").ok(),
        )?,
        bridge_runtime_namespace: bridge_image
            .as_ref()
            .map(|_| required("STEWARD_CONNECTIONS_RUNTIME_NAMESPACE"))
            .transpose()?,
        bridge_image,
        bridge_artifact_trust_mode,
    })
}

fn connections_bridge_startup_log(artifact: &VerifiedConnectionsBridgeArtifact) -> String {
    format!(
        "connections bridge artifact: trust_mode={} digest={}",
        artifact.trust_mode, artifact.digest
    )
}

fn openshell_task_log_mode(
    value: Result<String, env::VarError>,
) -> Result<OpenShellTaskLogMode, io::Error> {
    match value {
        Err(env::VarError::NotPresent) => Ok(OpenShellTaskLogMode::Off),
        Ok(ref value) if value == "off" => Ok(OpenShellTaskLogMode::Off),
        Ok(ref value) if value == "full" => Ok(OpenShellTaskLogMode::Full),
        Ok(value) => Err(io::Error::other(format!(
            "STEWARD_OPENSHELL_TASK_LOG_MODE must be off or full, got {value:?}"
        ))),
        Err(env::VarError::NotUnicode(_)) => Err(io::Error::other(
            "STEWARD_OPENSHELL_TASK_LOG_MODE must be Unicode off or full",
        )),
    }
}

fn bridge_gateway_origin_for_image(
    bridge_image: &Option<String>,
    configured_origin: Option<String>,
    variable_name: &str,
) -> Result<Option<String>, io::Error> {
    match (bridge_image, configured_origin) {
        (None, None) => Ok(None),
        (Some(_), Some(origin)) => {
            validate_connections_bridge_gateway_origin(&origin).map_err(|error| {
                io::Error::other(format!(
                    "{variable_name} must be an exact HTTP(S) origin: {error:?}"
                ))
            })?;
            Ok(Some(origin))
        }
        (Some(_), None) => Err(io::Error::other(format!(
            "bridge image provenance requires {variable_name}"
        ))),
        (None, Some(_)) => Err(io::Error::other(format!(
            "{variable_name} requires bridge image provenance"
        ))),
    }
}

fn bridge_gateway_version_for_image(
    bridge_image: &Option<String>,
    configured_version: Option<String>,
) -> Result<Option<String>, io::Error> {
    match (bridge_image, configured_version) {
        (None, None) => Ok(None),
        (Some(_), Some(version)) if version == "0.3.2" => Ok(Some(version)),
        (Some(_), Some(_)) => Err(io::Error::other(
            "STEWARD_CONNECTIONS_MCP_GW_VERSION must be the authority-pinned 0.3.2 release",
        )),
        (Some(_), None) => Err(io::Error::other(
            "bridge image provenance requires STEWARD_CONNECTIONS_MCP_GW_VERSION",
        )),
        (None, Some(_)) => Err(io::Error::other(
            "STEWARD_CONNECTIONS_MCP_GW_VERSION requires bridge image provenance",
        )),
    }
}

/// Reads one all-or-nothing artifact trust configuration. GitHub-attestation mode retains offline
/// provenance verification; operator-pinned mode accepts only an explicit canonical digest and
/// delegates provenance responsibility to the deployment system.
fn verified_connections_bridge_image_configuration()
-> Result<Option<VerifiedConnectionsBridgeArtifact>, io::Error> {
    let trust_mode = env::var("STEWARD_CONNECTIONS_BRIDGE_ARTIFACT_TRUST_MODE").ok();
    let image = env::var("STEWARD_CONNECTIONS_BRIDGE_IMAGE").ok();
    let signer_identity = env::var("STEWARD_CONNECTIONS_BRIDGE_SIGNER_IDENTITY").ok();
    let source_repository = env::var("STEWARD_CONNECTIONS_BRIDGE_SOURCE_REPOSITORY").ok();
    let source_commit = env::var("STEWARD_CONNECTIONS_BRIDGE_SOURCE_COMMIT").ok();
    let bundle_file = env::var("STEWARD_CONNECTIONS_BRIDGE_ATTESTATION_BUNDLE_FILE").ok();
    verify_connections_bridge_image_configuration(
        trust_mode,
        image,
        signer_identity,
        source_repository,
        source_commit,
        bundle_file,
    )
}

fn verify_connections_bridge_image_configuration(
    trust_mode: Option<String>,
    image: Option<String>,
    signer_identity: Option<String>,
    source_repository: Option<String>,
    source_commit: Option<String>,
    bundle_file: Option<String>,
) -> Result<Option<VerifiedConnectionsBridgeArtifact>, io::Error> {
    let configured = [
        image.as_ref(),
        signer_identity.as_ref(),
        source_repository.as_ref(),
        source_commit.as_ref(),
        bundle_file.as_ref(),
    ]
    .iter()
    .any(|value| value.is_some());
    if !configured {
        return match trust_mode {
            None => Ok(None),
            Some(_) => Err(io::Error::other(
                "connections bridge trust mode requires a complete bridge configuration",
            )),
        };
    }
    let normalized_mode = trust_mode.unwrap_or_else(|| GITHUB_ATTESTATION_TRUST_MODE.to_owned());
    match normalized_mode.as_str() {
        GITHUB_ATTESTATION_TRUST_MODE => verified_bridge_image_configuration(
            image,
            signer_identity,
            source_repository,
            source_commit,
            bundle_file,
        )
        .map(|verified| {
            verified.map(|image_reference| VerifiedConnectionsBridgeArtifact {
                digest: image_digest(&image_reference).to_owned(),
                image_reference,
                trust_mode: GITHUB_ATTESTATION_TRUST_MODE.to_owned(),
            })
        }),
        OPERATOR_PINNED_TRUST_MODE => {
            if [
                signer_identity.as_deref(),
                source_repository.as_deref(),
                source_commit.as_deref(),
                bundle_file.as_deref(),
            ]
            .iter()
            .flatten()
            .any(|value| !value.is_empty())
            {
                return Err(io::Error::other(
                    "operator-pinned connections bridge cannot include attestation configuration",
                ));
            }
            let image_reference = image.ok_or_else(|| {
                io::Error::other("operator-pinned connections bridge image is required")
            })?;
            if !valid_operator_pinned_image(&image_reference) {
                return Err(io::Error::other(
                    "operator-pinned connections bridge image must be an exact canonical sha256 OCI reference",
                ));
            }
            Ok(Some(VerifiedConnectionsBridgeArtifact {
                digest: image_digest(&image_reference).to_owned(),
                image_reference,
                trust_mode: OPERATOR_PINNED_TRUST_MODE.to_owned(),
            }))
        }
        _ => Err(io::Error::other(
            "STEWARD_CONNECTIONS_BRIDGE_ARTIFACT_TRUST_MODE must be github-attestation or operator-pinned",
        )),
    }
}

fn image_digest(image_reference: &str) -> &str {
    image_reference
        .rsplit_once('@')
        .map_or("invalid", |(_, digest)| digest)
}

fn valid_operator_pinned_image(value: &str) -> bool {
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

fn verified_stable_bridge_image_configuration() -> Result<Option<String>, io::Error> {
    verified_bridge_image_configuration(
        env::var("STEWARD_STABLE_BRIDGE_IMAGE").ok(),
        env::var("STEWARD_STABLE_BRIDGE_SIGNER_IDENTITY").ok(),
        env::var("STEWARD_STABLE_BRIDGE_SOURCE_REPOSITORY").ok(),
        env::var("STEWARD_STABLE_BRIDGE_SOURCE_COMMIT").ok(),
        env::var("STEWARD_STABLE_BRIDGE_ATTESTATION_BUNDLE_FILE").ok(),
    )
}

fn verified_bridge_image_configuration(
    image: Option<String>,
    signer_identity: Option<String>,
    source_repository: Option<String>,
    source_commit: Option<String>,
    bundle_file: Option<String>,
) -> Result<Option<String>, io::Error> {
    let values = [
        image.as_ref(),
        signer_identity.as_ref(),
        source_repository.as_ref(),
        source_commit.as_ref(),
        bundle_file.as_ref(),
    ];
    if values.iter().all(Option::is_none) {
        return Ok(None);
    }
    if values.iter().any(Option::is_none) {
        return Err(io::Error::other(
            "bridge image provenance requires image, signer identity, source repository, source commit, and attestation bundle together",
        ));
    }
    let bundle = fs::read_to_string(
        bundle_file
            .as_deref()
            .ok_or_else(|| io::Error::other("bridge image provenance bundle path is required"))?,
    )
    .map_err(|_| io::Error::other("read bridge image provenance bundle"))?;
    verify_bridge_image_provenance(
        image,
        signer_identity,
        source_repository,
        source_commit,
        Some(bundle),
    )
}

fn verify_bridge_image_provenance(
    image: Option<String>,
    signer_identity: Option<String>,
    source_repository: Option<String>,
    source_commit: Option<String>,
    bundle: Option<String>,
) -> Result<Option<String>, io::Error> {
    let values = [
        image.as_ref(),
        signer_identity.as_ref(),
        source_repository.as_ref(),
        source_commit.as_ref(),
        bundle.as_ref(),
    ];
    if values.iter().all(Option::is_none) {
        return Ok(None);
    }
    if values.iter().any(Option::is_none) {
        return Err(io::Error::other(
            "bridge image provenance requires image, signer identity, source repository, source commit, and attestation bundle together",
        ));
    }
    GitHubArtifactVerifier::from_jsonl(
        image.ok_or_else(|| io::Error::other("bridge image is required"))?,
        signer_identity.ok_or_else(|| io::Error::other("bridge signer identity is required"))?,
        source_repository
            .ok_or_else(|| io::Error::other("bridge source repository is required"))?,
        source_commit.ok_or_else(|| io::Error::other("bridge source commit is required"))?,
        &bundle.ok_or_else(|| io::Error::other("bridge image provenance bundle is required"))?,
    )
    .map_err(|_| io::Error::other("bridge image provenance configuration is invalid"))?
    .verify()
    .map(|artifact| Some(artifact.image_reference().to_owned()))
    .map_err(|_| io::Error::other("bridge image provenance is unverified"))
}

async fn tls_listener(
    bind: &str,
    certificate_path: &str,
    private_key_path: &str,
) -> Result<TlsListener, Box<dyn Error>> {
    let (certificates, private_key) =
        decode_tls_material(fs::read(certificate_path)?, fs::read(private_key_path)?)?;
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificates, private_key)?;
    Ok(TlsListener {
        acceptor: TlsAcceptor::from(Arc::new(config)),
        handshakes: JoinSet::new(),
        listener: TcpListener::bind(bind).await?,
    })
}

fn decode_tls_material(
    certificate_bytes: Vec<u8>,
    private_key_bytes: Vec<u8>,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), Box<dyn Error>> {
    let certificates = if certificate_bytes.starts_with(b"-----BEGIN") {
        CertificateDer::pem_slice_iter(&certificate_bytes).collect::<Result<Vec<_>, _>>()?
    } else {
        vec![CertificateDer::from(certificate_bytes)]
    };
    if certificates.is_empty() {
        return Err(io::Error::other("TLS certificate file contains no certificates").into());
    }
    let private_key = if private_key_bytes.starts_with(b"-----BEGIN") {
        PrivateKeyDer::from_pem_slice(&private_key_bytes)?
    } else {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(private_key_bytes))
    };
    Ok((certificates, private_key))
}

struct TlsListener {
    acceptor: TlsAcceptor,
    handshakes: JoinSet<Option<TlsConnection>>,
    listener: TcpListener,
}

type TlsConnection = (TlsStream<TcpStream>, std::net::SocketAddr);
type JoinedHandshake = Option<Result<Option<TlsConnection>, JoinError>>;

fn completed_handshake(result: JoinedHandshake) -> Option<TlsConnection> {
    match result {
        Some(Ok(connection)) => connection,
        Some(Err(error)) => {
            eprintln!("webhook TLS handshake task failed: {error}");
            None
        }
        None => None,
    }
}

impl Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = std::net::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            if self.handshakes.len() >= MAX_PENDING_TLS_HANDSHAKES {
                if let Some(connection) = completed_handshake(self.handshakes.join_next().await) {
                    return connection;
                }
                continue;
            }
            let has_pending_handshakes = !self.handshakes.is_empty();
            tokio::select! {
                accepted = self.listener.accept() => match accepted {
                    Ok((stream, address)) => {
                        let acceptor = self.acceptor.clone();
                        self.handshakes.spawn(async move {
                            match timeout(TLS_HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
                                Ok(Ok(stream)) => Some((stream, address)),
                                Ok(Err(error)) => {
                                    eprintln!("webhook TLS handshake failed: {error}");
                                    None
                                }
                                Err(_) => {
                                    eprintln!("webhook TLS handshake timed out");
                                    None
                                }
                            }
                        });
                    }
                    Err(error) => {
                        eprintln!("webhook listener accept failed: {error}");
                        sleep(Duration::from_secs(1)).await;
                    }
                },
                completed = self.handshakes.join_next(), if has_pending_handshakes => {
                    if let Some(connection) = completed_handshake(completed) {
                        return connection;
                    }
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use axum::serve::Listener;
    use tokio::io::AsyncReadExt;
    use tokio::net::{TcpListener, TcpStream};
    use tokio::time::timeout;
    use tokio_rustls::TlsAcceptor;
    use tokio_rustls::rustls::ServerConfig;
    use tokio_rustls::rustls::server::ResolvesServerCertUsingSni;

    use super::{
        GITHUB_ATTESTATION_TRUST_MODE, OPERATOR_PINNED_TRUST_MODE, TlsListener,
        bridge_gateway_origin_for_image, bridge_gateway_version_for_image,
        connections_bridge_startup_log, decode_tls_material, install_rustls_crypto_provider,
        openshell_task_log_mode, verify_bridge_image_provenance,
        verify_connections_bridge_image_configuration,
    };
    use steward_adapter_openshell::OpenShellTaskLogMode;

    #[test]
    fn task_log_mode_accepts_unset_off_and_full() -> Result<(), String> {
        assert_eq!(
            openshell_task_log_mode(Err(std::env::VarError::NotPresent))
                .map_err(|error| error.to_string())?,
            OpenShellTaskLogMode::Off
        );
        assert_eq!(
            openshell_task_log_mode(Ok("off".to_owned())).map_err(|error| error.to_string())?,
            OpenShellTaskLogMode::Off
        );
        assert_eq!(
            openshell_task_log_mode(Ok("full".to_owned())).map_err(|error| error.to_string())?,
            OpenShellTaskLogMode::Full
        );
        Ok(())
    }

    #[test]
    fn task_log_mode_rejects_every_other_value_with_the_setting_name() -> Result<(), String> {
        for invalid in ["", "OFF", "verbose", "full "] {
            let error = openshell_task_log_mode(Ok(invalid.to_owned()))
                .err()
                .ok_or_else(|| format!("unsupported task log mode {invalid:?} was accepted"))?;
            assert!(
                error
                    .to_string()
                    .contains("STEWARD_OPENSHELL_TASK_LOG_MODE")
                    && error.to_string().contains("off or full"),
                "the startup error must identify the setting and its accepted values"
            );
        }
        let non_unicode = openshell_task_log_mode(Err(std::env::VarError::NotUnicode(
            std::ffi::OsString::from("not-a-mode"),
        )))
        .err()
        .ok_or_else(|| "a non-Unicode task log mode was accepted".to_owned())?;
        assert!(
            non_unicode
                .to_string()
                .contains("STEWARD_OPENSHELL_TASK_LOG_MODE"),
            "a non-Unicode startup error must identify the setting"
        );
        Ok(())
    }

    #[test]
    fn bridge_image_configuration_is_all_or_nothing_and_fails_closed() -> Result<(), String> {
        assert_eq!(
            verify_bridge_image_provenance(None, None, None, None, None)
                .map_err(|error| error.to_string())?,
            None
        );
        assert!(
            verify_bridge_image_provenance(
                Some("registry.example.test/steward-bridge@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned()),
                None,
                None,
                None,
                None,
            )
            .is_err(),
            "a bare image reference cannot bypass provenance verification"
        );
        assert!(
            verify_bridge_image_provenance(
                Some("registry.example.test/steward-bridge@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned()),
                Some("https://github.com/example-org/steward/.github/workflows/release.yml@refs/tags/v0.1.0".to_owned()),
                Some("https://github.com/example-org/steward".to_owned()),
                Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned()),
                Some("not-a-signed-bundle".to_owned()),
            )
            .is_err(),
            "an unverifiable configured bridge is rejected before OpenShell is contacted"
        );
        Ok(())
    }

    #[test]
    fn connections_bridge_operator_pinned_mode_is_explicit_strict_and_unattested()
    -> Result<(), String> {
        let digest = "a".repeat(64);
        let image = format!("registry.example.test:5000/team/bridge@sha256:{digest}");
        let verified = verify_connections_bridge_image_configuration(
            Some(OPERATOR_PINNED_TRUST_MODE.to_owned()),
            Some(image.clone()),
            None,
            None,
            None,
            None,
        )
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "operator-pinned configuration was treated as disabled".to_owned())?;
        assert_eq!(verified.image_reference, image);
        assert_eq!(verified.trust_mode, OPERATOR_PINNED_TRUST_MODE);
        assert_eq!(verified.digest, format!("sha256:{digest}"));

        for invalid in [
            "registry.example.test/team/bridge:latest".to_owned(),
            format!("registry.example.test/team/bridge:tag@sha256:{digest}"),
            format!("registry.example.test/team/bridge@@sha256:{digest}"),
            format!("registry.example.test//team/bridge@sha256:{digest}"),
            format!("https://registry.example.test/team/bridge@sha256:{digest}"),
            format!("registry.example.test/team/bridge @sha256:{digest}"),
            format!(
                "registry.example.test/team/bridge@sha256:{}",
                "A".repeat(64)
            ),
        ] {
            assert!(
                verify_connections_bridge_image_configuration(
                    Some(OPERATOR_PINNED_TRUST_MODE.to_owned()),
                    Some(invalid.clone()),
                    None,
                    None,
                    None,
                    None,
                )
                .is_err(),
                "operator-pinned startup accepted {invalid:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn connections_bridge_trust_mode_defaults_to_github_and_rejects_contradictions() {
        let image = Some(
            "registry.example.test/team/bridge@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .to_owned(),
        );
        let default_result = verify_connections_bridge_image_configuration(
            None,
            image.clone(),
            None,
            None,
            None,
            None,
        );
        assert!(matches!(
            default_result,
            Err(ref error) if error.to_string().contains("provenance")
        ));

        assert!(
            verify_connections_bridge_image_configuration(
                Some(OPERATOR_PINNED_TRUST_MODE.to_owned()),
                image.clone(),
                Some("configured-signer".to_owned()),
                None,
                None,
                None,
            )
            .is_err(),
            "operator trust must reject even partial attestation configuration"
        );
        assert!(
            verify_connections_bridge_image_configuration(
                Some("unknown".to_owned()),
                image,
                None,
                None,
                None,
                None,
            )
            .is_err()
        );
        assert_eq!(GITHUB_ATTESTATION_TRUST_MODE, "github-attestation");
    }

    #[test]
    fn connections_bridge_startup_log_contains_only_mode_and_digest() -> Result<(), String> {
        let image = "private-registry.example.test/team/bridge@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let artifact = verify_connections_bridge_image_configuration(
            Some(OPERATOR_PINNED_TRUST_MODE.to_owned()),
            Some(image.to_owned()),
            None,
            None,
            None,
            None,
        )
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "operator artifact is missing".to_owned())?;
        let log = connections_bridge_startup_log(&artifact);
        assert!(log.contains("trust_mode=operator-pinned"));
        assert!(log.contains("digest=sha256:"));
        assert!(!log.contains("private-registry") && !log.contains("team/bridge"));
        Ok(())
    }

    #[test]
    fn bridge_gateway_origin_is_paired_with_a_verified_bridge_image() {
        let image = Some(
            "registry.example.test/steward-bridge@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
        );
        assert!(
            bridge_gateway_origin_for_image(&image, None, "STEWARD_CONNECTIONS_MCP_GW_ORIGIN",)
                .is_err(),
            "the controller must not construct a bridge-capable runtime without its server-owned gateway origin"
        );
        assert!(
            bridge_gateway_origin_for_image(
                &None,
                Some("https://mcp-gw.example.test".to_owned()),
                "STEWARD_CONNECTIONS_MCP_GW_ORIGIN",
            )
            .is_err(),
            "a gateway origin cannot turn on the bridge without verified image provenance"
        );
    }

    #[test]
    fn bridge_gateway_version_is_paired_and_exactly_pinned() {
        let image = Some(
            "registry.example.test/steward-bridge@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
        );
        assert!(
            matches!(
                bridge_gateway_version_for_image(&image, Some("0.3.2".to_owned())),
                Ok(Some(ref version)) if version == "0.3.2"
            ),
            "the exact authority-pinned gateway version must be retained"
        );
        assert!(
            bridge_gateway_version_for_image(&image, Some("0.3.1".to_owned())).is_err(),
            "an incompatible deployed OAuth contract must fail controller startup"
        );
        assert!(bridge_gateway_version_for_image(&image, None).is_err());
        assert!(bridge_gateway_version_for_image(&None, Some("0.3.2".to_owned())).is_err());
    }

    #[test]
    fn bridge_gateway_origin_is_rejected_before_controller_startup_when_not_exact() {
        let image = Some(
            "registry.example.test/steward-bridge@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
        );
        for invalid_origin in [
            " https://mcp-gw.example.test",
            "https://bridge-user@mcp-gw.example.test",
            "https://mcp-gw.example.test:70000",
            "https://mcp-gw.example.test:0",
            "https://mcp-gw.example.test/path",
            "https://mcp-gw.example.test?target=other",
            "https://mcp-gw.example.test#fragment",
        ] {
            assert!(
                bridge_gateway_origin_for_image(
                    &image,
                    Some(invalid_origin.to_owned()),
                    "STEWARD_CONNECTIONS_MCP_GW_ORIGIN",
                )
                .is_err(),
                "controller startup must reject non-exact bridge origin {invalid_origin:?}"
            );
        }
        let accepted_origin = bridge_gateway_origin_for_image(
            &image,
            Some("https://mcp-gw.example.test:8443".to_owned()),
            "STEWARD_CONNECTIONS_MCP_GW_ORIGIN",
        );
        assert!(
            matches!(accepted_origin, Ok(Some(ref origin)) if origin == "https://mcp-gw.example.test:8443"),
            "an exact HTTPS origin must be accepted and retained before controller startup"
        );
    }

    #[test]
    fn cert_manager_pem_tls_material_is_decoded() -> Result<(), String> {
        let private_key_pem = [
            b"-----BEGIN ".as_slice(),
            b"PRIVATE KEY-----\nBAUG\n-----END PRIVATE KEY-----\n".as_slice(),
        ]
        .concat();
        let (certificates, private_key) = decode_tls_material(
            b"-----BEGIN CERTIFICATE-----\nAQID\n-----END CERTIFICATE-----\n".to_vec(),
            private_key_pem,
        )
        .map_err(|error| error.to_string())?;
        assert_eq!(certificates.len(), 1);
        assert_eq!(certificates[0].as_ref(), &[1, 2, 3]);
        assert_eq!(private_key.secret_der(), &[4, 5, 6]);
        Ok(())
    }

    #[tokio::test]
    async fn stalled_tls_handshakes_do_not_serialize_acceptance() -> Result<(), String> {
        install_rustls_crypto_provider().map_err(|error| error.to_string())?;
        let tcp = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|error| format!("bind test listener: {error}"))?;
        let address = tcp
            .local_addr()
            .map_err(|error| format!("read test listener address: {error}"))?;
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(ResolvesServerCertUsingSni::new()));
        let mut listener = TlsListener {
            acceptor: TlsAcceptor::from(Arc::new(config)),
            handshakes: tokio::task::JoinSet::new(),
            listener: tcp,
        };
        let task = tokio::spawn(async move { listener.accept().await });
        let mut stalled = Vec::new();
        for _ in 0..6 {
            stalled.push(
                TcpStream::connect(address)
                    .await
                    .map_err(|error| format!("connect stalled client: {error}"))?,
            );
        }
        let closed = timeout(Duration::from_millis(100), async {
            for stream in &mut stalled {
                let mut byte = [0_u8; 1];
                let read = stream
                    .read(&mut byte)
                    .await
                    .map_err(|error| format!("read stalled client: {error}"))?;
                if read != 0 {
                    return Err("stalled TLS client received unexpected bytes".to_owned());
                }
            }
            Ok::<(), String>(())
        })
        .await;
        task.abort();
        assert!(
            matches!(closed, Ok(Ok(()))),
            "stalled TLS handshakes must time out concurrently"
        );
        Ok(())
    }
}
