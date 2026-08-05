use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::fs;
use std::io;
use std::time::Duration;

use openshell_sdk::raw::proto::{
    AddWorkspaceMemberRequest, ListWorkspaceMembersRequest, WorkspaceRole,
};
use openshell_sdk::{EdgeAuthInterceptor, OpenShellClient, SandboxSpec};
use steward_adapter_openshell::{NameKind, stable_name};
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let client = connect().await?;
    let health = client.health().await?;
    if health.version != "0.0.98" {
        return Err(io::Error::other(format!(
            "expected OpenShell 0.0.98, gateway reported {}",
            health.version
        ))
        .into());
    }

    let workspace_a = stable_name(NameKind::Workspace, b"team-a");
    let workspace_b = stable_name(NameKind::Workspace, b"team-b");
    let sandbox_name = stable_name(NameKind::Sandbox, b"runtime-uid-1");

    let result = verify_contract(&client, &workspace_a, &workspace_b, &sandbox_name).await;
    cleanup(&client, &workspace_a, &workspace_b, &sandbox_name).await;
    result?;

    println!(
        "OpenShell 0.0.98 workspace contract confirmed: scoped duplicate names, membership, 19-char cap, and sandbox-before-workspace teardown"
    );
    Ok(())
}

async fn connect() -> Result<OpenShellClient, Box<dyn Error>> {
    let required =
        |name: &str| env::var(name).map_err(|_| io::Error::other(format!("{name} is required")));
    let read = |name: &str| -> Result<Vec<u8>, io::Error> {
        let path = required(name)?;
        fs::read(path).map_err(|error| io::Error::other(format!("failed to read {name}: {error}")))
    };
    let endpoint = required("STEWARD_OPENSHELL_ENDPOINT")?;
    if !endpoint.starts_with("https://") {
        return Err(io::Error::other("STEWARD_OPENSHELL_ENDPOINT must use HTTPS").into());
    }
    let tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(read(
            "STEWARD_OPENSHELL_CA_CERTIFICATE_FILE",
        )?))
        .identity(Identity::from_pem(
            read("STEWARD_OPENSHELL_CLIENT_CERTIFICATE_FILE")?,
            read("STEWARD_OPENSHELL_CLIENT_PRIVATE_KEY_FILE")?,
        ))
        .domain_name(required("STEWARD_OPENSHELL_SERVER_NAME")?);
    let channel = Endpoint::from_shared(endpoint)?
        .tls_config(tls)?
        .connect()
        .await?;
    let bearer_token = required("STEWARD_OPENSHELL_TEST_BEARER_TOKEN")?;
    let interceptor = EdgeAuthInterceptor::new(Some(&bearer_token), None)?;
    Ok(OpenShellClient::from_parts(channel, interceptor))
}

async fn verify_contract(
    client: &OpenShellClient,
    workspace_a: &str,
    workspace_b: &str,
    sandbox_name: &str,
) -> Result<(), Box<dyn Error>> {
    client.create_workspace(workspace_a, HashMap::new()).await?;
    client.create_workspace(workspace_b, HashMap::new()).await?;

    let too_long = "a".repeat(20);
    if client
        .create_workspace(&too_long, HashMap::new())
        .await
        .is_ok()
    {
        return Err(io::Error::other("OpenShell accepted a 20-character workspace name").into());
    }

    let mut grpc = client.raw_grpc();
    grpc.add_workspace_member(AddWorkspaceMemberRequest {
        workspace: workspace_a.to_owned(),
        principal_subject: "alice@example.com".to_owned(),
        role: WorkspaceRole::User.into(),
    })
    .await?;
    let members = grpc
        .list_workspace_members(ListWorkspaceMembersRequest {
            workspace: workspace_a.to_owned(),
            limit: 100,
            offset: 0,
        })
        .await?
        .into_inner()
        .members;
    if !members
        .iter()
        .any(|member| member.principal_subject == "alice@example.com")
    {
        return Err(io::Error::other("workspace member was not scoped and persisted").into());
    }

    for workspace in [workspace_a, workspace_b] {
        client
            .workspace(workspace)
            .create_sandbox(SandboxSpec {
                name: Some(sandbox_name.to_owned()),
                image: None,
                ..SandboxSpec::default()
            })
            .await?;
        let sandbox = client
            .workspace(workspace)
            .wait_ready(sandbox_name, Duration::from_secs(300))
            .await?;
        if sandbox.workspace != workspace {
            return Err(io::Error::other(format!(
                "sandbox resolved to workspace {}, expected {workspace}",
                sandbox.workspace
            ))
            .into());
        }
    }

    if client.delete_workspace(workspace_a).await.is_ok() {
        return Err(
            io::Error::other("workspace deletion succeeded while a sandbox still existed").into(),
        );
    }

    Ok(())
}

async fn cleanup(
    client: &OpenShellClient,
    workspace_a: &str,
    workspace_b: &str,
    sandbox_name: &str,
) {
    for workspace in [workspace_a, workspace_b] {
        let scoped = client.workspace(workspace);
        if scoped.delete_sandbox(sandbox_name).await.is_ok() {
            let _ = scoped
                .wait_deleted(sandbox_name, Duration::from_secs(300))
                .await;
        }
        let _ = client.delete_workspace(workspace).await;
    }
}
