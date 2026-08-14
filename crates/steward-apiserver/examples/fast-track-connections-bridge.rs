use std::env;
use std::error::Error;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use steward_apiserver::fast_track_connections_bridge::{
    BRIDGE_HEALTH_PATH, FastTrackBridgeConfig, FastTrackConnectionsBridge,
};
use tokio::net::TcpListener;

fn required(name: &str) -> Result<String, io::Error> {
    env::var(name).map_err(|_| io::Error::other(format!("{name} is required")))
}

fn config() -> Result<(SocketAddr, FastTrackConnectionsBridge), Box<dyn Error>> {
    let bind = required("STEWARD_FAST_TRACK_BRIDGE_BIND")?
        .parse::<SocketAddr>()
        .map_err(|_| io::Error::other("STEWARD_FAST_TRACK_BRIDGE_BIND must be a socket address"))?;
    let ttl = required("STEWARD_FAST_TRACK_BRIDGE_TTL_SECONDS")?
        .parse::<u64>()
        .map(Duration::from_secs)
        .map_err(|_| {
            io::Error::other("STEWARD_FAST_TRACK_BRIDGE_TTL_SECONDS must be an integer")
        })?;
    let config = FastTrackBridgeConfig::new(
        required("STEWARD_FAST_TRACK_MCP_GW_ORIGIN")?,
        required("STEWARD_FAST_TRACK_COMPATIBILITY_ISSUER")?,
        required("STEWARD_FAST_TRACK_COMPATIBILITY_EMAIL")?,
        required("STEWARD_FAST_TRACK_REDIRECT_AFTER")?,
        ttl,
    )?;
    Ok((bind, FastTrackConnectionsBridge::new(config)?))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let (bind, bridge) = config()?;
    let listener = TcpListener::bind(bind).await?;
    let address = listener.local_addr()?;
    println!("FAST-TRACK / NON-PROMOTABLE bridge listening on {address}{BRIDGE_HEALTH_PATH}");
    axum::serve(listener, bridge.router()).await?;
    Ok(())
}
