use kube::Client as KubeClient;
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::net::SocketAddr;
use steward_adapter_openshell::WorkloadExchangeTransport;
use steward_store::PgStore;
use steward_trigger_receiver::client::{
    FileCatalog, KubernetesTokenRequester, KubernetesWorkloadTokenProvider, PgEnvelopeAuthority,
    TaskClientTiming, WorkloadExchangeClient,
};
use steward_trigger_receiver::{Receiver, ReceiverConfig, ReceiverLimits};

const MAX_CONFIG_BYTES: usize = 4 * 1024 * 1024;
const MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_IDENTIFIER_BYTES: usize = 1024;
const MAX_TRIGGER_COUNT: usize = 4096;
const MAX_CONCURRENCY: usize = 1024;
const MAX_STATE_ENTRIES: usize = 1_000_000;
const MAX_SIGNING_SECRET_BYTES: usize = 64 * 1024;
const MAX_COOLDOWN_SECONDS: u64 = 31_536_000;
const MAX_DATABASE_ACQUIRE_MILLISECONDS: u64 = 60_000;
const MAX_TASK_DEADLINE_MILLISECONDS: u64 = 86_400_000;
const MAX_POLL_MILLISECONDS: u64 = 60_000;
const MAX_CONNECT_TIMEOUT_MILLISECONDS: u64 = 60_000;
const MAX_REQUEST_TIMEOUT_MILLISECONDS: u64 = 300_000;
const MAX_REPLY_BYTES: usize = 1024 * 1024;
const MAX_EXCHANGE_REPLY_BYTES: usize = 64 * 1024;
const MIN_SOURCE_TOKEN_SECONDS: u64 = 600;
const MAX_SOURCE_TOKEN_SECONDS: u64 = 86_400;

fn required(name: &str) -> Result<String, Box<dyn std::error::Error>> {
    env::var(name).map_err(|_| format!("{name} is required").into())
}

fn bounded_u64(name: &str, value: String, min: u64, max: u64) -> Result<u64, String> {
    let value = value
        .parse::<u64>()
        .map_err(|_| format!("{name} must be a whole number"))?;
    if !(min..=max).contains(&value) {
        return Err(format!("{name} is outside its supported range"));
    }
    Ok(value)
}

fn bounded_usize(name: &str, value: String, max: usize) -> Result<usize, String> {
    let value = value
        .parse::<usize>()
        .map_err(|_| format!("{name} must be a whole number"))?;
    if !(1..=max).contains(&value) {
        return Err(format!("{name} is outside its supported range"));
    }
    Ok(value)
}

struct RuntimeSettings {
    limits: ReceiverLimits,
    database_acquire_timeout: std::time::Duration,
    source_token_seconds: i64,
    exchange_transport: WorkloadExchangeTransport,
    task_timing: TaskClientTiming,
}

fn runtime_settings_from(
    mut get: impl FnMut(&str) -> Result<String, String>,
) -> Result<RuntimeSettings, String> {
    let usize_value =
        |name: &str, max: usize, get: &mut dyn FnMut(&str) -> Result<String, String>| {
            bounded_usize(name, get(name)?, max)
        };
    let u64_value =
        |name: &str, min: u64, max: u64, get: &mut dyn FnMut(&str) -> Result<String, String>| {
            bounded_u64(name, get(name)?, min, max)
        };
    let duration = |name: &str, max: u64, get: &mut dyn FnMut(&str) -> Result<String, String>| {
        u64_value(name, 1, max, get).map(std::time::Duration::from_millis)
    };
    let limits = ReceiverLimits {
        max_config_bytes: usize_value(
            "STEWARD_TRIGGER_MAX_CONFIG_BYTES",
            MAX_CONFIG_BYTES,
            &mut get,
        )?,
        max_body_bytes: usize_value("STEWARD_TRIGGER_MAX_BODY_BYTES", MAX_BODY_BYTES, &mut get)?,
        max_identifier_bytes: usize_value(
            "STEWARD_TRIGGER_MAX_IDENTIFIER_BYTES",
            MAX_IDENTIFIER_BYTES,
            &mut get,
        )?,
        max_triggers: usize_value("STEWARD_TRIGGER_MAX_TRIGGERS", MAX_TRIGGER_COUNT, &mut get)?,
        max_ignored_accounts: usize_value(
            "STEWARD_TRIGGER_MAX_IGNORED_ACCOUNTS",
            MAX_TRIGGER_COUNT,
            &mut get,
        )?,
        max_concurrency: usize_value("STEWARD_TRIGGER_MAX_CONCURRENCY", MAX_CONCURRENCY, &mut get)?,
        max_cooldown_seconds: u64_value(
            "STEWARD_TRIGGER_MAX_COOLDOWN_SECONDS",
            1,
            MAX_COOLDOWN_SECONDS,
            &mut get,
        )?,
        max_recent_deliveries: usize_value(
            "STEWARD_TRIGGER_MAX_RECENT_DELIVERIES",
            MAX_STATE_ENTRIES,
            &mut get,
        )?,
        max_issue_cooldowns: usize_value(
            "STEWARD_TRIGGER_MAX_ISSUE_COOLDOWNS",
            MAX_STATE_ENTRIES,
            &mut get,
        )?,
        max_signing_secret_bytes: usize_value(
            "STEWARD_TRIGGER_MAX_SIGNING_SECRET_BYTES",
            MAX_SIGNING_SECRET_BYTES,
            &mut get,
        )?,
    };
    let database_acquire_timeout = duration(
        "STEWARD_TRIGGER_DATABASE_ACQUIRE_TIMEOUT_MILLISECONDS",
        MAX_DATABASE_ACQUIRE_MILLISECONDS,
        &mut get,
    )?;
    let source_token_seconds = u64_value(
        "STEWARD_WORKLOAD_SOURCE_TOKEN_SECONDS",
        MIN_SOURCE_TOKEN_SECONDS,
        MAX_SOURCE_TOKEN_SECONDS,
        &mut get,
    )? as i64;
    let exchange_transport = WorkloadExchangeTransport {
        connect_timeout: duration(
            "STEWARD_WORKLOAD_EXCHANGE_CONNECT_TIMEOUT_MILLISECONDS",
            MAX_CONNECT_TIMEOUT_MILLISECONDS,
            &mut get,
        )?,
        request_timeout: duration(
            "STEWARD_WORKLOAD_EXCHANGE_REQUEST_TIMEOUT_MILLISECONDS",
            MAX_REQUEST_TIMEOUT_MILLISECONDS,
            &mut get,
        )?,
        max_response_bytes: usize_value(
            "STEWARD_WORKLOAD_EXCHANGE_MAX_REPLY_BYTES",
            MAX_EXCHANGE_REPLY_BYTES,
            &mut get,
        )?,
    };
    let task_timing = TaskClientTiming {
        deadline: duration(
            "STEWARD_TRIGGER_TASK_DEADLINE_MILLISECONDS",
            MAX_TASK_DEADLINE_MILLISECONDS,
            &mut get,
        )?,
        initial_poll_delay: duration(
            "STEWARD_TRIGGER_POLL_INITIAL_MILLISECONDS",
            MAX_POLL_MILLISECONDS,
            &mut get,
        )?,
        max_poll_delay: duration(
            "STEWARD_TRIGGER_POLL_MAX_MILLISECONDS",
            MAX_POLL_MILLISECONDS,
            &mut get,
        )?,
        connect_timeout: duration(
            "STEWARD_TRIGGER_API_CONNECT_TIMEOUT_MILLISECONDS",
            MAX_CONNECT_TIMEOUT_MILLISECONDS,
            &mut get,
        )?,
        request_timeout: duration(
            "STEWARD_TRIGGER_API_REQUEST_TIMEOUT_MILLISECONDS",
            MAX_REQUEST_TIMEOUT_MILLISECONDS,
            &mut get,
        )?,
        max_reply_bytes: usize_value(
            "STEWARD_TRIGGER_MAX_TASK_REPLY_BYTES",
            MAX_REPLY_BYTES,
            &mut get,
        )?,
    }
    .validate()?;
    if exchange_transport.request_timeout < exchange_transport.connect_timeout {
        return Err("workload exchange timeouts are inconsistent".to_owned());
    }
    Ok(RuntimeSettings {
        limits,
        database_acquire_timeout,
        source_token_seconds,
        exchange_transport,
        task_timing,
    })
}

fn runtime_settings() -> Result<RuntimeSettings, Box<dyn std::error::Error>> {
    runtime_settings_from(|name| env::var(name).map_err(|_| format!("{name} is required")))
        .map_err(Into::into)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let settings = runtime_settings()?;
    let config = ReceiverConfig::from_json(
        &fs::read_to_string(required("STEWARD_TRIGGER_CONFIG_FILE")?)?,
        &settings.limits,
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
        settings.database_acquire_timeout,
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
        settings.source_token_seconds,
    )?;
    let tokens = KubernetesWorkloadTokenProvider::new(
        token_requester,
        required("STEWARD_WORKLOAD_EXCHANGE_ENDPOINT")?,
        required("STEWARD_WORKLOAD_EXCHANGE_SERVER_NAME")?,
        fs::read(required("STEWARD_WORKLOAD_EXCHANGE_CA_CERTIFICATE_FILE")?)?,
        settings.exchange_transport,
    )?;
    let tasks = WorkloadExchangeClient::new(
        required("STEWARD_API_ORIGIN")?,
        required("STEWARD_API_SERVER_NAME")?,
        fs::read(required("STEWARD_API_CA_CERTIFICATE_FILE")?)?,
        tokens,
        settings.task_timing,
    )?;
    let receiver = Receiver::new(
        config,
        secrets,
        catalog,
        PgEnvelopeAuthority::new(store),
        tasks,
        settings.limits,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_settings_accept_an_unknown_value_below_the_ceiling() -> Result<(), String> {
        let settings = runtime_settings_from(|name| {
            Ok(match name {
                "STEWARD_TRIGGER_MAX_CONCURRENCY" => "137",
                "STEWARD_WORKLOAD_SOURCE_TOKEN_SECONDS" => "601",
                "STEWARD_TRIGGER_TASK_DEADLINE_MILLISECONDS" => "700000",
                "STEWARD_TRIGGER_API_CONNECT_TIMEOUT_MILLISECONDS"
                | "STEWARD_WORKLOAD_EXCHANGE_CONNECT_TIMEOUT_MILLISECONDS" => "100",
                "STEWARD_TRIGGER_API_REQUEST_TIMEOUT_MILLISECONDS"
                | "STEWARD_WORKLOAD_EXCHANGE_REQUEST_TIMEOUT_MILLISECONDS" => "200",
                "STEWARD_TRIGGER_POLL_INITIAL_MILLISECONDS" => "10",
                "STEWARD_TRIGGER_POLL_MAX_MILLISECONDS" => "20",
                _ => "1000",
            }
            .to_owned())
        })?;
        assert_eq!(settings.limits.max_concurrency, 137);
        assert_eq!(settings.source_token_seconds, 601);
        assert!(
            settings.task_timing.deadline
                > std::time::Duration::from_secs(settings.source_token_seconds as u64)
        );
        assert_eq!(settings.task_timing.max_reply_bytes, 1000);
        assert_eq!(settings.exchange_transport.max_response_bytes, 1000);
        Ok(())
    }

    #[test]
    fn environment_settings_reject_values_above_the_ceiling() {
        let result = runtime_settings_from(|name| {
            Ok(if name == "STEWARD_TRIGGER_MAX_CONCURRENCY" {
                (MAX_CONCURRENCY + 1).to_string()
            } else if name == "STEWARD_WORKLOAD_SOURCE_TOKEN_SECONDS" {
                MIN_SOURCE_TOKEN_SECONDS.to_string()
            } else {
                "1000".to_owned()
            })
        });
        assert!(result.is_err());
    }
}
