use std::env;
use std::error::Error;
use std::fs;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use axum::serve::Listener;
use steward_adapter_jira::{JiraAdapter, JiraConfig};
use steward_apiserver::{KubeRuntimeRepository, KubernetesTokenAuthenticator, router};
use steward_store::PgStore;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::sleep;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio_rustls::server::TlsStream;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let client = kube::Client::try_default().await?;
    let store = PgStore::connect(&required("STEWARD_DATABASE_URL")?).await?;
    store.migrate().await?;
    let decisions = JiraAdapter::new(
        JiraConfig {
            base_url: required("STEWARD_JIRA_BASE_URL")?,
            project_key: required("STEWARD_JIRA_PROJECT_KEY")?,
            account_email: required("STEWARD_JIRA_ACCOUNT_EMAIL")?,
        },
        required("STEWARD_JIRA_TOKEN")?,
    )
    .map_err(|error| io::Error::other(format!("Jira configuration failed: {error:?}")))?;
    let authenticator = KubernetesTokenAuthenticator::new(
        client.clone(),
        env::var("STEWARD_ADMIN_GROUP").unwrap_or_else(|_| "agents.apelogic.ai/admin".to_owned()),
        env::var("STEWARD_TOKEN_AUDIENCE").ok(),
    );
    let app = router(
        KubeRuntimeRepository::new(client),
        store,
        authenticator,
        decisions,
    );
    let listener = tls_listener(
        &env::var("STEWARD_APISERVER_BIND").unwrap_or_else(|_| "0.0.0.0:8443".to_owned()),
        &required("STEWARD_TLS_CERT_DER")?,
        &required("STEWARD_TLS_KEY_DER")?,
    )
    .await?;
    axum::serve(listener, app).await?;
    Ok(())
}

fn required(name: &str) -> Result<String, io::Error> {
    env::var(name).map_err(|_| io::Error::other(format!("{name} is required")))
}

async fn tls_listener(
    bind: &str,
    certificate_path: &str,
    private_key_path: &str,
) -> Result<TlsListener, Box<dyn Error>> {
    let certificate = CertificateDer::from(fs::read(certificate_path)?);
    let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(fs::read(private_key_path)?));
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![certificate], private_key)?;
    Ok(TlsListener {
        acceptor: TlsAcceptor::from(Arc::new(config)),
        listener: TcpListener::bind(bind).await?,
    })
}

struct TlsListener {
    acceptor: TlsAcceptor,
    listener: TcpListener,
}

impl Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = std::net::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.listener.accept().await {
                Ok((stream, address)) => match self.acceptor.accept(stream).await {
                    Ok(stream) => return (stream, address),
                    Err(error) => eprintln!("apiserver TLS handshake failed: {error}"),
                },
                Err(error) => {
                    eprintln!("apiserver listener accept failed: {error}");
                    sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}
