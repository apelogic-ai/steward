use std::collections::BTreeMap;
use std::env;
use std::error::Error;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use axum::serve::Listener;
use reqwest::{Method, StatusCode as HttpStatusCode, Url};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use steward_adapter_claude_code::ClaudeCodeTaskExecutionAdapter;
use steward_adapter_codex::CodexTaskExecutionAdapter;
use steward_adapter_github_artifact::GitHubArtifactVerifier;
use steward_adapter_github_source::{GitHubAppCredentials, GitHubSourceAdapter};
use steward_adapter_jira::{JiraAdapter, JiraConfig};
use steward_apiserver::operator_admin::{
    OperatorAssignmentAction, OperatorAssignmentKind, OperatorAssignmentRequest,
    OperatorAssignmentResponse, OperatorEffectiveAccessResponse, OperatorProvisionRequest,
    OperatorProvisionResponse, OperatorRolesResponse, OperatorTemplateApplyRequest,
    OperatorTemplateResponse, OperatorUserView, OperatorUsersResponse,
};
use steward_apiserver::task_auth::{TaskAuthDiscoveryConfig, task_auth_discovery_router};
use steward_apiserver::{
    ConfiguredTaskIdentityResolver, ExecutionBindingCatalog,
    IdentityOrKubernetesTokenAuthenticator, KubeRuntimeRepository, KubernetesTokenAuthenticator,
    KubernetesTokenReviewAudience, MAX_EXECUTION_BINDING_CATALOG_BYTES,
    MAX_SOURCE_REPOSITORY_BINDINGS_BYTES, StewardRunWorkflowInstallationMode, TaskApiConfig,
    agent_runs_ui, browser_admin, browser_auth, browser_task_rerunner, browser_task_router,
    connections, github_automation, google_oidc, governed_connections, inference_connections,
    operator_admin, router, stable_runtime_bridge, task_router, user_envelopes, workflows,
};
use steward_store::{
    BrowserRbacAssignment, BrowserRbacAssignmentAction, BrowserRbacAssignmentChange,
    EnvelopeTemplatePublication, ManagedInferenceKeyCipher, PgStore, TaskOrchestrationMode,
};
use steward_types::{CanonicalUserId, InferenceMode, OrganizationId};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{JoinError, JoinSet};
use tokio::time::{sleep, timeout};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio_rustls::server::TlsStream;

#[cfg(not(test))]
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(test)]
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_millis(25);
const MAX_PENDING_TLS_HANDSHAKES: usize = 64;

const OPERATOR_EXIT_USAGE: u8 = 2;
const OPERATOR_EXIT_NOT_FOUND: u8 = 3;
const OPERATOR_EXIT_FORBIDDEN: u8 = 4;
const OPERATOR_EXIT_CONFLICT: u8 = 5;
const OPERATOR_EXIT_UNAVAILABLE: u8 = 6;

#[derive(Debug)]
enum OperatorCommandError {
    Invalid(&'static str),
    NotFound(&'static str),
    Forbidden(&'static str),
    Conflict(&'static str),
    Unavailable(&'static str),
}

impl std::fmt::Display for OperatorCommandError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message)
            | Self::NotFound(message)
            | Self::Forbidden(message)
            | Self::Conflict(message)
            | Self::Unavailable(message) => formatter.write_str(message),
        }
    }
}

impl Error for OperatorCommandError {}

#[tokio::main]
async fn main() -> ExitCode {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    let operator_command = arguments.first().is_some_and(|command| {
        matches!(
            command.as_str(),
            "bootstrap-rbac" | "rbac" | "templates" | "envelopes"
        )
    });
    match run(arguments).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(if operator_command {
                operator_exit_code(error.as_ref())
            } else {
                1
            })
        }
    }
}

async fn run(arguments: Vec<String>) -> Result<(), Box<dyn Error>> {
    install_rustls_crypto_provider()?;
    let mut arguments = arguments.into_iter();
    match arguments.next().as_deref() {
        Some("bootstrap-rbac") => return bootstrap_rbac(arguments.collect()).await,
        Some("rbac") => return rbac_command(arguments.collect()).await,
        Some("templates") => return templates_command(arguments.collect()).await,
        Some("envelopes") => return envelopes_command(arguments.collect()).await,
        Some("validate-execution-bindings") => {
            return validate_execution_bindings(arguments.collect());
        }
        Some("validate-jira-config") => {
            jira_adapter()?;
            return Ok(());
        }
        Some("validate-core-config") => {
            core_only_configuration()?;
            return Ok(());
        }
        Some(command) => {
            return Err(io::Error::other(format!("unknown command {command}")).into());
        }
        None => {}
    }
    let client = kube::Client::try_default().await?;
    let task_orchestration_mode = task_orchestration_mode()?;
    let inference_mode = inference_mode()?;
    let managed_inference_cipher = managed_inference_cipher(inference_mode)?;
    let store = PgStore::connect(&required("STEWARD_DATABASE_URL")?).await?;
    store.migrate().await?;
    ensure_default_llm_template(&store).await?;
    tokio::spawn(
        steward_apiserver::governed_connections::ConnectionOperationReconciler::new(store.clone())
            .run(),
    );
    let decisions = jira_adapter()?;
    let token_review_audience = kubernetes_token_review_audience(
        env::var("STEWARD_KUBERNETES_TOKEN_REVIEW_AUDIENCE").ok(),
    )?;
    let admin_group =
        env::var("STEWARD_ADMIN_GROUP").unwrap_or_else(|_| "agents.apelogic.ai/admin".to_owned());
    let kubernetes_authenticator = KubernetesTokenAuthenticator::new(
        client.clone(),
        admin_group.clone(),
        token_review_audience.clone(),
    );
    let configured_task_identity =
        configured_task_identity_resolver(client.clone(), token_review_audience, store.clone())?;
    let connection_auto_association_issuer = configured_task_identity
        .connection_auto_association_issuer
        .clone();
    let task_identity_issuer = configured_task_identity.task_identity_issuer.clone();
    let task_identity_discovery_enabled = configured_task_identity.discovery.is_some();
    let task_identities = configured_task_identity.resolver;
    let authenticator = IdentityOrKubernetesTokenAuthenticator::new(
        kubernetes_authenticator,
        task_identities.clone(),
        admin_group,
    );
    let task_mcp_gateway_endpoint = match env::var("STEWARD_TASK_MCP_GW_ENDPOINT") {
        Ok(value) => Some(value),
        Err(env::VarError::NotPresent) => None,
        Err(env::VarError::NotUnicode(_)) => {
            return Err(io::Error::other("STEWARD_TASK_MCP_GW_ENDPOINT must be Unicode").into());
        }
    };
    let task_execution_bindings_json = configured_execution_bindings_json()?;
    let source_repository_bindings_json = configured_source_repository_bindings_json()?;
    let github_source = configured_github_source_adapter()?;
    let github_source_enabled = github_source.is_some();
    let task_auth_discovery = configured_task_identity
        .discovery
        .map(|config| config.with_direct_packages_supported(github_source.is_some()));
    let task_execution_bindings_active = execution_bindings_active().map_err(io::Error::other)?;
    let mut task_api_config =
        TaskApiConfig::new(task_mcp_gateway_endpoint).map_err(io::Error::other)?;
    if execution_enabled()? {
        let task_execution_adapter = CodexTaskExecutionAdapter::new(required(
            "STEWARD_TASK_INFERENCE_ENDPOINT",
        )?)
        .map_err(|error| {
            io::Error::other(format!(
                "Codex execution adapter configuration failed: {error:?}"
            ))
        })?;
        task_api_config = task_api_config
            .with_execution_adapter(Arc::new(task_execution_adapter))
            .map_err(io::Error::other)?;
        task_api_config = with_claude_code_execution_adapter(
            task_api_config,
            optional_unicode_environment("STEWARD_TASK_ANTHROPIC_INFERENCE_ENDPOINT")?,
        )?;
    } else {
        core_only_configuration()?;
    }
    let task_api_config = task_api_config
        .with_execution_bindings_json(task_execution_bindings_json.as_deref())
        .and_then(|config| {
            config.with_source_repository_bindings_json(source_repository_bindings_json.as_deref())
        })
        .and_then(|config| config.with_execution_bindings_active(task_execution_bindings_active))
        .map_err(io::Error::other)?;
    let task_api_config = match github_source {
        Some(adapter) => task_api_config.with_git_hosting_plane(adapter),
        None => task_api_config,
    };
    let task_api_config = task_api_config
        .with_task_orchestration_mode(task_orchestration_mode)
        .with_inference_mode(inference_mode);
    let workflow_agents = task_api_config.execution_binding_advertisements();
    let runtimes = KubeRuntimeRepository::new(client);
    let browser = browser_application_router(
        store.clone(),
        runtimes.clone(),
        decisions.clone(),
        workflow_agents,
        task_api_config.clone(),
        BrowserApplicationConfig {
            task_orchestration_mode,
            connection_auto_association_issuer,
            task_identity_discovery_enabled,
            task_execution_bindings_active,
            github_source_enabled,
            github_actor_issuer: task_identity_issuer,
            inference_mode,
            managed_inference_cipher,
        },
    )
    .await?;
    let app = router(
        runtimes.clone(),
        store.clone(),
        authenticator.clone(),
        decisions.clone(),
    )
    .merge(operator_admin::router(store.clone(), authenticator))
    .merge(task_auth_discovery_router(task_auth_discovery))
    .merge(task_router(store.clone(), task_identities, task_api_config));
    let app = match browser {
        Some(browser) => app.merge(browser),
        None => app,
    };
    let listener = tls_listener(
        &env::var("STEWARD_APISERVER_BIND").unwrap_or_else(|_| "0.0.0.0:8443".to_owned()),
        &required("STEWARD_TLS_CERT_DER")?,
        &required("STEWARD_TLS_KEY_DER")?,
    )
    .await?;
    axum::serve(listener, app).await?;
    Ok(())
}

fn operator_exit_code(error: &(dyn Error + 'static)) -> u8 {
    if let Some(error) = error.downcast_ref::<OperatorCommandError>() {
        return match error {
            OperatorCommandError::Invalid(_) => OPERATOR_EXIT_USAGE,
            OperatorCommandError::NotFound(_) => OPERATOR_EXIT_NOT_FOUND,
            OperatorCommandError::Forbidden(_) => OPERATOR_EXIT_FORBIDDEN,
            OperatorCommandError::Conflict(_) => OPERATOR_EXIT_CONFLICT,
            OperatorCommandError::Unavailable(_) => OPERATOR_EXIT_UNAVAILABLE,
        };
    }
    if let Some(error) = error.downcast_ref::<steward_store::StoreError>() {
        return match error {
            steward_store::StoreError::Database(_) => OPERATOR_EXIT_UNAVAILABLE,
            steward_store::StoreError::CanonicalIdentityInactive
            | steward_store::StoreError::FederatedSubjectDisabled => OPERATOR_EXIT_FORBIDDEN,
            steward_store::StoreError::CanonicalIdentityNotFound
            | steward_store::StoreError::EnvelopeTemplateNotFound
            | steward_store::StoreError::EnvelopeRequestNotFound
            | steward_store::StoreError::WorkflowNotFound
            | steward_store::StoreError::TaskNotFound
            | steward_store::StoreError::ApprovalNotFound
            | steward_store::StoreError::ConnectionOperationNotFound
            | steward_store::StoreError::CumulativeEscalationNotFound => OPERATOR_EXIT_NOT_FOUND,
            steward_store::StoreError::CanonicalIdentityConflict
            | steward_store::StoreError::EnvelopeRequestDigestConflict
            | steward_store::StoreError::EnvelopeRequestIdempotencyConflict
            | steward_store::StoreError::EnvelopeRequestTemplateStale
            | steward_store::StoreError::EnvelopeRevisionNotIncreasing
            | steward_store::StoreError::TaskIdempotencyConflict
            | steward_store::StoreError::WorkflowAlreadyExists => OPERATOR_EXIT_CONFLICT,
            _ => OPERATOR_EXIT_USAGE,
        };
    }
    OPERATOR_EXIT_USAGE
}

fn execution_enabled() -> Result<bool, io::Error> {
    match env::var("STEWARD_EXECUTION_ENABLED") {
        Ok(value) if value == "true" => Ok(true),
        Ok(value) if value == "false" => Ok(false),
        Err(env::VarError::NotPresent) => Ok(true),
        _ => Err(io::Error::other(
            "STEWARD_EXECUTION_ENABLED must be true or false",
        )),
    }
}

fn inference_mode() -> Result<InferenceMode, io::Error> {
    let value = env::var("STEWARD_INFERENCE_MODE").unwrap_or_else(|_| "stock".to_owned());
    InferenceMode::parse(&value).map_err(io::Error::other)
}

fn managed_inference_cipher(
    mode: InferenceMode,
) -> Result<Option<ManagedInferenceKeyCipher>, io::Error> {
    let path = env::var("STEWARD_MANAGED_INFERENCE_ENCRYPTION_KEY_FILE").ok();
    match (mode, path) {
        (InferenceMode::Stock, None) => Ok(None),
        (InferenceMode::Stock, Some(_)) => Err(io::Error::other(
            "stock inference mode must not configure a managed credential encryption key",
        )),
        (InferenceMode::Managed, None) => Err(io::Error::other(
            "managed inference mode requires STEWARD_MANAGED_INFERENCE_ENCRYPTION_KEY_FILE",
        )),
        (InferenceMode::Managed, Some(path)) => fs::read(path)
            .map_err(|error| {
                io::Error::other(format!(
                    "failed to read managed inference encryption key: {error}"
                ))
            })
            .and_then(|mut bytes| {
                let cipher = ManagedInferenceKeyCipher::from_bytes(&bytes).map_err(|error| {
                    io::Error::other(format!(
                        "managed inference encryption key is invalid: {error}"
                    ))
                });
                bytes.fill(0);
                cipher
            })
            .map(Some),
    }
}

fn core_only_configuration() -> Result<(), io::Error> {
    if execution_enabled()? {
        return Err(io::Error::other(
            "core-only mode requires STEWARD_EXECUTION_ENABLED=false",
        ));
    }
    if task_orchestration_mode()?.is_active() {
        return Err(io::Error::other(
            "core-only mode requires staged Task orchestration",
        ));
    }
    if execution_bindings_active().map_err(io::Error::other)? {
        return Err(io::Error::other(
            "core-only mode requires staged execution bindings",
        ));
    }
    Ok(())
}

fn jira_adapter() -> Result<JiraAdapter, io::Error> {
    let optional = |name| match env::var(name) {
        Ok(value) => Ok(value),
        Err(env::VarError::NotPresent) => Ok(String::new()),
        Err(env::VarError::NotUnicode(_)) => {
            Err(io::Error::other(format!("{name} must be Unicode")))
        }
    };
    JiraAdapter::new(
        JiraConfig {
            base_url: optional("STEWARD_JIRA_BASE_URL")?,
            project_key: optional("STEWARD_JIRA_PROJECT_KEY")?,
            account_email: optional("STEWARD_JIRA_ACCOUNT_EMAIL")?,
        },
        optional("STEWARD_JIRA_TOKEN")?,
    )
    .map_err(|error| io::Error::other(format!("Jira configuration failed: {error:?}")))
}

fn configured_execution_bindings_json() -> Result<Option<String>, io::Error> {
    let inline = match env::var("STEWARD_TASK_EXECUTION_BINDINGS_JSON") {
        Ok(value) => Some(value),
        Err(env::VarError::NotPresent) => None,
        Err(env::VarError::NotUnicode(_)) => {
            return Err(io::Error::other(
                "STEWARD_TASK_EXECUTION_BINDINGS_JSON must be Unicode",
            ));
        }
    };
    let file = match env::var("STEWARD_TASK_EXECUTION_BINDINGS_FILE") {
        Ok(value) => Some(value),
        Err(env::VarError::NotPresent) => None,
        Err(env::VarError::NotUnicode(_)) => {
            return Err(io::Error::other(
                "STEWARD_TASK_EXECUTION_BINDINGS_FILE must be Unicode",
            ));
        }
    };
    match (inline, file) {
        (Some(_), Some(_)) => Err(io::Error::other(
            "configure exactly one of STEWARD_TASK_EXECUTION_BINDINGS_JSON or STEWARD_TASK_EXECUTION_BINDINGS_FILE",
        )),
        (Some(value), None) => Ok(Some(value)),
        (None, Some(path)) if path.is_empty() => Err(io::Error::other(
            "STEWARD_TASK_EXECUTION_BINDINGS_FILE must be a non-empty path",
        )),
        (None, Some(path)) => read_execution_binding_catalog(&path).map(Some),
        (None, None) => Ok(None),
    }
}

fn with_claude_code_execution_adapter(
    config: TaskApiConfig,
    inference_endpoint: Option<String>,
) -> Result<TaskApiConfig, io::Error> {
    let Some(inference_endpoint) = inference_endpoint.filter(|value| !value.is_empty()) else {
        return Ok(config);
    };
    let adapter = ClaudeCodeTaskExecutionAdapter::new(inference_endpoint).map_err(|error| {
        io::Error::other(format!(
            "Claude Code execution adapter configuration failed: {error:?}"
        ))
    })?;
    config
        .with_execution_adapter(Arc::new(adapter))
        .map_err(io::Error::other)
}

fn configured_source_repository_bindings_json() -> Result<Option<String>, io::Error> {
    configured_bounded_json(
        "STEWARD_SOURCE_REPOSITORY_BINDINGS_JSON",
        "STEWARD_SOURCE_REPOSITORY_BINDINGS_FILE",
        MAX_SOURCE_REPOSITORY_BINDINGS_BYTES,
        "source repository binding catalog",
    )
}

fn configured_bounded_json(
    inline_name: &str,
    file_name: &str,
    max_bytes: usize,
    description: &str,
) -> Result<Option<String>, io::Error> {
    let inline = match env::var(inline_name) {
        Ok(value) => Some(value),
        Err(env::VarError::NotPresent) => None,
        Err(env::VarError::NotUnicode(_)) => {
            return Err(io::Error::other(format!("{inline_name} must be Unicode")));
        }
    };
    let file = match env::var(file_name) {
        Ok(value) => Some(value),
        Err(env::VarError::NotPresent) => None,
        Err(env::VarError::NotUnicode(_)) => {
            return Err(io::Error::other(format!("{file_name} must be Unicode")));
        }
    };
    match (inline, file) {
        (Some(_), Some(_)) => Err(io::Error::other(format!(
            "configure exactly one of {inline_name} or {file_name}"
        ))),
        (Some(value), None) if value.len() > max_bytes => Err(io::Error::other(format!(
            "{description} exceeds {max_bytes} bytes"
        ))),
        (Some(value), None) => Ok(Some(value)),
        (None, Some(path)) if path.is_empty() => Err(io::Error::other(format!(
            "{file_name} must be a non-empty path"
        ))),
        (None, Some(path)) => {
            let metadata = fs::metadata(&path)
                .map_err(|error| io::Error::other(format!("read {description} {path}: {error}")))?;
            if metadata.len() > max_bytes as u64 {
                return Err(io::Error::other(format!(
                    "{description} exceeds {max_bytes} bytes"
                )));
            }
            fs::read_to_string(&path)
                .map(Some)
                .map_err(|error| io::Error::other(format!("read {description} {path}: {error}")))
        }
        (None, None) => Ok(None),
    }
}

fn configured_github_source_adapter() -> Result<Option<GitHubSourceAdapter>, io::Error> {
    let app_id = optional_unicode_environment("STEWARD_GITHUB_SOURCE_APP_ID")?;
    let private_key_file = optional_unicode_environment("STEWARD_GITHUB_SOURCE_PRIVATE_KEY_FILE")?;
    github_source_adapter_from_values(app_id, private_key_file)
}

fn github_source_adapter_from_values(
    app_id: Option<String>,
    private_key_file: Option<String>,
) -> Result<Option<GitHubSourceAdapter>, io::Error> {
    let (app_id, private_key_file) = match (app_id, private_key_file) {
        (None, None) => return Ok(None),
        (Some(app_id), Some(private_key_file)) => (app_id, private_key_file),
        _ => {
            return Err(io::Error::other(
                "STEWARD_GITHUB_SOURCE_APP_ID and STEWARD_GITHUB_SOURCE_PRIVATE_KEY_FILE must be configured together",
            ));
        }
    };
    if private_key_file.is_empty() {
        return Err(io::Error::other(
            "STEWARD_GITHUB_SOURCE_PRIVATE_KEY_FILE must be a non-empty path",
        ));
    }
    let app_id = app_id
        .parse::<u64>()
        .map_err(|_| io::Error::other("STEWARD_GITHUB_SOURCE_APP_ID must be a positive integer"))?;
    let metadata = fs::metadata(&private_key_file).map_err(|error| {
        io::Error::other(format!("read GitHub source App private key: {error}"))
    })?;
    if metadata.len() == 0 || metadata.len() > 64 * 1024 {
        return Err(io::Error::other(
            "GitHub source App private key must contain at most 65536 bytes",
        ));
    }
    let private_key = fs::read(&private_key_file).map_err(|error| {
        io::Error::other(format!("read GitHub source App private key: {error}"))
    })?;
    let credentials = GitHubAppCredentials::new(app_id, private_key)
        .map_err(|_| io::Error::other("GitHub source App credentials are invalid"))?;
    GitHubSourceAdapter::new(credentials)
        .map(Some)
        .map_err(|_| io::Error::other("GitHub source adapter configuration is invalid"))
}

fn optional_unicode_environment(name: &str) -> Result<Option<String>, io::Error> {
    match env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => {
            Err(io::Error::other(format!("{name} must be Unicode")))
        }
    }
}

fn execution_bindings_active() -> Result<bool, String> {
    match env::var("STEWARD_TASK_EXECUTION_BINDINGS_MODE") {
        Ok(value) => parse_execution_bindings_mode(&value),
        Err(env::VarError::NotPresent) => Ok(false),
        Err(env::VarError::NotUnicode(_)) => {
            Err("STEWARD_TASK_EXECUTION_BINDINGS_MODE must be Unicode".to_owned())
        }
    }
}

fn parse_execution_bindings_mode(value: &str) -> Result<bool, String> {
    match value {
        "active" => Ok(true),
        "staged" => Ok(false),
        _ => Err("STEWARD_TASK_EXECUTION_BINDINGS_MODE must be staged or active".to_owned()),
    }
}

fn validate_execution_bindings(arguments: Vec<String>) -> Result<(), Box<dyn Error>> {
    let [flag, path] = arguments.as_slice() else {
        return Err(io::Error::other(
            "usage: steward-apiserver validate-execution-bindings --file <path>",
        )
        .into());
    };
    if flag != "--file" || path.is_empty() {
        return Err(io::Error::other(
            "usage: steward-apiserver validate-execution-bindings --file <path>",
        )
        .into());
    }
    let document = read_execution_binding_catalog(path)?;
    let catalog = ExecutionBindingCatalog::from_json(&document).map_err(io::Error::other)?;
    println!("{}", catalog.validation_report_json()?);
    Ok(())
}

fn read_execution_binding_catalog(path: &str) -> Result<String, io::Error> {
    let metadata = fs::metadata(path).map_err(|error| {
        io::Error::other(format!("read execution binding catalog {path}: {error}"))
    })?;
    if metadata.len() > MAX_EXECUTION_BINDING_CATALOG_BYTES as u64 {
        return Err(io::Error::other(
            "execution binding catalog exceeds 1048576 bytes",
        ));
    }
    fs::read_to_string(path).map_err(|error| {
        io::Error::other(format!("read execution binding catalog {path}: {error}"))
    })
}

struct ConfiguredTaskIdentity {
    resolver: ConfiguredTaskIdentityResolver,
    discovery: Option<TaskAuthDiscoveryConfig>,
    connection_auto_association_issuer: Option<String>,
    task_identity_issuer: Option<String>,
}

fn connection_auto_association_issuer(
    federated_subjects_enabled: bool,
    auto_associate_from_connections: bool,
    issuer: String,
) -> Option<String> {
    (federated_subjects_enabled && auto_associate_from_connections).then_some(issuer)
}

fn configured_task_identity_resolver(
    client: kube::Client,
    kubernetes_audience: KubernetesTokenReviewAudience,
    store: PgStore,
) -> Result<ConfiguredTaskIdentity, io::Error> {
    let values = [
        env::var("STEWARD_IDENTITY_TASK_ISSUER").ok(),
        env::var("STEWARD_IDENTITY_TASK_AUDIENCE").ok(),
        env::var("STEWARD_IDENTITY_TASK_JWKS_FILE").ok(),
    ];
    let resource = optional_unicode_environment("STEWARD_TASK_AUTH_RESOURCE")?;
    let federated_subjects_enabled = match env::var("STEWARD_FEDERATED_TASK_IDENTITY_ENABLED") {
        Ok(value) if value == "true" => true,
        Ok(value) if value == "false" => false,
        Err(env::VarError::NotPresent) => false,
        _ => {
            return Err(io::Error::other(
                "STEWARD_FEDERATED_TASK_IDENTITY_ENABLED must be true or false",
            ));
        }
    };
    let auto_associate_from_connections = match env::var(
        "STEWARD_FEDERATED_TASK_IDENTITY_AUTO_ASSOCIATE_FROM_CONNECTIONS",
    ) {
        Ok(value) if value == "true" => true,
        Ok(value) if value == "false" => false,
        Err(env::VarError::NotPresent) => true,
        _ => {
            return Err(io::Error::other(
                "STEWARD_FEDERATED_TASK_IDENTITY_AUTO_ASSOCIATE_FROM_CONNECTIONS must be true or false",
            ));
        }
    };
    if values.iter().all(Option::is_none) {
        if resource.is_some() || federated_subjects_enabled {
            return Err(io::Error::other(
                "task auth discovery and federated identity require Identity task authentication",
            ));
        }
        return Ok(ConfiguredTaskIdentity {
            resolver: ConfiguredTaskIdentityResolver::kubernetes(
                client,
                kubernetes_audience,
                store,
            ),
            discovery: None,
            connection_auto_association_issuer: None,
            task_identity_issuer: None,
        });
    }
    let [issuer, audience, jwks_file] = values;
    let required = |value: Option<String>| {
        value
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                io::Error::other("Identity task authentication configuration must be complete")
            })
    };
    let issuer = required(issuer)?;
    let discovery = match resource {
        Some(resource) => Some(
            TaskAuthDiscoveryConfig::new(resource, issuer.clone(), federated_subjects_enabled)
                .map_err(io::Error::other)?,
        ),
        None if federated_subjects_enabled => {
            return Err(io::Error::other(
                "federated task identity requires STEWARD_TASK_AUTH_RESOURCE",
            ));
        }
        None => None,
    };
    let resolver = ConfiguredTaskIdentityResolver::identity_from_jwks_file(
        issuer.clone(),
        required(audience)?,
        std::path::Path::new(&required(jwks_file)?),
        store,
        federated_subjects_enabled,
    )
    .map_err(|_| io::Error::other("Identity task authentication configuration is invalid"))?;
    Ok(ConfiguredTaskIdentity {
        resolver,
        discovery,
        connection_auto_association_issuer: connection_auto_association_issuer(
            federated_subjects_enabled,
            auto_associate_from_connections,
            issuer.clone(),
        ),
        task_identity_issuer: Some(issuer),
    })
}

fn install_rustls_crypto_provider() -> Result<(), io::Error> {
    use tokio_rustls::rustls::crypto::{CryptoProvider, ring};

    if CryptoProvider::get_default().is_none() {
        let _ = ring::default_provider().install_default();
    }
    if CryptoProvider::get_default().is_some() {
        Ok(())
    } else {
        Err(io::Error::other(
            "install the Steward Rustls crypto provider",
        ))
    }
}

fn parse_custom_envelope_safety_ceiling(
    document: &str,
    capability_catalog: &browser_admin::CapabilityCatalog,
) -> Result<steward_admission::Envelope, io::Error> {
    let ceiling = serde_json::from_str::<user_envelopes::BrowserEnvelope>(document)
        .map(Into::<steward_admission::Envelope>::into)
        .map_err(|_| io::Error::other("custom Envelope safety ceiling is invalid JSON"))?;
    if ceiling.revision <= 0
        || ceiling.spec.runtime_minutes_limit.is_none()
        || steward_admission::validate_envelope(&ceiling).is_err()
    {
        return Err(io::Error::other(
            "custom Envelope safety ceiling is structurally invalid or lacks a runtime-minutes limit",
        ));
    }
    if ceiling
        .spec
        .llms
        .iter()
        .any(|model| !capability_catalog.models.contains(model))
        || ceiling.spec.tools.iter().any(|tool| {
            !capability_catalog
                .tools
                .iter()
                .any(|available| available.grants(tool))
        })
    {
        return Err(io::Error::other(
            "custom Envelope safety ceiling selects an unavailable capability",
        ));
    }
    Ok(ceiling)
}

struct BrowserApplicationConfig {
    task_orchestration_mode: TaskOrchestrationMode,
    connection_auto_association_issuer: Option<String>,
    task_identity_discovery_enabled: bool,
    task_execution_bindings_active: bool,
    github_source_enabled: bool,
    github_actor_issuer: Option<String>,
    inference_mode: InferenceMode,
    managed_inference_cipher: Option<ManagedInferenceKeyCipher>,
}

async fn browser_application_router(
    store: PgStore,
    runtimes: KubeRuntimeRepository,
    decisions: JiraAdapter,
    workflow_agents: Vec<steward_apiserver::ExecutionBindingAdvertisement>,
    task_api_config: TaskApiConfig,
    application_config: BrowserApplicationConfig,
) -> Result<Option<axum::Router>, Box<dyn Error>> {
    let Ok(client_id) = env::var("STEWARD_GOOGLE_OIDC_CLIENT_ID") else {
        return Ok(None);
    };
    let capability_catalog_json = configured_bounded_json(
        "STEWARD_CAPABILITY_CATALOG_JSON",
        "STEWARD_CAPABILITY_CATALOG_FILE",
        browser_admin::MAX_CAPABILITY_CATALOG_BYTES,
        "capability catalog",
    )?
    .ok_or_else(|| io::Error::other("browser administration requires a capability catalog"))?;
    let capability_catalog = browser_admin::CapabilityCatalog::from_json(&capability_catalog_json)
        .map_err(io::Error::other)?;
    let admin_setup_config = steward_apiserver::admin_setup::AdminSetupConfig {
        orchestration_active: application_config.task_orchestration_mode.is_active(),
        execution_bindings_active: application_config.task_execution_bindings_active,
        resolvable_execution_bindings: workflow_agents.len(),
        task_identity_discovery_enabled: application_config.task_identity_discovery_enabled,
        github_source_enabled: application_config.github_source_enabled,
        github_actor_issuer: application_config.github_actor_issuer,
        capability_catalog: capability_catalog.clone(),
    };
    let starter_task_json = configured_bounded_json(
        "STEWARD_STARTER_TASK_JSON",
        "STEWARD_STARTER_TASK_FILE",
        steward_apiserver::onboarding::MAX_STARTER_TASK_BYTES,
        "starter task setting",
    )?;
    let starter_task = steward_apiserver::onboarding::StarterTaskSetting::from_optional_json(
        starter_task_json.as_deref(),
        &workflow_agents,
    )
    .map_err(|error| io::Error::other(format!("starter task configuration failed: {error}")))?;
    let custom_envelope_safety_ceiling = configured_bounded_json(
        "STEWARD_CUSTOM_ENVELOPE_SAFETY_CEILING_JSON",
        "STEWARD_CUSTOM_ENVELOPE_SAFETY_CEILING_FILE",
        user_envelopes::MAX_CUSTOM_ENVELOPE_SAFETY_CEILING_BYTES,
        "custom Envelope safety ceiling",
    )?
    .map(|document| parse_custom_envelope_safety_ceiling(&document, &capability_catalog))
    .transpose()?;
    let steward_run_release = configured_bounded_json(
        "STEWARD_RUN_RELEASE_JSON",
        "STEWARD_RUN_RELEASE_FILE",
        steward_apiserver::MAX_STEWARD_RUN_RELEASE_BYTES,
        "steward-run release coordinates",
    )?
    .ok_or_else(|| {
        io::Error::other(
            "browser administration requires steward-run release coordinates from the installation BOM",
        )
    })
    .and_then(|document| {
        steward_apiserver::steward_run_release_from_installation_bom(&document)
            .map_err(io::Error::other)
    })?;
    let workflow_installation_mode = StewardRunWorkflowInstallationMode::parse(
        env::var("STEWARD_RUN_WORKFLOW_INSTALLATION_MODE")
            .ok()
            .as_deref(),
    )
    .map_err(io::Error::other)?;
    steward_apiserver::validate_steward_run_workflow_installation(
        workflow_installation_mode,
        &steward_run_release,
    )
    .map_err(io::Error::other)?;
    let origin = required("STEWARD_BROWSER_ORIGIN")?;
    let config = browser_auth::GoogleOidcConfig::new(
        client_id,
        &origin,
        format!("{origin}/admin/auth/callback"),
        required("STEWARD_GOOGLE_WORKSPACE_DOMAIN")?,
        OrganizationId::parse(required("STEWARD_ORGANIZATION_ID")?)?,
    )
    .map_err(io::Error::other)?;
    let provider = google_oidc::GoogleOidcProvider::new(
        config.clone(),
        required("STEWARD_GOOGLE_OIDC_CLIENT_SECRET")?,
    )
    .map_err(io::Error::other)?;
    let auth = browser_auth::BrowserAuthService::google(
        config,
        Arc::new(provider),
        Arc::new(browser_auth::PgBrowserIdentityResolver::new(store.clone())),
    )
    .map_err(io::Error::other)?;
    let connections = governed_connections_configuration(
        &origin,
        store.clone(),
        application_config.task_orchestration_mode,
        application_config.connection_auto_association_issuer,
    )?;
    let inference_connections = inference_connections::PgInferenceConnectionBroker::new(
        store.clone(),
        application_config.inference_mode,
        application_config.managed_inference_cipher,
    )
    .map_err(io::Error::other)?;
    if workflows::ensure_sample_workflow(&store, &workflow_agents)
        .await
        .map_err(|error| io::Error::other(format!("sample Workflow bootstrap failed: {error}")))?
        .is_none()
    {
        eprintln!(
            "warning: onboarding sample Workflow is unavailable because no configured execution binding advertises its immutable agent"
        );
    }
    let browser_task_rerunner = browser_task_rerunner(store.clone(), task_api_config.clone());
    let github_automation_config = github_automation::GithubAutomationConfig::new(
        task_api_config.clone(),
        steward_run_release.clone(),
        workflow_installation_mode,
        application_config.task_identity_discovery_enabled,
    );
    let app = browser_auth::browser_auth_router(auth.clone())
        .merge(user_envelopes::protected_router(
            user_envelopes::PgEnvelopeRequestBroker::new(
                store.clone(),
                capability_catalog.clone(),
                custom_envelope_safety_ceiling.clone(),
                steward_run_release,
                workflow_installation_mode,
                application_config.task_identity_discovery_enabled,
            ),
            workflow_agents.clone(),
            auth.clone(),
        ))
        .merge(steward_apiserver::preferences::protected_router(
            store.clone(),
            auth.clone(),
        ))
        .merge(steward_apiserver::onboarding::protected_router(
            starter_task,
            auth.clone(),
        ))
        .merge(browser_admin::protected_router_with_custom_envelope_safety(
            runtimes.clone(),
            store.clone(),
            decisions,
            capability_catalog,
            custom_envelope_safety_ceiling,
            auth.clone(),
        ))
        .merge(browser_admin::protected_federated_subject_router(
            store.clone(),
            auth.clone(),
        ))
        .merge(steward_apiserver::browser_members::protected_router(
            store.clone(),
            auth.clone(),
        ))
        .merge(steward_apiserver::admin_setup::protected_router(
            store.clone(),
            admin_setup_config,
            auth.clone(),
        ))
        .merge(workflows::protected_admin_router_with_agents(
            store.clone(),
            auth.clone(),
            workflow_agents,
        ))
        .merge(browser_task_router(
            store.clone(),
            task_api_config,
            auth.clone(),
        ))
        .merge(inference_connections::protected_router(
            inference_connections,
            auth.clone(),
        ));
    let app = match connections {
        Some(broker) => app
            .merge(agent_runs_ui::protected_router_with_rerunners(
                store.clone(),
                broker.clone(),
                browser_task_rerunner,
                auth.clone(),
            ))
            .merge(github_automation::protected_router(
                store.clone(),
                broker.clone(),
                github_automation_config,
                auth.clone(),
            ))
            .merge(connections::protected_router(broker, auth.clone())),
        None => app.merge(agent_runs_ui::protected_router_with_task_reruns(
            store.clone(),
            browser_task_rerunner,
            auth.clone(),
        )),
    };
    let app = match stable_bridge_configuration()? {
        Some((service, verifier)) => app.merge(stable_runtime_bridge::protected_router(
            store, runtimes, verifier, auth, service,
        )),
        None => app,
    };
    Ok(Some(app))
}

type GovernedConnectionMutations =
    governed_connections::GovernedConnectionsBroker<browser_auth::BrowserSessionBinding>;
type GovernedConnectionsBroker = governed_connections::SplitConnectionsBroker<
    GovernedConnectionMutations,
    governed_connections::DirectConnectionStatusReader,
>;

fn governed_connections_configuration(
    browser_origin: &str,
    store: PgStore,
    task_orchestration_mode: TaskOrchestrationMode,
    connection_auto_association_issuer: Option<String>,
) -> Result<Option<GovernedConnectionsBroker>, io::Error> {
    let artifact_trust_mode = env::var("STEWARD_CONNECTIONS_BRIDGE_ARTIFACT_TRUST_MODE").ok();
    let values = [
        env::var("STEWARD_CONNECTIONS_BRIDGE_IMAGE").ok(),
        env::var("STEWARD_CONNECTIONS_MCP_GW_ORIGIN").ok(),
        env::var("STEWARD_CONNECTIONS_MCP_GW_VERSION").ok(),
        env::var("STEWARD_CONNECTIONS_RUNTIME_NAMESPACE").ok(),
    ];
    if artifact_trust_mode.is_none() && values.iter().all(Option::is_none) {
        return Ok(None);
    }
    let [
        bridge_image_digest,
        mcp_gw_origin,
        mcp_gw_version,
        namespace,
    ] = values;
    let required_connection = |value: Option<String>| {
        value
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| io::Error::other("governed Connections configuration must be complete"))
    };
    let bindings = governed_connections::ConnectionExecutionBindings {
        artifact_trust_mode: artifact_trust_mode
            .unwrap_or_else(|| governed_connections::GITHUB_ATTESTATION_TRUST_MODE.to_owned()),
        bridge_image_digest: required_connection(bridge_image_digest)?,
        mcp_gw_origin: required_connection(mcp_gw_origin)?,
        mcp_gw_version: required_connection(mcp_gw_version)?,
        namespace: required_connection(namespace)?,
        runtime_class: env::var("STEWARD_OPENSHELL_RUNTIME_CLASS_NAME")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_default(),
    };
    let config =
        governed_connections::GovernedConnectionsConfig::new(bindings.clone(), browser_origin)
            .map_err(|_| io::Error::other("governed Connections configuration is invalid"))?;
    let status = governed_connections::DirectConnectionStatusReader::new(
        store.clone(),
        governed_connections::DirectConnectionStatusConfig {
            control_plane_credential_file: PathBuf::from(required(
                "STEWARD_CONNECTIONS_CONTROL_PLANE_CREDENTIAL_FILE",
            )?),
            mint_origin: required("STEWARD_CONNECTIONS_MINT_ORIGIN")?,
            federated_subject_issuer: connection_auto_association_issuer,
        },
        &bindings.mcp_gw_origin,
        &bindings.mcp_gw_version,
    )
    .map_err(|_| io::Error::other("direct Connections status configuration is invalid"))?;
    let mutations = governed_connections::GovernedConnectionsBroker::new(
        store,
        config,
        task_orchestration_mode,
    );
    Ok(Some(governed_connections::SplitConnectionsBroker::new(
        mutations, status,
    )))
}

fn task_orchestration_mode() -> Result<TaskOrchestrationMode, io::Error> {
    let value = required("STEWARD_TASK_ORCHESTRATION_MODE")?;
    TaskOrchestrationMode::parse(&value)
        .map_err(|_| io::Error::other("STEWARD_TASK_ORCHESTRATION_MODE must be staged or active"))
}

fn stable_bridge_configuration()
-> Result<Option<(stable_runtime_bridge::BridgeService, GitHubArtifactVerifier)>, io::Error> {
    let image = env::var("STEWARD_STABLE_BRIDGE_IMAGE").ok();
    let signer_identity = env::var("STEWARD_STABLE_BRIDGE_SIGNER_IDENTITY").ok();
    let source_repository = env::var("STEWARD_STABLE_BRIDGE_SOURCE_REPOSITORY").ok();
    let source_commit = env::var("STEWARD_STABLE_BRIDGE_SOURCE_COMMIT").ok();
    let bundle_file = env::var("STEWARD_STABLE_BRIDGE_ATTESTATION_BUNDLE_FILE").ok();
    let service = env::var("STEWARD_STABLE_BRIDGE_SERVICE").ok();
    let bundle = bundle_file
        .as_deref()
        .map(fs::read_to_string)
        .transpose()
        .map_err(|_| io::Error::other("read stable bridge attestation bundle"))?;
    stable_bridge_configuration_from_values(
        image,
        signer_identity,
        source_repository,
        source_commit,
        bundle,
        service,
    )
}

fn stable_bridge_configuration_from_values(
    image: Option<String>,
    signer_identity: Option<String>,
    source_repository: Option<String>,
    source_commit: Option<String>,
    bundle: Option<String>,
    service: Option<String>,
) -> Result<Option<(stable_runtime_bridge::BridgeService, GitHubArtifactVerifier)>, io::Error> {
    if [
        image.as_ref(),
        signer_identity.as_ref(),
        source_repository.as_ref(),
        source_commit.as_ref(),
        bundle.as_ref(),
        service.as_ref(),
    ]
    .iter()
    .all(Option::is_none)
    {
        return Ok(None);
    }
    let image = image.ok_or_else(stable_bridge_configuration_error)?;
    let signer_identity = signer_identity.ok_or_else(stable_bridge_configuration_error)?;
    let source_repository = source_repository.ok_or_else(stable_bridge_configuration_error)?;
    let source_commit = source_commit.ok_or_else(stable_bridge_configuration_error)?;
    let bundle = bundle.ok_or_else(stable_bridge_configuration_error)?;
    let service = stable_runtime_bridge::BridgeService::new(
        service.ok_or_else(stable_bridge_configuration_error)?,
    )
    .map_err(io::Error::other)?;
    let verifier = GitHubArtifactVerifier::from_jsonl(
        image,
        signer_identity,
        source_repository,
        source_commit,
        &bundle,
    )
    .map_err(|_| io::Error::other("stable bridge provenance configuration is invalid"))?;
    Ok(Some((service, verifier)))
}

fn stable_bridge_configuration_error() -> io::Error {
    io::Error::other(
        "stable bridge configuration requires image, signer identity, source repository, source commit, attestation bundle file, and service together",
    )
}

async fn bootstrap_rbac(arguments: Vec<String>) -> Result<(), Box<dyn Error>> {
    let (user_id, assignment, actor) = bootstrap_rbac_arguments(arguments)?;
    let store = PgStore::connect(&required("STEWARD_DATABASE_URL")?).await?;
    store.migrate().await?;
    store
        .append_browser_rbac_assignment(BrowserRbacAssignmentChange {
            user_id: &user_id,
            assignment: &assignment,
            action: BrowserRbacAssignmentAction::Grant,
            actor: &actor,
        })
        .await?;
    println!("local browser RBAC grant recorded");
    Ok(())
}

#[derive(Default)]
struct OperatorOptions {
    values: BTreeMap<String, String>,
    json: bool,
}

fn operator_options(arguments: &[String]) -> Result<OperatorOptions, io::Error> {
    let mut options = OperatorOptions::default();
    let mut values = arguments.iter();
    while let Some(flag) = values.next() {
        if !flag.starts_with("--") {
            return Err(io::Error::other(format!("unexpected argument {flag}")));
        }
        let value = values
            .next()
            .ok_or_else(|| io::Error::other(format!("{flag} requires a value")))?;
        if flag == "--output" {
            if value != "json" || options.json {
                return Err(io::Error::other("--output accepts json exactly once"));
            }
            options.json = true;
        } else if options
            .values
            .insert(flag.trim_start_matches("--").to_owned(), value.clone())
            .is_some()
        {
            return Err(io::Error::other(format!("duplicate option {flag}")));
        }
    }
    Ok(options)
}

fn required_option<'a>(options: &'a OperatorOptions, name: &str) -> Result<&'a str, io::Error> {
    options
        .values
        .get(name)
        .map(String::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| io::Error::other(format!("--{name} is required")))
}

struct OperatorApiClient {
    base_url: Url,
    bearer_token: String,
    client: reqwest::Client,
}

impl OperatorApiClient {
    fn from_environment() -> Result<Self, Box<dyn Error>> {
        let base_url = Url::parse(&required("STEWARD_OPERATOR_API_URL")?)
            .map_err(|_| OperatorCommandError::Invalid("STEWARD_OPERATOR_API_URL is invalid"))?;
        if base_url.scheme() != "https"
            || base_url.host_str().is_none()
            || base_url.username() != ""
            || base_url.password().is_some()
            || base_url.path() != "/"
            || base_url.query().is_some()
            || base_url.fragment().is_some()
        {
            return Err(OperatorCommandError::Invalid(
                "STEWARD_OPERATOR_API_URL must be an HTTPS origin",
            )
            .into());
        }
        let token_file = required("STEWARD_OPERATOR_TOKEN_FILE")?;
        let bearer_token = fs::read_to_string(token_file).map_err(|_| {
            OperatorCommandError::Unavailable("operator bearer token file is unavailable")
        })?;
        let bearer_token = bearer_token.trim().to_owned();
        if bearer_token.is_empty()
            || bearer_token.len() > 16 * 1024
            || bearer_token.chars().any(char::is_whitespace)
        {
            return Err(
                OperatorCommandError::Invalid("operator bearer token file is invalid").into(),
            );
        }
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30));
        if let Some(ca_file) = optional_unicode_environment("STEWARD_OPERATOR_CA_FILE")? {
            let pem = fs::read(ca_file).map_err(|_| {
                OperatorCommandError::Unavailable("operator CA file is unavailable")
            })?;
            let certificate = reqwest::Certificate::from_pem(&pem)
                .map_err(|_| OperatorCommandError::Invalid("operator CA file is invalid"))?;
            builder = builder.add_root_certificate(certificate);
        }
        let client = builder.build().map_err(|_| {
            OperatorCommandError::Unavailable("operator HTTP client is unavailable")
        })?;
        Ok(Self {
            base_url,
            bearer_token,
            client,
        })
    }

    fn endpoint(&self, segments: &[&str]) -> Result<Url, Box<dyn Error>> {
        let mut url = self.base_url.clone();
        {
            let mut path = url.path_segments_mut().map_err(|_| {
                OperatorCommandError::Invalid("STEWARD_OPERATOR_API_URL cannot be a base URL")
            })?;
            path.pop_if_empty();
            for segment in ["admin", "operator", "v1"]
                .into_iter()
                .chain(segments.iter().copied())
            {
                path.push(segment);
            }
        }
        Ok(url)
    }

    async fn get<T: DeserializeOwned>(&self, segments: &[&str]) -> Result<T, Box<dyn Error>> {
        self.send::<(), T>(Method::GET, segments, None).await
    }

    async fn send<B: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        method: Method,
        segments: &[&str],
        body: Option<&B>,
    ) -> Result<T, Box<dyn Error>> {
        let mut request = self
            .client
            .request(method, self.endpoint(segments)?)
            .bearer_auth(&self.bearer_token);
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request
            .send()
            .await
            .map_err(|_| OperatorCommandError::Unavailable("operator API is unavailable"))?;
        match response.status() {
            status if status.is_success() => response.json().await.map_err(|_| {
                OperatorCommandError::Unavailable("operator API returned an invalid response")
                    .into()
            }),
            HttpStatusCode::BAD_REQUEST | HttpStatusCode::UNPROCESSABLE_ENTITY => {
                Err(OperatorCommandError::Invalid("operator API rejected the request").into())
            }
            HttpStatusCode::UNAUTHORIZED | HttpStatusCode::FORBIDDEN => {
                Err(OperatorCommandError::Forbidden("operator authorization failed").into())
            }
            HttpStatusCode::NOT_FOUND => {
                Err(OperatorCommandError::NotFound("operator resource not found").into())
            }
            HttpStatusCode::CONFLICT => {
                Err(OperatorCommandError::Conflict("operator request conflicts").into())
            }
            _ => Err(OperatorCommandError::Unavailable("operator API is unavailable").into()),
        }
    }
}

async fn rbac_command(arguments: Vec<String>) -> Result<(), Box<dyn Error>> {
    let Some(command) = arguments.first().map(String::as_str) else {
        return Err(io::Error::other(rbac_usage()).into());
    };
    let client = OperatorApiClient::from_environment()?;
    match command {
        "users" => rbac_users(&client, &arguments[1..]).await,
        "roles" => rbac_roles(&client, &arguments[1..]).await,
        "grant" => rbac_mutation(&client, OperatorAssignmentAction::Grant, &arguments[1..]).await,
        "revoke" => rbac_mutation(&client, OperatorAssignmentAction::Revoke, &arguments[1..]).await,
        "effective-access" => rbac_effective_access(&client, &arguments[1..]).await,
        _ => Err(io::Error::other(rbac_usage()).into()),
    }
}

async fn rbac_users(
    client: &OperatorApiClient,
    arguments: &[String],
) -> Result<(), Box<dyn Error>> {
    let Some(command) = arguments.first().map(String::as_str) else {
        return Err(io::Error::other(rbac_usage()).into());
    };
    let options = operator_options(&arguments[1..])?;
    match command {
        "list" if options.values.is_empty() => {
            let users = client.get::<OperatorUsersResponse>(&["users"]).await?.users;
            if options.json {
                let rows = users
                    .iter()
                    .map(|user| {
                        serde_json::json!({
                            "userId": user.user_id,
                            "displayEmail": user.display_email,
                            "organizationId": user.organization_id,
                            "state": user.state,
                        })
                    })
                    .collect::<Vec<_>>();
                println!("{}", serde_json::to_string(&rows)?);
            } else {
                for user in users {
                    println!(
                        "{}\t{}\t{}\t{}",
                        user.user_id, user.display_email, user.organization_id, user.state
                    );
                }
            }
            Ok(())
        }
        "show" => {
            let user_id = CanonicalUserId::parse(required_option(&options, "user-id")?.to_owned())
                .map_err(io::Error::other)?;
            if options.values.len() != 1 {
                return Err(io::Error::other(rbac_usage()).into());
            }
            let user = client
                .get::<OperatorUserView>(&["users", user_id.as_str()])
                .await?;
            if options.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "userId": user.user_id,
                        "displayEmail": user.display_email,
                        "organizationId": user.organization_id,
                        "state": user.state,
                    })
                );
            } else {
                println!("user id: {}", user.user_id);
                println!("display email: {}", user.display_email);
                println!("organization id: {}", user.organization_id);
                println!("state: {}", user.state);
            }
            Ok(())
        }
        _ => Err(io::Error::other(rbac_usage()).into()),
    }
}

async fn rbac_roles(
    client: &OperatorApiClient,
    arguments: &[String],
) -> Result<(), Box<dyn Error>> {
    if arguments.first().map(String::as_str) != Some("list") {
        return Err(io::Error::other(rbac_usage()).into());
    }
    let options = operator_options(&arguments[1..])?;
    if !options.values.is_empty() {
        return Err(io::Error::other(rbac_usage()).into());
    }
    let roles = client
        .get::<OperatorRolesResponse>(&["roles"])
        .await?
        .member_roles;
    if options.json {
        println!("{}", serde_json::to_string(&roles)?);
    } else {
        for role in roles {
            println!("{role}");
        }
    }
    Ok(())
}

async fn rbac_mutation(
    client: &OperatorApiClient,
    action: OperatorAssignmentAction,
    arguments: &[String],
) -> Result<(), Box<dyn Error>> {
    let Some(kind) = arguments.first().map(String::as_str) else {
        return Err(io::Error::other(rbac_usage()).into());
    };
    let options = operator_options(&arguments[1..])?;
    let user_id = CanonicalUserId::parse(required_option(&options, "user-id")?.to_owned())
        .map_err(io::Error::other)?;
    let (assignment_kind, member_role) = match kind {
        "admin" if options.values.len() == 1 => (OperatorAssignmentKind::Administrator, None),
        "member-role" if options.values.len() == 2 => (
            OperatorAssignmentKind::MemberRole,
            Some(required_option(&options, "role")?.to_owned()),
        ),
        _ => return Err(io::Error::other(rbac_usage()).into()),
    };
    let response = client
        .send::<_, OperatorAssignmentResponse>(
            Method::POST,
            &["rbac"],
            Some(&OperatorAssignmentRequest {
                user_id: user_id.as_str().to_owned(),
                kind: assignment_kind,
                member_role: member_role.clone(),
                action,
            }),
        )
        .await?;
    if options.json {
        println!("{}", serde_json::to_string(&response)?);
    } else {
        println!("RBAC mutation recorded for {}", user_id.as_str());
    }
    Ok(())
}

async fn rbac_effective_access(
    client: &OperatorApiClient,
    arguments: &[String],
) -> Result<(), Box<dyn Error>> {
    let options = operator_options(arguments)?;
    let user_id = CanonicalUserId::parse(required_option(&options, "user-id")?.to_owned())
        .map_err(io::Error::other)?;
    if options.values.len() != 1 {
        return Err(io::Error::other(rbac_usage()).into());
    }
    let response = client
        .get::<OperatorEffectiveAccessResponse>(&["users", user_id.as_str(), "effective-access"])
        .await?;
    if options.json {
        println!("{}", serde_json::to_string(&response)?);
    } else {
        print!("{}", format_effective_access_human(&response));
    }
    Ok(())
}

fn format_effective_access_human(response: &OperatorEffectiveAccessResponse) -> String {
    let mut output = format!(
        "user id: {}\ndisplay email: {}\nadministrator: {}\nmember roles: {}\neligible templates: {}\n",
        response.user.user_id,
        response.user.display_email,
        response.administrator,
        response.member_roles.join(", "),
        response.eligible_templates.len(),
    );
    for template in &response.eligible_templates {
        output.push_str(&format!(
            "  {}@{}\n",
            template.template_id, template.revision
        ));
    }
    output.push_str(&format!(
        "active Envelopes: {}\n",
        response.active_envelopes.len()
    ));
    for envelope in &response.active_envelopes {
        output.push_str(&format!(
            "  {}\t{}\t{}@{}\n",
            envelope.envelope_instance_id,
            envelope.envelope_digest,
            envelope.template_id.as_deref().unwrap_or("custom"),
            envelope
                .template_revision
                .map_or_else(|| "-".to_owned(), |revision| revision.to_string())
        ));
    }
    output
}

fn rbac_usage() -> &'static str {
    "usage: steward rbac users list [--output json] | users show --user-id <id> [--output json] | roles list [--output json] | grant|revoke admin --user-id <id> [--output json] | grant|revoke member-role --user-id <id> --role <role> [--output json] | effective-access --user-id <id> [--output json]"
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TemplateDocument {
    template_id: String,
    display_name: String,
    member_roles: Vec<String>,
    ceiling: user_envelopes::BrowserEnvelope,
    auto_provision_threshold: Option<user_envelopes::BrowserEnvelope>,
}

fn parse_template_document(bytes: &[u8]) -> Result<TemplateDocument, io::Error> {
    const MAX_TEMPLATE_DOCUMENT_BYTES: usize = 256 * 1024;
    if bytes.len() > MAX_TEMPLATE_DOCUMENT_BYTES {
        return Err(io::Error::other("template document exceeds 256 KiB"));
    }
    let document = std::str::from_utf8(bytes)
        .map_err(|_| io::Error::other("template document must be UTF-8"))?;
    let options = serde_saphyr::options! {
        budget: serde_saphyr::budget! {
            max_events: 10_000,
            max_aliases: 0,
            max_anchors: 0,
            max_depth: 32,
            max_inclusion_depth: 0,
            max_documents: 1,
            max_nodes: 5_000,
            max_total_scalar_bytes: MAX_TEMPLATE_DOCUMENT_BYTES,
            max_total_comment_bytes: 32 * 1024,
            max_merge_keys: 0,
        },
        duplicate_keys: serde_saphyr::DuplicateKeyPolicy::Error,
        merge_keys: serde_saphyr::MergeKeyPolicy::Error,
        alias_limits: serde_saphyr::alias_limits! {
            max_total_replayed_events: 0,
            max_replay_stack_depth: 0,
            max_alias_expansions_per_anchor: 0,
        },
        strict_booleans: true,
        require_indent: serde_saphyr::RequireIndent::Even,
    };
    serde_saphyr::from_str_with_options(document, options)
        .map_err(|_| io::Error::other("template document must be valid strict YAML or JSON"))
}

async fn ensure_default_llm_template(store: &PgStore) -> Result<(), Box<dyn Error>> {
    let Some(document) = optional_unicode_environment("STEWARD_DEFAULT_LLM_TEMPLATE_JSON")? else {
        return Ok(());
    };
    let mut document: TemplateDocument = serde_json::from_str(&document)?;
    document.member_roles.sort();
    if document
        .member_roles
        .windows(2)
        .any(|roles| roles[0] == roles[1])
    {
        return Err(io::Error::other("template member roles must be unique").into());
    }
    let ceiling: steward_admission::Envelope = document.ceiling.into();
    let capability_catalog_json = configured_bounded_json(
        "STEWARD_CAPABILITY_CATALOG_JSON",
        "STEWARD_CAPABILITY_CATALOG_FILE",
        browser_admin::MAX_CAPABILITY_CATALOG_BYTES,
        "capability catalog",
    )?
    .ok_or_else(|| {
        io::Error::other("default LLM smoke template requires the capability catalog")
    })?;
    let capability_catalog = browser_admin::CapabilityCatalog::from_json(&capability_catalog_json)
        .map_err(io::Error::other)?;
    if !ceiling.spec.tools.is_empty()
        || ceiling.spec.llms.len() != 1
        || !capability_catalog.models.contains(&ceiling.spec.llms[0])
    {
        return Err(io::Error::other(
            "default LLM smoke template requires one exact available model and empty tools",
        )
        .into());
    }
    if document.auto_provision_threshold.is_some() {
        return Err(io::Error::other(
            "default LLM smoke template threshold is fixed to its ceiling",
        )
        .into());
    }
    if let Some(existing) = store
        .envelope_template_revision(&document.template_id, ceiling.revision)
        .await?
    {
        if existing.display_name != document.display_name
            || existing.member_roles != document.member_roles
            || existing.ceiling != ceiling
            || existing.auto_provision_threshold.as_ref() != Some(&ceiling)
        {
            return Err(io::Error::other(
                "default LLM smoke template revision exists with different content",
            )
            .into());
        }
        return Ok(());
    }
    store
        .insert_envelope_template_revision(EnvelopeTemplatePublication {
            template_id: &document.template_id,
            display_name: &document.display_name,
            member_roles: &document.member_roles,
            ceiling: &ceiling,
            auto_provision_threshold: Some(&ceiling),
            allow_inline_browser_tasks: true,
            authored_by: "system:install",
        })
        .await?;
    Ok(())
}

async fn templates_command(arguments: Vec<String>) -> Result<(), Box<dyn Error>> {
    if arguments.first().map(String::as_str) != Some("apply") {
        return Err(io::Error::other(
            "usage: steward templates apply --file <path> [--output json]",
        )
        .into());
    }
    let options = operator_options(&arguments[1..])?;
    if options.values.len() != 1 {
        return Err(io::Error::other(
            "usage: steward templates apply --file <path> [--output json]",
        )
        .into());
    }
    let path = required_option(&options, "file")?;
    let mut document = parse_template_document(&fs::read(path)?)?;
    document.member_roles.sort();
    if document
        .member_roles
        .windows(2)
        .any(|roles| roles[0] == roles[1])
    {
        return Err(io::Error::other("template member roles must be unique").into());
    }
    let revision = document.ceiling.revision;
    let client = OperatorApiClient::from_environment()?;
    let response = client
        .send::<_, OperatorTemplateResponse>(
            Method::PUT,
            &[
                "templates",
                &document.template_id,
                "revisions",
                &revision.to_string(),
            ],
            Some(&OperatorTemplateApplyRequest {
                display_name: document.display_name,
                member_roles: document.member_roles,
                ceiling: document.ceiling,
                auto_provision_threshold: document.auto_provision_threshold,
                allow_inline_browser_tasks: true,
            }),
        )
        .await?;
    if options.json {
        println!("{}", serde_json::to_string(&response)?);
    } else {
        println!(
            "template {}@{} applied",
            response.template_id, response.ceiling.revision
        );
    }
    Ok(())
}

async fn envelopes_command(arguments: Vec<String>) -> Result<(), Box<dyn Error>> {
    if arguments.first().map(String::as_str) != Some("provision") {
        return Err(io::Error::other(
            "usage: steward envelopes provision --user-id <id> --template-id <id> [--template-revision <revision>] [--output json]",
        )
        .into());
    }
    let options = operator_options(&arguments[1..])?;
    if !matches!(options.values.len(), 2 | 3) {
        return Err(io::Error::other(
            "usage: steward envelopes provision --user-id <id> --template-id <id> [--template-revision <revision>] [--output json]",
        )
        .into());
    }
    let user_id = CanonicalUserId::parse(required_option(&options, "user-id")?.to_owned())
        .map_err(io::Error::other)?;
    let template_id = required_option(&options, "template-id")?;
    if !browser_admin::valid_template_identifier(template_id) {
        return Err(io::Error::other("--template-id must be a valid catalog identifier").into());
    }
    let client = OperatorApiClient::from_environment()?;
    let template = if let Some(revision) = options.values.get("template-revision") {
        let revision = revision
            .parse::<i64>()
            .map_err(|_| io::Error::other("--template-revision must be a positive integer"))?;
        if revision <= 0 {
            return Err(io::Error::other("--template-revision must be a positive integer").into());
        }
        client
            .get::<OperatorTemplateResponse>(&[
                "templates",
                template_id,
                "revisions",
                &revision.to_string(),
            ])
            .await?
    } else {
        client
            .get::<OperatorTemplateResponse>(&["templates", template_id])
            .await?
    };
    let idempotency_key = format!(
        "cli:{}:{}:{}",
        user_id.as_str(),
        template.template_id,
        template.ceiling.revision,
    );
    let provisioned = client
        .send::<_, OperatorProvisionResponse>(
            Method::POST,
            &["envelopes", "provision"],
            Some(&OperatorProvisionRequest {
                owner_user_id: user_id.as_str().to_owned(),
                template_id: template.template_id,
                template_revision: template.ceiling.revision,
                requested_envelope: template.ceiling,
                idempotency_key,
            }),
        )
        .await?;
    if options.json {
        println!("{}", serde_json::to_string(&provisioned)?);
    } else {
        println!(
            "Envelope {} provisioned for {}",
            provisioned.envelope_instance_id,
            user_id.as_str()
        );
    }
    Ok(())
}

fn bootstrap_rbac_arguments(
    arguments: Vec<String>,
) -> Result<(CanonicalUserId, BrowserRbacAssignment, String), io::Error> {
    let mut user_id = None;
    let mut grant = None;
    let mut actor = None;
    let mut values = arguments.into_iter();
    while let Some(flag) = values.next() {
        let value = values.next().ok_or_else(bootstrap_rbac_usage)?;
        match flag.as_str() {
            "--user-id" if user_id.is_none() => {
                user_id = Some(CanonicalUserId::parse(value).map_err(|_| bootstrap_rbac_usage())?)
            }
            "--grant" if grant.is_none() => {
                grant = Some(match value.as_str() {
                    "administrator" => BrowserRbacAssignment::Administrator,
                    _ => BrowserRbacAssignment::MemberRole(value),
                })
            }
            "--actor" if actor.is_none() && !value.trim().is_empty() => actor = Some(value),
            _ => return Err(bootstrap_rbac_usage()),
        }
    }
    match (user_id, grant, actor) {
        (Some(user_id), Some(grant), Some(actor)) => Ok((user_id, grant, actor)),
        _ => Err(bootstrap_rbac_usage()),
    }
}

fn bootstrap_rbac_usage() -> io::Error {
    io::Error::other(
        "usage: steward-apiserver-bin bootstrap-rbac --user-id usr_<opaque-id> --grant administrator|<member-role> --actor <audited-operator>",
    )
}

fn required(name: &str) -> Result<String, io::Error> {
    env::var(name).map_err(|_| io::Error::other(format!("{name} is required")))
}

fn kubernetes_token_review_audience(
    value: Option<String>,
) -> Result<KubernetesTokenReviewAudience, io::Error> {
    let value = value.ok_or_else(|| {
        io::Error::other("STEWARD_KUBERNETES_TOKEN_REVIEW_AUDIENCE is required and non-empty")
    })?;
    KubernetesTokenReviewAudience::new(value).map_err(|_| {
        io::Error::other("STEWARD_KUBERNETES_TOKEN_REVIEW_AUDIENCE is required and non-empty")
    })
}

async fn tls_listener(
    bind: &str,
    certificate_path: &str,
    private_key_path: &str,
) -> Result<TlsListener, Box<dyn Error>> {
    let (certificates, private_key) =
        decode_tls_material(fs::read(certificate_path)?, fs::read(private_key_path)?)?;
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificates, private_key)?;
    Ok(TlsListener {
        acceptor: TlsAcceptor::from(Arc::new(config)),
        handshakes: JoinSet::new(),
        listener: TcpListener::bind(bind).await?,
    })
}

fn decode_tls_material(
    certificate_bytes: Vec<u8>,
    private_key_bytes: Vec<u8>,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), Box<dyn Error>> {
    let certificates = if certificate_bytes.starts_with(b"-----BEGIN") {
        CertificateDer::pem_slice_iter(&certificate_bytes).collect::<Result<Vec<_>, _>>()?
    } else {
        vec![CertificateDer::from(certificate_bytes)]
    };
    if certificates.is_empty() {
        return Err(io::Error::other("TLS certificate file contains no certificates").into());
    }
    let private_key = if private_key_bytes.starts_with(b"-----BEGIN") {
        PrivateKeyDer::from_pem_slice(&private_key_bytes)?
    } else {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(private_key_bytes))
    };
    Ok((certificates, private_key))
}

struct TlsListener {
    acceptor: TlsAcceptor,
    handshakes: JoinSet<Option<TlsConnection>>,
    listener: TcpListener,
}

type TlsConnection = (TlsStream<TcpStream>, std::net::SocketAddr);
type JoinedHandshake = Option<Result<Option<TlsConnection>, JoinError>>;

fn completed_handshake(result: JoinedHandshake) -> Option<TlsConnection> {
    match result {
        Some(Ok(connection)) => connection,
        Some(Err(error)) => {
            eprintln!("apiserver TLS handshake task failed: {error}");
            None
        }
        None => None,
    }
}

impl Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = std::net::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            if self.handshakes.len() >= MAX_PENDING_TLS_HANDSHAKES {
                if let Some(connection) = completed_handshake(self.handshakes.join_next().await) {
                    return connection;
                }
                continue;
            }
            let has_pending_handshakes = !self.handshakes.is_empty();
            tokio::select! {
                accepted = self.listener.accept() => match accepted {
                    Ok((stream, address)) => {
                        let acceptor = self.acceptor.clone();
                        self.handshakes.spawn(async move {
                            match timeout(TLS_HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
                                Ok(Ok(stream)) => Some((stream, address)),
                                Ok(Err(error)) => {
                                    eprintln!("apiserver TLS handshake failed: {error}");
                                    None
                                }
                                Err(_) => {
                                    eprintln!("apiserver TLS handshake timed out");
                                    None
                                }
                            }
                        });
                    }
                    Err(error) => {
                        eprintln!("apiserver listener accept failed: {error}");
                        sleep(Duration::from_secs(1)).await;
                    }
                },
                completed = self.handshakes.join_next(), if has_pending_handshakes => {
                    if let Some(connection) = completed_handshake(completed) {
                        return connection;
                    }
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use axum::serve::Listener;
    use reqwest::{Method, Url};
    use steward_store::{BrowserRbacAssignment, StoreError};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::time::timeout;
    use tokio_rustls::TlsAcceptor;
    use tokio_rustls::rustls::ServerConfig;
    use tokio_rustls::rustls::server::ResolvesServerCertUsingSni;

    use super::{
        CodexTaskExecutionAdapter, KubernetesTokenReviewAudience, OPERATOR_EXIT_CONFLICT,
        OPERATOR_EXIT_FORBIDDEN, OPERATOR_EXIT_NOT_FOUND, OPERATOR_EXIT_UNAVAILABLE,
        OPERATOR_EXIT_USAGE, OperatorCommandError, TaskApiConfig, TlsListener,
        bootstrap_rbac_arguments, connection_auto_association_issuer, decode_tls_material,
        format_effective_access_human, github_source_adapter_from_values,
        install_rustls_crypto_provider, kubernetes_token_review_audience, operator_exit_code,
        parse_custom_envelope_safety_ceiling, parse_execution_bindings_mode,
        parse_template_document, stable_bridge_configuration_from_values,
        validate_execution_bindings, with_claude_code_execution_adapter,
    };

    static NEXT_PREFLIGHT_CONFIG_TEST_ID: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn connection_auto_association_requires_both_feature_switches() {
        let issuer = "https://identity.example.test".to_owned();

        assert_eq!(
            connection_auto_association_issuer(true, true, issuer.clone()),
            Some(issuer.clone())
        );
        assert_eq!(
            connection_auto_association_issuer(true, false, issuer.clone()),
            None
        );
        assert_eq!(
            connection_auto_association_issuer(false, true, issuer),
            None
        );
    }

    struct OwnedTestDirectory(std::path::PathBuf);

    impl Drop for OwnedTestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn effective_access_human_output_names_eligible_templates() {
        let output = format_effective_access_human(
            &steward_apiserver::operator_admin::OperatorEffectiveAccessResponse {
                user: steward_apiserver::operator_admin::OperatorUserView {
                    user_id: "usr_0123456789abcdef0123456789abcdef".to_owned(),
                    display_email: "alice@example.com".to_owned(),
                    organization_id: "example-org".to_owned(),
                    state: "active".to_owned(),
                },
                administrator: false,
                member_roles: vec!["engineer".to_owned()],
                eligible_templates: vec![
                    steward_apiserver::operator_admin::OperatorEligibleTemplateView {
                        template_id: "default".to_owned(),
                        revision: 3,
                    },
                ],
                active_envelopes: Vec::new(),
            },
        );

        assert!(output.contains("  default@3\n"));
    }

    #[test]
    fn custom_envelope_safety_ceiling_requires_bounded_runtime_minutes() {
        let catalog = steward_apiserver::browser_admin::CapabilityCatalog {
            schema_version: "steward.capability-catalog/v2".to_owned(),
            models: vec![steward_types::ModelRef {
                provider: "provider-a".to_owned(),
                model: "model-a".to_owned(),
            }],
            tools: Vec::new(),
            catalogs: Vec::new(),
        };
        let unbounded = r#"{"revision":1,"spec":{"llms":[{"provider":"provider-a","model":"model-a"}],"tools":[],"budget":{"monthlyLimit":"1.00","currency":"USD"},"ttl":"1h","runner":{}}}"#;
        assert!(parse_custom_envelope_safety_ceiling(unbounded, &catalog).is_err());
    }

    #[tokio::test]
    async fn operator_http_client_sends_the_bearer_to_the_versioned_path_and_maps_statuses()
    -> Result<(), String> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|error| format!("bind operator test server: {error}"))?;
        let address = listener
            .local_addr()
            .map_err(|error| format!("read operator test address: {error}"))?;
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, body) in [
                ("200 OK", r#"{"ok":true}"#),
                ("409 Conflict", r#"{"error":"conflict"}"#),
            ] {
                let (mut stream, _) = listener
                    .accept()
                    .await
                    .map_err(|error| format!("accept operator request: {error}"))?;
                let mut bytes = Vec::with_capacity(2048);
                tokio::time::timeout(Duration::from_secs(2), async {
                    while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                        if bytes.len() >= 8192 {
                            return Err("operator request headers exceed 8192 bytes".to_owned());
                        }
                        let mut chunk = [0_u8; 1024];
                        let count = stream
                            .read(&mut chunk)
                            .await
                            .map_err(|error| format!("read operator request: {error}"))?;
                        if count == 0 {
                            return Err("operator request ended before its headers".to_owned());
                        }
                        bytes.extend_from_slice(&chunk[..count]);
                    }
                    Ok::<_, String>(())
                })
                .await
                .map_err(|_| "operator request read timed out".to_owned())??;
                requests.push(String::from_utf8_lossy(&bytes).into_owned());
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .map_err(|error| format!("write operator response: {error}"))?;
            }
            Ok::<_, String>(requests)
        });
        let client = super::OperatorApiClient {
            base_url: Url::parse(&format!("http://{address}/"))
                .map_err(|error| format!("build operator test URL: {error}"))?,
            bearer_token: "test-operator-token".to_owned(),
            client: reqwest::Client::builder()
                .build()
                .map_err(|error| format!("build operator test client: {error}"))?,
        };
        let response = client
            .get::<serde_json::Value>(&["users", "usr_0123456789abcdef0123456789abcdef"])
            .await
            .map_err(|error| format!("operator client GET failed: {error}"))?;
        assert_eq!(response["ok"], true);
        let error = match client
            .send::<(), serde_json::Value>(Method::POST, &["rbac"], None)
            .await
        {
            Ok(_) => return Err("409 was not surfaced as an operator conflict".to_owned()),
            Err(error) => error,
        };
        assert_eq!(
            super::operator_exit_code(error.as_ref()),
            OPERATOR_EXIT_CONFLICT
        );
        let requests = server
            .await
            .map_err(|error| format!("join operator test server: {error}"))??;
        assert!(requests[0].starts_with(
            "GET /admin/operator/v1/users/usr_0123456789abcdef0123456789abcdef HTTP/1.1"
        ));
        assert!(
            requests
                .iter()
                .all(|request| request.contains("authorization: Bearer test-operator-token"))
        );
        assert!(requests[1].starts_with("POST /admin/operator/v1/rbac HTTP/1.1"));
        Ok(())
    }

    #[test]
    fn operator_commands_publish_stable_exit_code_classes() {
        assert_eq!(
            operator_exit_code(&OperatorCommandError::Invalid("invalid")),
            OPERATOR_EXIT_USAGE
        );
        assert_eq!(
            operator_exit_code(&OperatorCommandError::NotFound("missing")),
            OPERATOR_EXIT_NOT_FOUND
        );
        assert_eq!(
            operator_exit_code(&OperatorCommandError::Forbidden("forbidden")),
            OPERATOR_EXIT_FORBIDDEN
        );
        assert_eq!(
            operator_exit_code(&OperatorCommandError::Conflict("conflict")),
            OPERATOR_EXIT_CONFLICT
        );
        assert_eq!(
            operator_exit_code(&OperatorCommandError::Unavailable("unavailable")),
            OPERATOR_EXIT_UNAVAILABLE
        );
        assert_eq!(
            operator_exit_code(&StoreError::CanonicalIdentityInactive),
            OPERATOR_EXIT_FORBIDDEN
        );
        assert_eq!(
            operator_exit_code(&StoreError::EnvelopeRequestDigestConflict),
            OPERATOR_EXIT_CONFLICT
        );
        assert_eq!(
            operator_exit_code(&StoreError::InvalidEnvelopeTemplate),
            OPERATOR_EXIT_USAGE
        );
        assert_eq!(
            operator_exit_code(&StoreError::Database("unavailable".to_owned())),
            OPERATOR_EXIT_UNAVAILABLE
        );
    }

    #[test]
    fn template_apply_accepts_strict_yaml_and_rejects_duplicate_keys() -> Result<(), String> {
        let document = parse_template_document(
            br#"templateId: default
displayName: Default smoke
memberRoles:
  - engineer
ceiling:
  revision: 1
  spec:
    llms:
      - provider: provider-a
        model: model-a
    tools: []
    budget:
      monthlyLimit: "1.00"
      currency: USD
    ttl: 10m
autoProvisionThreshold: null
"#,
        )
        .map_err(|error| error.to_string())?;
        assert_eq!(document.template_id, "default");
        assert_eq!(document.member_roles, ["engineer"]);
        assert!(
            parse_template_document(
                b"templateId: default\ntemplateId: duplicate\ndisplayName: Default\nmemberRoles: []\n"
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn released_validator_accepts_the_documented_catalog_example() -> Result<(), String> {
        let example = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/installation/execution-bindings.example.json");
        validate_execution_bindings(vec![
            "--file".to_owned(),
            example.to_string_lossy().into_owned(),
        ])
        .map_err(|error| error.to_string())
    }

    #[test]
    fn configured_claude_adapter_activates_both_supported_catalog_contracts() -> Result<(), String>
    {
        let catalog = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../charts/steward/testdata/execution-bindings/valid.json"),
        )
        .map_err(|error| format!("read execution binding fixture: {error}"))?;
        let codex = CodexTaskExecutionAdapter::new("https://inference.example.test/v1".to_owned())
            .map_err(|error| format!("configure Codex adapter: {error:?}"))?;
        let config = TaskApiConfig::default()
            .with_execution_adapter(Arc::new(codex))
            .map_err(|error| format!("register Codex adapter: {error}"))?;
        let config = with_claude_code_execution_adapter(
            config,
            Some("https://inference.example.test".to_owned()),
        )
        .map_err(|error| error.to_string())?
        .with_execution_bindings_json(Some(&catalog))?
        .with_execution_bindings_active(true)?;
        assert_eq!(
            config.execution_binding_refs(),
            ["claude-code@2.1.222", "codex@1.2.3"]
        );

        let codex = CodexTaskExecutionAdapter::new("https://inference.example.test/v1".to_owned())
            .map_err(|error| format!("configure Codex adapter: {error:?}"))?;
        let unavailable = TaskApiConfig::default()
            .with_execution_adapter(Arc::new(codex))?
            .with_execution_bindings_json(Some(&catalog))?
            .with_execution_bindings_active(true);
        assert!(
            unavailable.is_err_and(|reason| reason.contains("claude-code-v1")),
            "an active Claude binding must fail startup when its endpoint-backed adapter is absent"
        );
        Ok(())
    }

    #[test]
    fn empty_claude_endpoint_is_absent_until_a_claude_binding_is_activated() -> Result<(), String> {
        let codex_catalog = r#"{
          "apiVersion": "steward.execution-bindings/v1",
          "bindings": [{
            "agentRef": "codex@1.2.3",
            "displayName": "Codex",
            "adapter": "codex-v1",
            "image": "registry.example.test/agents/codex@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "executable": "/usr/bin/codex",
            "versionProbe": {"arguments": ["--version"], "expectedStdout": "codex 1.2.3"},
            "providerProfiles": {}
          }]
        }"#;
        let codex = CodexTaskExecutionAdapter::new("https://inference.example.test/v1".to_owned())
            .map_err(|error| format!("configure Codex adapter: {error:?}"))?;
        let configured = with_claude_code_execution_adapter(
            TaskApiConfig::default().with_execution_adapter(Arc::new(codex))?,
            Some(String::new()),
        )
        .map_err(|error| error.to_string())?
        .with_execution_bindings_json(Some(codex_catalog))?
        .with_execution_bindings_active(true)?;

        assert_eq!(configured.execution_binding_refs(), ["codex@1.2.3"]);
        Ok(())
    }

    #[test]
    fn generated_preflight_values_pass_apiserver_execution_config_validation() -> Result<(), String>
    {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let directory = OwnedTestDirectory(std::env::temp_dir().join(format!(
            "steward-preflight-apiserver-config-{}-{}",
            std::process::id(),
            NEXT_PREFLIGHT_CONFIG_TEST_ID.fetch_add(1, Ordering::Relaxed),
        )));
        let bundle = directory.0.join("provider-profile-bundle/v1.2.2");
        let installer = bundle.join("bin/steward-provider-profile");
        let output = directory.0.join("rendered");
        fs::create_dir_all(
            installer
                .parent()
                .ok_or_else(|| "installer has no parent".to_owned())?,
        )
        .map_err(|error| format!("create preflight fixture: {error}"))?;
        fs::write(
            &installer,
            r##"#!/bin/sh
set -eu
if [ "$1" = "validate" ]; then
  printf '%s\n' '{"schemaVersion":"steward.provider-profile-result/v1","operation":"validate","status":"valid","bundle":{"id":"steward-runtime-providers","version":"1.2.2"},"profiles":[{"id":"steward-litellm","digest":"sha256:7777777777777777777777777777777777777777777777777777777777777777"},{"id":"steward-mcp-gw","digest":"sha256:8888888888888888888888888888888888888888888888888888888888888888"}]}'
else
  printf '%s\n' '{"result":{"schemaVersion":"steward.provider-profile-result/v1","operation":"render","status":"valid"},"installation":{"schema":"steward.provider-profile-install-state/v1","bundle":{"id":"steward-runtime-providers","version":"1.2.2"},"profiles":{"steward-litellm":{"endpoints":[{"host":"inference.example.test","port":443,"allowed_ips":["192.0.2.0/24"],"access":"read-write"}],"credentials":[{"token_grant":{"audience":"steward-mcp"}}],"binaries":["/usr/bin/curl"]},"steward-mcp-gw":{"endpoints":[{"host":"mcp.example.test","port":443,"allowed_ips":["192.0.2.0/24"],"rules":[{"allow":{"method":"GET","path":"**"}},{"allow":{"method":"HEAD","path":"**"}},{"allow":{"method":"OPTIONS","path":"**"}},{"allow":{"method":"POST","path":"**"}},{"allow":{"method":"PUT","path":"**"}},{"allow":{"method":"PATCH","path":"**"}},{"allow":{"method":"DELETE","path":"**"}}]}],"credentials":[{"token_grant":{"audience":"steward-mcp"}}],"binaries":["/usr/bin/curl","/usr/local/bin/steward-connections-bridge"]}}}}'
fi
"##,
        )
        .map_err(|error| format!("write preflight fixture: {error}"))?;
        let mut permissions = fs::metadata(&installer)
            .map_err(|error| format!("read preflight fixture permissions: {error}"))?
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&installer, permissions)
            .map_err(|error| format!("make preflight fixture executable: {error}"))?;

        let generated = Command::new("python3")
            .arg(root.join("scripts/steward-platform-preflight.py"))
            .args(["generate", "--input"])
            .arg(root.join("config/platform-preflight/v1/examples/compact.json"))
            .arg("--provider-profile-bundle")
            .arg(&bundle)
            .arg("--output")
            .arg(&output)
            .output()
            .map_err(|error| format!("run platform preflight: {error}"))?;
        if !generated.status.success() {
            return Err(format!(
                "platform preflight failed: {}",
                String::from_utf8_lossy(&generated.stderr)
            ));
        }
        let values: serde_json::Value = serde_json::from_slice(
            &fs::read(output.join("steward-values.json"))
                .map_err(|error| format!("read generated Steward values: {error}"))?,
        )
        .map_err(|error| format!("parse generated Steward values: {error}"))?;
        let apiserver = values
            .pointer("/config/apiserver")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| "generated values require config.apiserver".to_owned())?;
        let inference_endpoint = apiserver
            .get("inferenceEndpoint")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "generated values require inferenceEndpoint".to_owned())?;
        let anthropic_endpoint = apiserver
            .get("anthropicInferenceEndpoint")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let bindings_value = apiserver
            .get("executionBindings")
            .ok_or_else(|| "generated values require executionBindings".to_owned())?;
        let expected_agent_ref = bindings_value
            .pointer("/bindings/0/agentRef")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "generated execution binding requires an agentRef".to_owned())?
            .to_owned();
        let bindings = bindings_value.to_string();
        let codex = CodexTaskExecutionAdapter::new(inference_endpoint.to_owned())
            .map_err(|error| format!("configure generated Codex adapter: {error:?}"))?;
        let configured = with_claude_code_execution_adapter(
            TaskApiConfig::default().with_execution_adapter(Arc::new(codex))?,
            Some(anthropic_endpoint.to_owned()),
        )
        .map_err(|error| error.to_string())?
        .with_execution_bindings_json(Some(&bindings))?
        .with_execution_bindings_active(true)?;
        assert_eq!(
            configured.execution_binding_refs(),
            [expected_agent_ref.as_str()]
        );

        Ok(())
    }

    #[test]
    fn execution_binding_rollout_mode_is_exact_and_staged_by_default() {
        assert_eq!(parse_execution_bindings_mode("staged"), Ok(false));
        assert_eq!(parse_execution_bindings_mode("active"), Ok(true));
        for invalid in ["", "enabled", "Active", " active"] {
            assert!(parse_execution_bindings_mode(invalid).is_err());
        }
    }

    #[test]
    fn github_source_configuration_is_optional_but_never_partial_or_ambiguous() {
        assert!(
            github_source_adapter_from_values(None, None)
                .is_ok_and(|configured| configured.is_none())
        );
        assert!(
            github_source_adapter_from_values(Some("123".to_owned()), None).is_err(),
            "an App ID without its mounted key must fail startup"
        );
        assert!(
            github_source_adapter_from_values(None, Some("/run/secret/key.pem".to_owned()))
                .is_err(),
            "a mounted key without its App ID must fail startup"
        );
        assert!(
            github_source_adapter_from_values(
                Some("not-an-id".to_owned()),
                Some("/run/secret/key.pem".to_owned())
            )
            .is_err(),
            "an invalid App ID must fail before any key read"
        );
        assert!(
            github_source_adapter_from_values(Some("123".to_owned()), Some(String::new())).is_err(),
            "an empty key path must fail startup"
        );
    }

    #[test]
    fn governed_connections_configuration_rejects_unpinned_or_partial_bindings()
    -> Result<(), String> {
        let valid = steward_apiserver::governed_connections::GovernedConnectionsConfig::new(
            steward_apiserver::governed_connections::ConnectionExecutionBindings {
                artifact_trust_mode: steward_apiserver::governed_connections::GITHUB_ATTESTATION_TRUST_MODE.to_owned(),
                bridge_image_digest: "registry.example.test/bridge@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
                mcp_gw_origin: "https://mcp-gw.example.test".to_owned(),
                mcp_gw_version: "0.3.2".to_owned(),
                namespace: "steward-test".to_owned(),
                runtime_class: String::new(),
            },
            "https://steward.example.test/",
        );
        assert!(valid.is_ok());
        let invalid = steward_apiserver::governed_connections::GovernedConnectionsConfig::new(
            steward_apiserver::governed_connections::ConnectionExecutionBindings {
                artifact_trust_mode:
                    steward_apiserver::governed_connections::GITHUB_ATTESTATION_TRUST_MODE
                        .to_owned(),
                bridge_image_digest: "registry.example.test/bridge:latest".to_owned(),
                mcp_gw_origin: "https://mcp-gw.example.test".to_owned(),
                mcp_gw_version: "0.3.1".to_owned(),
                namespace: "steward-test".to_owned(),
                runtime_class: "sandbox-vm".to_owned(),
            },
            "https://steward.example.test/",
        );
        assert!(invalid.is_err());
        Ok(())
    }

    #[test]
    fn stable_bridge_configuration_rejects_partial_or_unattested_input() {
        let image = Some(
            "ghcr.io/example-org/steward-bridge@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
        );
        let signer = Some(
            "https://github.com/example-org/steward/.github/workflows/release.yml@refs/tags/v0.1.0"
                .to_owned(),
        );
        let source_repository = Some("https://github.com/example-org/steward".to_owned());
        let source_commit = Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned());
        assert!(
            stable_bridge_configuration_from_values(
                image.clone(),
                signer.clone(),
                source_repository.clone(),
                source_commit.clone(),
                None,
                Some("steward-run".to_owned())
            )
            .is_err(),
            "a production route must not start with only a digest and signer identity"
        );
        assert!(
            stable_bridge_configuration_from_values(
                image,
                signer,
                source_repository,
                source_commit,
                Some("not a bundle".to_owned()),
                Some("steward-run".to_owned())
            )
            .is_err(),
            "a production route must reject an unparseable provenance bundle"
        );
    }

    #[test]
    fn cert_manager_pem_tls_material_is_decoded() -> Result<(), String> {
        let private_key_pem = [
            b"-----BEGIN ".as_slice(),
            b"PRIVATE KEY-----\nBAUG\n-----END PRIVATE KEY-----\n".as_slice(),
        ]
        .concat();
        let (certificates, private_key) = decode_tls_material(
            b"-----BEGIN CERTIFICATE-----\nAQID\n-----END CERTIFICATE-----\n".to_vec(),
            private_key_pem,
        )
        .map_err(|error| error.to_string())?;
        assert_eq!(certificates.len(), 1);
        assert_eq!(certificates[0].as_ref(), &[1, 2, 3]);
        assert_eq!(private_key.secret_der(), &[4, 5, 6]);
        Ok(())
    }

    #[test]
    fn apiserver_requires_an_explicit_nonempty_kubernetes_token_review_audience() {
        assert!(
            kubernetes_token_review_audience(None).is_err(),
            "production authentication must never omit delegated audience validation"
        );
        assert!(
            kubernetes_token_review_audience(Some(String::new())).is_err(),
            "an empty delegated audience must not disable audience validation"
        );
        assert!(
            kubernetes_token_review_audience(Some("   ".to_owned())).is_err(),
            "a whitespace-only delegated audience must not disable audience validation"
        );
        let audience =
            kubernetes_token_review_audience(Some("https://kubernetes.default.svc".to_owned()));
        assert_eq!(
            audience
                .as_ref()
                .ok()
                .map(KubernetesTokenReviewAudience::as_str),
            Some("https://kubernetes.default.svc")
        );
    }

    #[test]
    fn rbac_bootstrap_requires_an_explicit_opaque_user_and_audited_actor() -> Result<(), String> {
        assert!(
            bootstrap_rbac_arguments(vec![
                "--user-id".to_owned(),
                "usr_0123456789abcdef0123456789abcdef".to_owned(),
                "--grant".to_owned(),
                "administrator".to_owned(),
            ])
            .is_err()
        );
        let (user_id, assignment, actor) = bootstrap_rbac_arguments(vec![
            "--user-id".to_owned(),
            "usr_0123456789abcdef0123456789abcdef".to_owned(),
            "--grant".to_owned(),
            "administrator".to_owned(),
            "--actor".to_owned(),
            "bootstrap-operator".to_owned(),
        ])
        .map_err(|error| error.to_string())?;
        assert_eq!(user_id.as_str(), "usr_0123456789abcdef0123456789abcdef");
        assert_eq!(assignment, BrowserRbacAssignment::Administrator);
        assert_eq!(actor, "bootstrap-operator");
        Ok(())
    }

    #[tokio::test]
    async fn stalled_tls_handshakes_do_not_serialize_acceptance() -> Result<(), String> {
        install_rustls_crypto_provider().map_err(|error| error.to_string())?;
        let tcp = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|error| format!("bind test listener: {error}"))?;
        let address = tcp
            .local_addr()
            .map_err(|error| format!("read test listener address: {error}"))?;
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(ResolvesServerCertUsingSni::new()));
        let mut listener = TlsListener {
            acceptor: TlsAcceptor::from(Arc::new(config)),
            handshakes: tokio::task::JoinSet::new(),
            listener: tcp,
        };
        let task = tokio::spawn(async move { listener.accept().await });
        let mut stalled = Vec::new();
        for _ in 0..6 {
            stalled.push(
                TcpStream::connect(address)
                    .await
                    .map_err(|error| format!("connect stalled client: {error}"))?,
            );
        }
        let closed = timeout(Duration::from_millis(100), async {
            for stream in &mut stalled {
                let mut byte = [0_u8; 1];
                let read = stream
                    .read(&mut byte)
                    .await
                    .map_err(|error| format!("read stalled client: {error}"))?;
                if read != 0 {
                    return Err("stalled TLS client received unexpected bytes".to_owned());
                }
            }
            Ok::<(), String>(())
        })
        .await;
        task.abort();
        assert!(
            matches!(closed, Ok(Ok(()))),
            "stalled TLS handshakes must time out concurrently"
        );
        Ok(())
    }
}
