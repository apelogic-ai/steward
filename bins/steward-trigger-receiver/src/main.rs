use kube::Client as KubeClient;
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::net::SocketAddr;
use steward_apiserver::browser_admin::CapabilityCatalog;
use steward_store::PgStore;
use steward_trigger_receiver::client::{
    PgEnvelopeAuthority, StaticCatalog, WorkloadExchangeClient,
};
use steward_trigger_receiver::{Receiver, ReceiverConfig, ReceiverLimits};

fn required(name: &str) -> Result<String, Box<dyn std::error::Error>> {
    env::var(name).map_err(|_| format!("{name} is required").into())
}

fn limits() -> ReceiverLimits {
    ReceiverLimits {
        max_config_bytes: 64 * 1024,
        max_body_bytes: 8 * 1024,
        max_identifier_bytes: 128,
        max_triggers: 32,
        max_ignored_accounts: 32,
        max_concurrency: 32,
        max_cooldown_seconds: 86_400,
        max_recent_deliveries: 256,
        max_issue_cooldowns: 64,
        max_signing_secret_bytes: 256,
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = ReceiverConfig::from_json(
        &fs::read_to_string(required("STEWARD_TRIGGER_CONFIG_FILE")?)?,
        &limits(),
    )?;
    let mut secrets = BTreeMap::new();
    for trigger in &config.triggers {
        if trigger.enabled {
            secrets.insert(trigger.id.clone(), fs::read(&trigger.signing_secret_file)?);
        }
    }
    let catalog = CapabilityCatalog::from_json(&fs::read_to_string(required(
        "STEWARD_CAPABILITY_CATALOG_FILE",
    )?)?)?;
    let store = PgStore::connect(&required("STEWARD_DATABASE_URL")?).await?;
    let kube = KubeClient::try_default().await?;
    let namespace = required("STEWARD_TRIGGER_NAMESPACE")?;
    let service_account = required("STEWARD_TRIGGER_SERVICE_ACCOUNT")?;
    let audience = required("STEWARD_TASK_AUDIENCE")?;
    let token_requester = steward_trigger_receiver::client::KubernetesTokenRequester::new(
        kube,
        namespace,
        service_account,
        audience.clone(),
    )?;
    let source_token = token_requester.request().await?;
    let tasks = WorkloadExchangeClient::new(
        required("STEWARD_WORKLOAD_EXCHANGE_ENDPOINT")?,
        required("STEWARD_API_ORIGIN")?,
        audience,
        source_token,
    )?;
    let receiver = Receiver::new(
        config,
        secrets,
        StaticCatalog(catalog),
        PgEnvelopeAuthority::new(store),
        tasks,
        limits(),
    )
    .await
    .map_err(std::io::Error::other)?;
    let address: SocketAddr = env::var("STEWARD_TRIGGER_BIND")
        .unwrap_or_else(|_| "0.0.0.0:8080".to_owned())
        .parse()?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    axum::serve(listener, steward_trigger_receiver::router(receiver))
        .await
        .map_err(Into::into)
}
