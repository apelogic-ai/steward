use std::env;
use std::error::Error;
use std::fs;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use axum::serve::Listener;
use sqlx::postgres::PgPoolOptions;
use steward_controller::webhook_router_for_controller;
use steward_store::PgStore;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::sleep;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio_rustls::server::TlsStream;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    install_rustls_crypto_provider()?;
    let database_url = required("STEWARD_TEST_DATABASE_URL")?;
    let controller_username = required("STEWARD_TEST_CONTROLLER_USERNAME")?;
    let certificate_path = required("STEWARD_TEST_TLS_CERT_DER")?;
    let private_key_path = required("STEWARD_TEST_TLS_KEY_DER")?;
    let bind = env::var("STEWARD_TEST_HTTP_BIND").unwrap_or_else(|_| "0.0.0.0:8443".to_owned());

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;
    let app = webhook_router_for_controller(store, controller_username);
    let listener = TcpListener::bind(&bind).await?;
    let certificate = CertificateDer::from(fs::read(certificate_path)?);
    let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(fs::read(private_key_path)?));
    let tls = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![certificate], private_key)?;
    axum::serve(
        TlsListener {
            acceptor: TlsAcceptor::from(Arc::new(tls)),
            listener,
        },
        app,
    )
    .await?;
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
        Err(io::Error::other("Rustls crypto provider is unavailable"))
    }
}

fn required(name: &str) -> Result<String, io::Error> {
    env::var(name).map_err(|_| io::Error::other(format!("{name} is required")))
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
                    Err(error) => eprintln!("test TLS handshake failed: {error}"),
                },
                Err(error) => {
                    eprintln!("test TLS listener accept failed: {error}");
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
    use super::{ServerConfig, install_rustls_crypto_provider};

    #[test]
    fn tls_server_configuration_selects_a_crypto_provider() -> Result<(), String> {
        install_rustls_crypto_provider().map_err(|error| error.to_string())?;
        let _ = ServerConfig::builder();
        Ok(())
    }
}
