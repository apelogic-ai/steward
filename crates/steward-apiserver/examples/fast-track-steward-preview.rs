use std::env;
use std::error::Error;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::http::StatusCode;
use axum::routing::get;
use steward_apiserver::KubeRuntimeRepository;
use steward_apiserver::browser_auth::{
    BrowserAuthService, BrowserSessionBinding, GoogleOidcConfig, browser_auth_router,
};
use steward_apiserver::connections;
use steward_apiserver::fast_track_connections_bridge::{
    FastTrackConnectionsBff, FastTrackIdentityResolver,
};
use steward_apiserver::fast_track_runtime_bootstrap::{
    FastTrackRuntimeBootstrap, protected_router as runtime_bootstrap_router,
};
use steward_apiserver::google_oidc::GoogleOidcProvider;
use steward_types::{CanonicalUserId, OrganizationId};
use tokio::net::TcpListener;

fn required(name: &str) -> Result<String, io::Error> {
    env::var(name).map_err(|_| io::Error::other(format!("{name} is required")))
}

fn ttl() -> Result<Duration, io::Error> {
    required("STEWARD_FAST_TRACK_BRIDGE_TTL_SECONDS")?
        .parse::<u64>()
        .map(Duration::from_secs)
        .map_err(|_| io::Error::other("STEWARD_FAST_TRACK_BRIDGE_TTL_SECONDS must be an integer"))
}

async fn router() -> Result<Router, Box<dyn Error>> {
    let browser_origin = required("STEWARD_BROWSER_ORIGIN")?;
    let organization_id = OrganizationId::parse(required("STEWARD_ORGANIZATION_ID")?)?;
    let google = GoogleOidcConfig::new(
        required("STEWARD_GOOGLE_CLIENT_ID")?,
        browser_origin.clone(),
        format!("{browser_origin}/admin/auth/callback"),
        required("STEWARD_GOOGLE_HOSTED_DOMAIN")?,
        organization_id,
    )?;
    let compatibility_email = required("STEWARD_FAST_TRACK_COMPATIBILITY_EMAIL")?;
    let canonical_user_id =
        CanonicalUserId::parse(required("STEWARD_FAST_TRACK_CANONICAL_USER_ID")?)?;
    let identity_resolver =
        FastTrackIdentityResolver::new(canonical_user_id.clone(), compatibility_email.clone())?;
    let provider =
        GoogleOidcProvider::new(google.clone(), required("STEWARD_GOOGLE_CLIENT_SECRET")?)?;
    let browser_auth =
        BrowserAuthService::google(google, Arc::new(provider), Arc::new(identity_resolver))?;
    let broker = FastTrackConnectionsBff::<BrowserSessionBinding>::new(
        required("STEWARD_FAST_TRACK_BRIDGE_ORIGIN")?,
        canonical_user_id,
        required("STEWARD_FAST_TRACK_COMPATIBILITY_ISSUER")?,
        compatibility_email,
        ttl()?,
    )?;
    let runtime_bootstrap = FastTrackRuntimeBootstrap::new(KubeRuntimeRepository::new(
        kube::Client::try_default().await?,
    ));
    Ok(Router::new()
        .route("/healthz", get(|| async { StatusCode::NO_CONTENT }))
        .merge(browser_auth_router(browser_auth.clone()))
        .merge(connections::protected_router(broker, browser_auth.clone()))
        .merge(runtime_bootstrap_router(runtime_bootstrap, browser_auth)))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let bind = required("STEWARD_FAST_TRACK_PREVIEW_BIND")?
        .parse::<SocketAddr>()
        .map_err(|_| {
            io::Error::other("STEWARD_FAST_TRACK_PREVIEW_BIND must be a socket address")
        })?;
    let listener = TcpListener::bind(bind).await?;
    let address = listener.local_addr()?;
    println!("FAST-TRACK / NON-PROMOTABLE preview listening on {address}/healthz");
    axum::serve(listener, router().await?).await?;
    Ok(())
}
