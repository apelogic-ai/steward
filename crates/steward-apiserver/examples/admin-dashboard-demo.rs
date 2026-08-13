use std::error::Error;
use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::str::FromStr;

use steward_apiserver::admin_demo::{AdminDashboardDemoConfig, AdminDashboardDemoMode, router};
use tokio::net::TcpListener;

fn parse_args<I>(args: I) -> Result<AdminDashboardDemoConfig, String>
where
    I: IntoIterator<Item = String>,
{
    let mut arguments = args.into_iter();
    let mut bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    let mut bind_seen = false;
    let mut mode = None;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--mode" if mode.is_none() => {
                let value = arguments
                    .next()
                    .ok_or_else(|| "--mode requires a value".to_owned())?;
                mode = Some(AdminDashboardDemoMode::from_str(&value)?);
            }
            "--bind" if !bind_seen => {
                let value = arguments
                    .next()
                    .ok_or_else(|| "--bind requires a value".to_owned())?;
                bind = SocketAddr::from_str(&value)
                    .map_err(|_| "--bind must be a socket address".to_owned())?;
                bind_seen = true;
            }
            "--mode" => return Err("--mode may be specified only once".to_owned()),
            "--bind" => return Err("--bind may be specified only once".to_owned()),
            _ => return Err("unknown localhost demo argument".to_owned()),
        }
    }
    let mode = mode.ok_or_else(|| "--mode is required".to_owned())?;
    AdminDashboardDemoConfig::new(mode, bind)
}

async fn bind(
    config: AdminDashboardDemoConfig,
) -> Result<(TcpListener, std::net::SocketAddr), String> {
    let listener = TcpListener::bind(config.bind())
        .await
        .map_err(|error| format!("bind localhost dashboard demo: {error}"))?;
    let address = listener
        .local_addr()
        .map_err(|error| format!("read localhost dashboard demo address: {error}"))?;
    Ok((listener, address))
}

async fn serve_until<F>(
    listener: TcpListener,
    mode: AdminDashboardDemoMode,
    shutdown: F,
) -> Result<(), String>
where
    F: Future<Output = ()> + Send + 'static,
{
    axum::serve(listener, router(mode))
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(|error| format!("serve localhost dashboard demo: {error}"))
}

async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        eprintln!("localhost dashboard demo could not wait for shutdown: {error}");
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let config = parse_args(std::env::args().skip(1)).map_err(io::Error::other)?;
    let mode = config.mode();
    let (listener, address) = bind(config).await.map_err(io::Error::other)?;
    println!("Steward admin localhost demo: http://{address}/admin");
    serve_until(listener, mode, shutdown_signal())
        .await
        .map_err(io::Error::other)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::future;

    use tokio::net::TcpListener;

    use super::{AdminDashboardDemoMode, bind, parse_args, serve_until};

    #[test]
    fn invocation_requires_an_explicit_mode_and_defaults_to_loopback() -> Result<(), String> {
        let config = parse_args(["--mode".to_owned(), "authenticated".to_owned()])?;
        assert_eq!(config.mode(), AdminDashboardDemoMode::Authenticated);
        assert!(config.bind().ip().is_loopback());
        assert_eq!(config.bind().port(), 0);
        assert!(
            parse_args(Vec::<String>::new()).is_err(),
            "fixture authentication mode must never be implicit"
        );
        Ok(())
    }

    #[test]
    fn invocation_rejects_non_loopback_duplicates_and_unknown_arguments() {
        for args in [
            vec![
                "--mode".to_owned(),
                "authenticated".to_owned(),
                "--bind".to_owned(),
                "0.0.0.0:3000".to_owned(),
            ],
            vec![
                "--mode".to_owned(),
                "authenticated".to_owned(),
                "--mode".to_owned(),
                "unauthenticated".to_owned(),
            ],
            vec!["--unknown".to_owned()],
        ] {
            assert!(
                parse_args(args).is_err(),
                "unsafe or ambiguous demo invocation must fail closed"
            );
        }
    }

    #[tokio::test]
    async fn graceful_shutdown_releases_the_loopback_listener() -> Result<(), String> {
        let config = parse_args(["--mode".to_owned(), "authenticated".to_owned()])?;
        let mode = config.mode();
        let (listener, address) = bind(config).await?;
        serve_until(listener, mode, future::ready(())).await?;
        let rebound = TcpListener::bind(address)
            .await
            .map_err(|error| format!("rebind released demo listener: {error}"))?;
        drop(rebound);
        Ok(())
    }
}
