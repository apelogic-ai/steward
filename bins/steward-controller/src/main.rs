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
    OpenShellConnectionConfig, OpenShellRuntime, validate_connections_bridge_gateway_origin,
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    install_rustls_crypto_provider()?;
    let client = Client::try_default().await?;
    let sandbox_runtime = OpenShellRuntime::connect(openshell_connection_config()?)
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
    let bridge_image = verified_bridge_image_configuration()?;
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
        bridge_gateway_origin: bridge_gateway_origin_for_image(
            &bridge_image,
            env::var("STEWARD_STABLE_BRIDGE_MCP_GW_ORIGIN").ok(),
        )?,
        bridge_image,
    })
}

fn bridge_gateway_origin_for_image(
    bridge_image: &Option<String>,
    configured_origin: Option<String>,
) -> Result<Option<String>, io::Error> {
    match (bridge_image, configured_origin) {
        (None, None) => Ok(None),
        (Some(_), Some(origin)) => {
            validate_connections_bridge_gateway_origin(&origin).map_err(|error| {
                io::Error::other(format!(
                    "STEWARD_STABLE_BRIDGE_MCP_GW_ORIGIN must be an exact HTTP(S) origin: {error:?}"
                ))
            })?;
            Ok(Some(origin))
        }
        (Some(_), None) => Err(io::Error::other(
            "bridge image provenance requires STEWARD_STABLE_BRIDGE_MCP_GW_ORIGIN",
        )),
        (None, Some(_)) => Err(io::Error::other(
            "STEWARD_STABLE_BRIDGE_MCP_GW_ORIGIN requires bridge image provenance",
        )),
    }
}

/// Reads one all-or-nothing provenance configuration set. The controller never accepts a bridge
/// image directly from an environment value: this function returns it only after offline GitHub
/// provenance verification binds the digest to the configured source and workflow signer.
fn verified_bridge_image_configuration() -> Result<Option<String>, io::Error> {
    let image = env::var("STEWARD_STABLE_BRIDGE_IMAGE").ok();
    let signer_identity = env::var("STEWARD_STABLE_BRIDGE_SIGNER_IDENTITY").ok();
    let source_repository = env::var("STEWARD_STABLE_BRIDGE_SOURCE_REPOSITORY").ok();
    let source_commit = env::var("STEWARD_STABLE_BRIDGE_SOURCE_COMMIT").ok();
    let bundle_file = env::var("STEWARD_STABLE_BRIDGE_ATTESTATION_BUNDLE_FILE").ok();
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
        TlsListener, bridge_gateway_origin_for_image, decode_tls_material,
        install_rustls_crypto_provider, verify_bridge_image_provenance,
    };

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
    fn bridge_gateway_origin_is_paired_with_a_verified_bridge_image() {
        let image = Some(
            "registry.example.test/steward-bridge@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
        );
        assert!(
            bridge_gateway_origin_for_image(&image, None).is_err(),
            "the controller must not construct a bridge-capable runtime without its server-owned gateway origin"
        );
        assert!(
            bridge_gateway_origin_for_image(&None, Some("https://mcp-gw.example.test".to_owned()))
                .is_err(),
            "a gateway origin cannot turn on the bridge without verified image provenance"
        );
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
                bridge_gateway_origin_for_image(&image, Some(invalid_origin.to_owned())).is_err(),
                "controller startup must reject non-exact bridge origin {invalid_origin:?}"
            );
        }
        let accepted_origin = bridge_gateway_origin_for_image(
            &image,
            Some("https://mcp-gw.example.test:8443".to_owned()),
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
