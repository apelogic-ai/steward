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
        .map_err(|_| format!("{name} must be a positive number of milliseconds"))?;
    if value == 0 {
        return Err(format!("{name} must be greater than zero").into());
    }
    Ok(std::time::Duration::from_millis(value))
}

fn required_usize(name: &str) -> Result<usize, Box<dyn std::error::Error>> {
    let value = required(name)?
        .parse::<usize>()
        .map_err(|_| format!("{name} must be a positive integer"))?;
    if value == 0 {
        return Err(format!("{name} must be greater than zero").into());
    }
    Ok(value)
}

fn required_u64(name: &str) -> Result<u64, Box<dyn std::error::Error>> {
    let value = required(name)?
        .parse::<u64>()
        .map_err(|_| format!("{name} must be a positive integer"))?;
    if value == 0 {
        return Err(format!("{name} must be greater than zero").into());
    }
    Ok(value)
}

fn limits() -> Result<ReceiverLimits, Box<dyn std::error::Error>> {
    Ok(ReceiverLimits {
        max_config_bytes: required_usize("STEWARD_TRIGGER_MAX_CONFIG_BYTES")?,
        max_body_bytes: required_usize("STEWARD_TRIGGER_MAX_BODY_BYTES")?,
        max_identifier_bytes: required_usize("STEWARD_TRIGGER_MAX_IDENTIFIER_BYTES")?,
        max_triggers: required_usize("STEWARD_TRIGGER_MAX_TRIGGERS")?,
        max_ignored_accounts: required_usize("STEWARD_TRIGGER_MAX_IGNORED_ACCOUNTS")?,
        max_concurrency: required_usize("STEWARD_TRIGGER_MAX_CONCURRENCY")?,
        max_cooldown_seconds: required_u64("STEWARD_TRIGGER_MAX_COOLDOWN_SECONDS")?,
        max_recent_deliveries: required_usize("STEWARD_TRIGGER_MAX_RECENT_DELIVERIES")?,
        max_issue_cooldowns: required_usize("STEWARD_TRIGGER_MAX_ISSUE_COOLDOWNS")?,
        max_signing_secret_bytes: required_usize("STEWARD_TRIGGER_MAX_SIGNING_SECRET_BYTES")?,
    })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let limits = limits()?;
    let config = ReceiverConfig::from_json(
        &fs::read_to_string(required("STEWARD_TRIGGER_CONFIG_FILE")?)?,
        &limits,
    )?;
    let mut secrets = BTreeMap::new();
    for trigger in &config.triggers {
        if trigger.enabled {
            secrets.insert(trigger.id.clone(), fs::read(&trigger.signing_secret_file)?);
        }
    }
    let catalog = FileCatalog::new(required("STEWARD_CAPABILITY_CATALOG_FILE")?.into())?;
    let store = PgStore::connect_with_acquire_timeout(
        &required("STEWARD_DATABASE_URL")?,
        required_duration_ms("STEWARD_TRIGGER_DATABASE_ACQUIRE_TIMEOUT_MILLISECONDS")?,
    )
    .await?;
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
            connect_timeout: required_duration_ms(
                "STEWARD_TRIGGER_API_CONNECT_TIMEOUT_MILLISECONDS",
            )?,
            request_timeout: required_duration_ms(
                "STEWARD_TRIGGER_API_REQUEST_TIMEOUT_MILLISECONDS",
            )?,
            max_reply_bytes: required_usize("STEWARD_TRIGGER_MAX_TASK_REPLY_BYTES")?,
        },
    )?;
    let receiver = Receiver::new(
        config,
        secrets,
        catalog,
        PgEnvelopeAuthority::new(store),
        tasks,
        limits,
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
