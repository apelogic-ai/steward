use std::env;
use std::error::Error;
use std::fs;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use axum::serve::Listener;
use sqlx::postgres::PgPoolOptions;
use steward_apiserver::{
    AuthenticatedCaller, AuthenticationError, BoxFuture, KubeRuntimeRepository,
    RequestAuthenticator, router,
};
use steward_controller::webhook_router;
use steward_store::PgStore;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::sleep;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio_rustls::server::TlsStream;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let database_url = required("STEWARD_TEST_DATABASE_URL")?;
    let actor = required("STEWARD_TEST_ACTOR")?;
    let member_role = required("STEWARD_TEST_MEMBER_ROLE")?;
    let admin = required("STEWARD_TEST_ADMIN")?;
    let certificate_path = required("STEWARD_TEST_TLS_CERT_DER")?;
    let private_key_path = required("STEWARD_TEST_TLS_KEY_DER")?;
    let bind = env::var("STEWARD_TEST_HTTP_BIND").unwrap_or_else(|_| "0.0.0.0:8080".to_owned());

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;
    let client = kube::Client::try_default().await?;
    let app = router(
        KubeRuntimeRepository::new(client),
        store.clone(),
        S3Authenticator {
            actor,
            member_role,
            admin,
        },
    )
    .merge(webhook_router(store));
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

#[derive(Clone)]
struct S3Authenticator {
    actor: String,
    member_role: String,
    admin: String,
}

impl RequestAuthenticator for S3Authenticator {
    fn authenticate<'a>(
        &'a self,
        bearer_token: &'a str,
    ) -> BoxFuture<'a, Result<AuthenticatedCaller, AuthenticationError>> {
        Box::pin(async move {
            match bearer_token {
                "test-user-session" => Ok(AuthenticatedCaller {
                    actor: self.actor.clone(),
                    member_roles: vec![self.member_role.clone()],
                    is_admin: false,
                }),
                "test-admin-session" => Ok(AuthenticatedCaller {
                    actor: self.admin.clone(),
                    member_roles: Vec::new(),
                    is_admin: true,
                }),
                _ => Err(AuthenticationError::InvalidCredentials),
            }
        })
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
