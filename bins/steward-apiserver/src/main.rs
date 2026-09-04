use std::env;
use std::error::Error;
use std::fs;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use axum::serve::Listener;
use steward_adapter_github_artifact::GitHubArtifactVerifier;
use steward_adapter_jira::{JiraAdapter, JiraConfig};
use steward_apiserver::{
    ConfiguredTaskIdentityResolver, ExecutionBindingCatalog,
    IdentityOrKubernetesTokenAuthenticator, KubeRuntimeRepository, KubernetesTokenAuthenticator,
    KubernetesTokenReviewAudience, MAX_EXECUTION_BINDING_CATALOG_BYTES, StaticTaskWorkflowCatalog,
    TaskApiConfig, agent_runs_ui, browser_admin, browser_auth, connections, google_oidc,
    governed_connections, router, stable_runtime_bridge, task_router, user_envelopes, workflows,
};
use steward_store::{
    BrowserRbacAssignment, BrowserRbacAssignmentAction, BrowserRbacAssignmentChange, PgStore,
};
use steward_types::{CanonicalUserId, OrganizationId};
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    install_rustls_crypto_provider()?;
    let mut arguments = env::args().skip(1);
    match arguments.next().as_deref() {
        Some("bootstrap-rbac") => return bootstrap_rbac(arguments.collect()).await,
        Some("validate-execution-bindings") => {
            return validate_execution_bindings(arguments.collect());
        }
        Some(command) => {
            return Err(io::Error::other(format!("unknown command {command}")).into());
        }
        None => {}
    }
    let client = kube::Client::try_default().await?;
    let store = PgStore::connect(&required("STEWARD_DATABASE_URL")?).await?;
    store.migrate().await?;
    tokio::spawn(
        steward_apiserver::governed_connections::ConnectionOperationReconciler::new(store.clone())
            .run(),
    );
    let decisions = JiraAdapter::new(
        JiraConfig {
            base_url: required("STEWARD_JIRA_BASE_URL")?,
            project_key: required("STEWARD_JIRA_PROJECT_KEY")?,
            account_email: required("STEWARD_JIRA_ACCOUNT_EMAIL")?,
        },
        required("STEWARD_JIRA_TOKEN")?,
    )
    .map_err(|error| io::Error::other(format!("Jira configuration failed: {error:?}")))?;
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
    let task_identities =
        configured_task_identity_resolver(client.clone(), token_review_audience, store.clone())?;
    let authenticator = IdentityOrKubernetesTokenAuthenticator::new(
        kubernetes_authenticator,
        task_identities.clone(),
        admin_group,
    );
    let task_workflows =
        StaticTaskWorkflowCatalog::from_json(&required("STEWARD_TASK_WORKFLOWS_JSON")?)
            .map_err(io::Error::other)?;
    let task_mcp_gateway_endpoint = match env::var("STEWARD_TASK_MCP_GW_ENDPOINT") {
        Ok(value) => Some(value),
        Err(env::VarError::NotPresent) => None,
        Err(env::VarError::NotUnicode(_)) => {
            return Err(io::Error::other("STEWARD_TASK_MCP_GW_ENDPOINT must be Unicode").into());
        }
    };
    let task_execution_bindings_json = configured_execution_bindings_json()?;
    let task_api_config = TaskApiConfig::new(task_mcp_gateway_endpoint)
        .and_then(|config| {
            config.with_execution_bindings_json(task_execution_bindings_json.as_deref())
        })
        .map_err(io::Error::other)?;
    let workflow_agents = task_api_config.execution_binding_refs();
    let runtimes = KubeRuntimeRepository::new(client);
    let browser = browser_application_router(
        store.clone(),
        runtimes.clone(),
        decisions.clone(),
        workflow_agents,
    )?;
    let app = router(
        runtimes.clone(),
        store.clone(),
        authenticator,
        decisions.clone(),
    )
    .merge(task_router(
        runtimes.clone(),
        store.clone(),
        decisions,
        task_identities,
        task_workflows,
        task_api_config,
    ));
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

fn configured_task_identity_resolver(
    client: kube::Client,
    kubernetes_audience: KubernetesTokenReviewAudience,
    store: PgStore,
) -> Result<ConfiguredTaskIdentityResolver, io::Error> {
    let values = [
        env::var("STEWARD_IDENTITY_TASK_ISSUER").ok(),
        env::var("STEWARD_IDENTITY_TASK_AUDIENCE").ok(),
        env::var("STEWARD_IDENTITY_TASK_JWKS_FILE").ok(),
    ];
    if values.iter().all(Option::is_none) {
        return Ok(ConfiguredTaskIdentityResolver::kubernetes(
            client,
            kubernetes_audience,
            store,
        ));
    }
    let [issuer, audience, jwks_file] = values;
    let required = |value: Option<String>| {
        value
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                io::Error::other("Identity task authentication configuration must be complete")
            })
    };
    ConfiguredTaskIdentityResolver::identity_from_jwks_file(
        required(issuer)?,
        required(audience)?,
        std::path::Path::new(&required(jwks_file)?),
        store,
    )
    .map_err(|_| io::Error::other("Identity task authentication configuration is invalid"))
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

fn browser_application_router(
    store: PgStore,
    runtimes: KubeRuntimeRepository,
    decisions: JiraAdapter,
    workflow_agents: Vec<String>,
) -> Result<Option<axum::Router>, Box<dyn Error>> {
    let Ok(client_id) = env::var("STEWARD_GOOGLE_OIDC_CLIENT_ID") else {
        return Ok(None);
    };
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
    let connections = governed_connections_configuration(&origin, store.clone())?;
    let app = browser_auth::browser_auth_router(auth.clone())
        .merge(user_envelopes::protected_router(
            user_envelopes::PgEnvelopeRequestBroker::new(store.clone()),
            auth.clone(),
        ))
        .merge(agent_runs_ui::protected_router(store.clone(), auth.clone()))
        .merge(browser_admin::protected_router(
            runtimes.clone(),
            store.clone(),
            decisions,
            auth.clone(),
        ))
        .merge(workflows::protected_admin_router_with_agents(
            store.clone(),
            auth.clone(),
            workflow_agents,
        ));
    let app = match connections {
        Some(broker) => app.merge(connections::protected_router(broker, auth.clone())),
        None => app,
    };
    let app = match stable_bridge_configuration()? {
        Some((service, verifier)) => app.merge(stable_runtime_bridge::protected_router(
            store, runtimes, verifier, auth, service,
        )),
        None => app,
    };
    Ok(Some(app))
}

type GovernedConnectionsBroker =
    governed_connections::GovernedConnectionsBroker<browser_auth::BrowserSessionBinding>;

fn governed_connections_configuration(
    browser_origin: &str,
    store: PgStore,
) -> Result<Option<GovernedConnectionsBroker>, io::Error> {
    let artifact_trust_mode = env::var("STEWARD_CONNECTIONS_BRIDGE_ARTIFACT_TRUST_MODE").ok();
    let values = [
        env::var("STEWARD_CONNECTIONS_BRIDGE_IMAGE").ok(),
        env::var("STEWARD_CONNECTIONS_MCP_GW_ORIGIN").ok(),
        env::var("STEWARD_CONNECTIONS_MCP_GW_VERSION").ok(),
        env::var("STEWARD_CONNECTIONS_RUNTIME_NAMESPACE").ok(),
        env::var("STEWARD_OPENSHELL_RUNTIME_CLASS_NAME").ok(),
    ];
    if artifact_trust_mode.is_none() && values.iter().all(Option::is_none) {
        return Ok(None);
    }
    let [
        bridge_image_digest,
        mcp_gw_origin,
        mcp_gw_version,
        namespace,
        runtime_class,
    ] = values;
    let required = |value: Option<String>| {
        value
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| io::Error::other("governed Connections configuration must be complete"))
    };
    let config = governed_connections::GovernedConnectionsConfig::new(
        governed_connections::ConnectionExecutionBindings {
            artifact_trust_mode: artifact_trust_mode
                .unwrap_or_else(|| governed_connections::GITHUB_ATTESTATION_TRUST_MODE.to_owned()),
            bridge_image_digest: required(bridge_image_digest)?,
            mcp_gw_origin: required(mcp_gw_origin)?,
            mcp_gw_version: required(mcp_gw_version)?,
            namespace: required(namespace)?,
            runtime_class: required(runtime_class)?,
        },
        browser_origin,
    )
    .map_err(|_| io::Error::other("governed Connections configuration is invalid"))?;
    Ok(Some(governed_connections::GovernedConnectionsBroker::new(
        store, config,
    )))
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
    use std::sync::Arc;
    use std::time::Duration;

    use axum::serve::Listener;
    use steward_store::BrowserRbacAssignment;
    use tokio::io::AsyncReadExt;
    use tokio::net::{TcpListener, TcpStream};
    use tokio::time::timeout;
    use tokio_rustls::TlsAcceptor;
    use tokio_rustls::rustls::ServerConfig;
    use tokio_rustls::rustls::server::ResolvesServerCertUsingSni;

    use super::{
        KubernetesTokenReviewAudience, TlsListener, bootstrap_rbac_arguments, decode_tls_material,
        install_rustls_crypto_provider, kubernetes_token_review_audience,
        stable_bridge_configuration_from_values, validate_execution_bindings,
    };

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
    fn governed_connections_configuration_rejects_unpinned_or_partial_bindings()
    -> Result<(), String> {
        let valid = steward_apiserver::governed_connections::GovernedConnectionsConfig::new(
            steward_apiserver::governed_connections::ConnectionExecutionBindings {
                artifact_trust_mode: steward_apiserver::governed_connections::GITHUB_ATTESTATION_TRUST_MODE.to_owned(),
                bridge_image_digest: "registry.example.test/bridge@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
                mcp_gw_origin: "https://mcp-gw.example.test".to_owned(),
                mcp_gw_version: "0.3.2".to_owned(),
                namespace: "steward-test".to_owned(),
                runtime_class: "kata-qemu".to_owned(),
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
                runtime_class: "kata-qemu".to_owned(),
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
