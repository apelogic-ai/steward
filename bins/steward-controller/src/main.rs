use std::env;
use std::error::Error;
use std::io;

use kube::Client;
use steward_adapter_openshell::OpenShellRuntime;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let endpoint = env::var("STEWARD_OPENSHELL_ENDPOINT")
        .map_err(|_| io::Error::other("STEWARD_OPENSHELL_ENDPOINT is required"))?;
    let client = Client::try_default().await?;
    let sandbox_runtime = OpenShellRuntime::connect(endpoint)
        .await
        .map_err(|error| io::Error::other(format!("OpenShell connection failed: {error:?}")))?;
    steward_controller::run_controller(client, sandbox_runtime).await;
    Ok(())
}
