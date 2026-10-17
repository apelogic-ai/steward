use kube::Client as KubeClient;
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::net::SocketAddr;
use steward_store::PgStore;
use steward_trigger_receiver::client::{
    FileCatalog, KubernetesTokenRequester, KubernetesWorkloadTokenProvider, PgEnvelopeAuthority,
    TaskClientTiming, WorkloadExchangeClient,
};
use steward_trigger_receiver::{Receiver, ReceiverConfig, ReceiverLimits};

fn required(name: &str) -> Result<String, Box<dyn std::error::Error>> {
    env::var(name).map_err(|_| format!("{name} is required").into())
}

fn required_duration_ms(name: &str) -> Result<std::time::Duration, Box<dyn std::error::Error>> {
    let value = required(name)?
        .parse::<u64>()
        .map_err(|_| format!("{name} must be milliseconds"))?;
    Ok(std::time::Duration::from_millis(value))
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
    let catalog = FileCatalog::new(required("STEWARD_CAPABILITY_CATALOG_FILE")?.into())?;
    let store = PgStore::connect(&required("STEWARD_DATABASE_URL")?).await?;
    let kube = KubeClient::try_default().await?;
    let namespace = required("STEWARD_TRIGGER_NAMESPACE")?;
    let service_account = required("STEWARD_TRIGGER_SERVICE_ACCOUNT")?;
    let token_requester = KubernetesTokenRequester::new(
        kube,
        namespace,
        service_account,
        required("STEWARD_WORKLOAD_SOURCE_AUDIENCE")?,
    )?;
    let tokens = KubernetesWorkloadTokenProvider::new(
        token_requester,
        required("STEWARD_WORKLOAD_EXCHANGE_ENDPOINT")?,
        required("STEWARD_WORKLOAD_EXCHANGE_SERVER_NAME")?,
        fs::read(required("STEWARD_WORKLOAD_EXCHANGE_CA_CERTIFICATE_FILE")?)?,
    )?;
    let tasks = WorkloadExchangeClient::new(
        required("STEWARD_API_ORIGIN")?,
        required("STEWARD_API_SERVER_NAME")?,
        fs::read(required("STEWARD_API_CA_CERTIFICATE_FILE")?)?,
        tokens,
        TaskClientTiming {
            deadline: required_duration_ms("STEWARD_TRIGGER_TASK_DEADLINE_MILLISECONDS")?,
            initial_poll_delay: required_duration_ms("STEWARD_TRIGGER_POLL_INITIAL_MILLISECONDS")?,
            max_poll_delay: required_duration_ms("STEWARD_TRIGGER_POLL_MAX_MILLISECONDS")?,
        },
    )?;
    let receiver = Receiver::new(
        config,
        secrets,
        catalog,
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
