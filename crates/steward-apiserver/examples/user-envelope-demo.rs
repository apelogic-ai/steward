use std::error::Error;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::str::FromStr;

use steward_apiserver::user_envelopes_demo::router;
use tokio::net::TcpListener;

fn parse_bind<I>(args: I) -> Result<SocketAddr, String>
where
    I: IntoIterator<Item = String>,
{
    let mut arguments = args.into_iter();
    let mut bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    let mut bind_seen = false;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--bind" if !bind_seen => {
                bind = SocketAddr::from_str(
                    &arguments
                        .next()
                        .ok_or_else(|| "--bind requires a value".to_owned())?,
                )
                .map_err(|_| "--bind must be a socket address".to_owned())?;
                bind_seen = true;
            }
            "--bind" => return Err("--bind may be specified only once".to_owned()),
            _ => return Err("unknown localhost demo argument".to_owned()),
        }
    }
    if !bind.ip().is_loopback() {
        return Err("localhost envelope demo bind must be loopback".to_owned());
    }
    Ok(bind)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let bind = parse_bind(std::env::args().skip(1)).map_err(io::Error::other)?;
    let listener = TcpListener::bind(bind).await.map_err(io::Error::other)?;
    let origin = format!(
        "http://{}",
        listener.local_addr().map_err(io::Error::other)?
    );
    println!("Steward envelope localhost demo: {origin}/admin/sign-in");
    axum::serve(listener, router(&origin).map_err(io::Error::other)?)
        .await
        .map_err(io::Error::other)?;
    Ok(())
}
