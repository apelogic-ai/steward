use std::env;
use std::error::Error;
use std::fs;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use axum::serve::Listener;
use kube::Client;
use steward_adapter_openshell::OpenShellRuntime;
use steward_store::PgStore;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{sleep, timeout};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio_rustls::server::TlsStream;

#[cfg(not(test))]
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(test)]
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_millis(25);

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let endpoint = required("STEWARD_OPENSHELL_ENDPOINT")?;
    let client = Client::try_default().await?;
    let sandbox_runtime = OpenShellRuntime::connect(endpoint)
        .await
        .map_err(|error| io::Error::other(format!("OpenShell connection failed: {error:?}")))?;
    if env::var("STEWARD_S0_BOOTSTRAP").as_deref() == Ok("1") {
        steward_controller::run_controller(client, sandbox_runtime).await;
        return Ok(());
    }

    let database_url = required("STEWARD_DATABASE_URL")?;
    let store = PgStore::connect(&database_url).await?;
    store.migrate().await?;
    let listener = tls_listener(
        &env::var("STEWARD_WEBHOOK_BIND").unwrap_or_else(|_| "0.0.0.0:8443".to_owned()),
        &required("STEWARD_TLS_CERT_DER")?,
        &required("STEWARD_TLS_KEY_DER")?,
    )
    .await?;
    let webhook = axum::serve(
        listener,
        steward_controller::webhook_router_for_controller(
            store.clone(),
            required("STEWARD_CONTROLLER_USERNAME")?,
        ),
    );
    let controller = steward_controller::run_controller_with_store(client, sandbox_runtime, store);
    tokio::select! {
        result = webhook => result?,
        () = controller => return Err(io::Error::other("controller exited").into()),
    }
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
                Ok((stream, address)) => {
                    match timeout(TLS_HANDSHAKE_TIMEOUT, self.acceptor.accept(stream)).await {
                        Ok(Ok(stream)) => return (stream, address),
                        Ok(Err(error)) => eprintln!("webhook TLS handshake failed: {error}"),
                        Err(_) => eprintln!("webhook TLS handshake timed out"),
                    }
                }
                Err(error) => {
                    eprintln!("webhook listener accept failed: {error}");
                    sleep(Duration::from_secs(1)).await;
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

    use super::TlsListener;

    #[tokio::test]
    async fn incomplete_tls_handshake_is_bounded() -> Result<(), String> {
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
            listener: tcp,
        };
        let task = tokio::spawn(async move { listener.accept().await });
        let mut stalled = TcpStream::connect(address)
            .await
            .map_err(|error| format!("connect stalled client: {error}"))?;
        let mut byte = [0_u8; 1];
        let closed = timeout(Duration::from_millis(100), stalled.read(&mut byte)).await;
        task.abort();
        assert!(
            matches!(closed, Ok(Ok(0))),
            "an incomplete TLS handshake must be closed promptly"
        );
        Ok(())
    }
}
