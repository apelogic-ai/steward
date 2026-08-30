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
    ConfiguredTaskIdentityResolver, IdentityOrKubernetesTokenAuthenticator, KubeRuntimeRepository,
    KubernetesTokenAuthenticator, KubernetesTokenReviewAudience, StaticTaskWorkflowCatalog,
    agent_runs_ui, browser_admin, browser_auth, browser_hop1_attestation, connections, google_oidc,
    mcp_gw_connections, router, stable_runtime_bridge, task_router, user_envelopes, workflows,
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
    if env::args().nth(1).as_deref() == Some("bootstrap-rbac") {
        return bootstrap_rbac(env::args().skip(2).collect()).await;
    }
    let client = kube::Client::try_default().await?;
    let store = PgStore::connect(&required("STEWARD_DATABASE_URL")?).await?;
    store.migrate().await?;
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
    let runtimes = KubeRuntimeRepository::new(client);
    let browser = browser_application_router(store.clone(), runtimes.clone(), decisions.clone())?;
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
    let connections = browser_hop1_connections_configuration(&origin)?;
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
        .merge(workflows::protected_admin_router(
            store.clone(),
            auth.clone(),
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

type BrowserHop1ConnectionsBroker = mcp_gw_connections::McpGwConnectionsBroker<
    browser_auth::BrowserSessionBinding,
    browser_hop1_attestation::BrowserHop1AttestationIssuer<
        browser_hop1_attestation::IdentityBrowserHop1Client,
    >,
>;

fn browser_hop1_connections_configuration(
    browser_origin: &str,
) -> Result<Option<BrowserHop1ConnectionsBroker>, io::Error> {
    let values = BrowserHop1Environment::from_process()?;
    let Some(values) = values else {
        return Ok(None);
    };
    let signing = browser_hop1_attestation::BrowserHop1AttestationConfig::from_files(
        values.issuer,
        values.assertion_audience,
        values.key_id,
        &values.signing_key_file,
        &values.public_jwks_file,
    )
    .map_err(|_| io::Error::other("browser HOP-1 signer configuration is invalid"))?;
    let service_account_token = browser_hop1_attestation::ProjectedServiceAccountTokenFile::new(
        values.service_account_token_file,
    )
    .map_err(|_| io::Error::other("browser HOP-1 workload token configuration is invalid"))?;
    let endpoint =
        browser_hop1_attestation::IdentityBrowserHop1Endpoint::new(values.identity_endpoint)
            .map_err(|_| io::Error::other("browser HOP-1 Identity endpoint is invalid"))?;
    let identity_ca_certificate_pem = fs::read(values.identity_ca_certificate_file)
        .map_err(|_| io::Error::other("read browser HOP-1 Identity CA certificate"))?;
    let client = browser_hop1_attestation::IdentityBrowserHop1Client::new(
        endpoint,
        identity_ca_certificate_pem,
    )
    .map_err(|_| io::Error::other("browser HOP-1 Identity client is unavailable"))?;
    let issuer = browser_hop1_attestation::BrowserHop1AttestationIssuer::new(
        signing,
        service_account_token,
        client,
    );
    let mcp_gateway_origin = values.mcp_gateway_origin;
    let config = mcp_gw_connections::McpGwConnectionsConfig::new(
        mcp_gateway_origin,
        browser_origin.to_owned(),
    )
    .map_err(|_| io::Error::other("browser MCP-GW connection origin is invalid"))?;
    mcp_gw_connections::McpGwConnectionsBroker::new(config, issuer)
        .map(Some)
        .map_err(io::Error::other)
}

struct BrowserHop1Environment {
    mcp_gateway_origin: String,
    identity_endpoint: String,
    identity_ca_certificate_file: std::path::PathBuf,
    issuer: String,
    assertion_audience: String,
    key_id: String,
    signing_key_file: std::path::PathBuf,
    public_jwks_file: std::path::PathBuf,
    service_account_token_file: std::path::PathBuf,
}

impl BrowserHop1Environment {
    fn from_process() -> Result<Option<Self>, io::Error> {
        Self::from_values([
            env::var("STEWARD_MCP_GW_ORIGIN").ok(),
            env::var("STEWARD_IDENTITY_BROWSER_HOP1_ENDPOINT").ok(),
            env::var("STEWARD_IDENTITY_BROWSER_HOP1_CA_CERTIFICATE_FILE").ok(),
            env::var("STEWARD_BROWSER_HOP1_ISSUER").ok(),
            env::var("STEWARD_BROWSER_HOP1_ASSERTION_AUDIENCE").ok(),
            env::var("STEWARD_BROWSER_HOP1_KEY_ID").ok(),
            env::var("STEWARD_BROWSER_HOP1_SIGNING_KEY_FILE").ok(),
            env::var("STEWARD_BROWSER_HOP1_JWKS_FILE").ok(),
            env::var("STEWARD_BROWSER_HOP1_SERVICE_ACCOUNT_TOKEN_FILE").ok(),
        ])
    }

    fn from_values(values: [Option<String>; 9]) -> Result<Option<Self>, io::Error> {
        if values.iter().all(Option::is_none) {
            return Ok(None);
        }
        let [
            mcp_gateway_origin,
            identity_endpoint,
            identity_ca_certificate_file,
            issuer,
            assertion_audience,
            key_id,
            signing_key_file,
            public_jwks_file,
            service_account_token_file,
        ] = values;
        let required = |value: Option<String>| {
            value
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| io::Error::other("browser HOP-1 configuration must be complete"))
        };
        Ok(Some(Self {
            mcp_gateway_origin: required(mcp_gateway_origin)?,
            identity_endpoint: required(identity_endpoint)?,
            identity_ca_certificate_file: std::path::PathBuf::from(required(
                identity_ca_certificate_file,
            )?),
            issuer: required(issuer)?,
            assertion_audience: required(assertion_audience)?,
            key_id: required(key_id)?,
            signing_key_file: std::path::PathBuf::from(required(signing_key_file)?),
            public_jwks_file: std::path::PathBuf::from(required(public_jwks_file)?),
            service_account_token_file: std::path::PathBuf::from(required(
                service_account_token_file,
            )?),
        }))
    }
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
        BrowserHop1Environment, KubernetesTokenReviewAudience, TlsListener,
        bootstrap_rbac_arguments, decode_tls_material, install_rustls_crypto_provider,
        kubernetes_token_review_audience, stable_bridge_configuration_from_values,
    };

    #[test]
    fn browser_hop1_connections_configuration_is_all_or_nothing() -> Result<(), String> {
        let mut partial = std::array::from_fn(|_| None);
        partial[0] = Some("https://mcp-gw.example.test".to_owned());
        assert!(BrowserHop1Environment::from_values(partial).is_err());
        assert!(
            BrowserHop1Environment::from_values(std::array::from_fn(|_| None))
                .map_err(|error| error.to_string())?
                .is_none()
        );
        let configured = BrowserHop1Environment::from_values([
            Some("https://mcp-gw.example.test".to_owned()),
            Some("https://identity.example.test/v1/browser-hop1/exchange".to_owned()),
            Some("/run/browser-hop1/identity-ca.crt".to_owned()),
            Some("https://steward.example.test".to_owned()),
            Some("identity-browser-hop1".to_owned()),
            Some("steward-browser-hop1-current".to_owned()),
            Some("/run/steward-browser-hop1/signing-key.der".to_owned()),
            Some("/run/steward-browser-hop1/jwks.json".to_owned()),
            Some("/var/run/secrets/tokens/identity-exchange".to_owned()),
        ])
        .map_err(|error| error.to_string())?
        .ok_or("complete browser HOP-1 configuration was disabled")?;
        assert_eq!(configured.assertion_audience, "identity-browser-hop1");
        assert_eq!(
            configured.identity_ca_certificate_file.to_string_lossy(),
            "/run/browser-hop1/identity-ca.crt"
        );
        assert_eq!(
            configured.signing_key_file.to_string_lossy(),
            "/run/steward-browser-hop1/signing-key.der"
        );
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
