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
    KubeRuntimeRepository, KubernetesTaskIdentityResolver, KubernetesTokenAuthenticator,
    KubernetesTokenReviewAudience, StaticTaskWorkflowCatalog, agent_runs_ui, browser_auth,
    google_oidc, router, stable_runtime_bridge, task_router, user_envelopes,
};
use steward_store::{
    BrowserRbacAssignment, BrowserRbacAssignmentAction, BrowserRbacAssignmentChange, PgStore,
};
#[cfg(feature = "local-fixtures")]
use steward_types::Email;
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
    #[cfg(feature = "local-fixtures")]
    if env::args().nth(1).as_deref() == Some("bootstrap-local-canonical-user") {
        return bootstrap_local_canonical_user(env::args().skip(2).collect()).await;
    }
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
    let authenticator = KubernetesTokenAuthenticator::new(
        client.clone(),
        env::var("STEWARD_ADMIN_GROUP").unwrap_or_else(|_| "agents.apelogic.ai/admin".to_owned()),
        token_review_audience.clone(),
    );
    let task_identities =
        KubernetesTaskIdentityResolver::new(client.clone(), token_review_audience, store.clone());
    let task_workflows =
        StaticTaskWorkflowCatalog::from_json(&required("STEWARD_TASK_WORKFLOWS_JSON")?)
            .map_err(io::Error::other)?;
    let runtimes = KubeRuntimeRepository::new(client);
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
    let browser = browser_application_router(store.clone())?;
    let stable_bridge =
        stable_bridge_configuration()?.map(|(service, verifier)| match browser.as_ref() {
            Some(browser) => stable_runtime_bridge::protected_router(
                store,
                runtimes,
                verifier,
                browser.auth.clone(),
                service,
            ),
            None => stable_runtime_bridge::authentication_required_router(),
        });
    let app = application_router(app, browser.map(|browser| browser.router), stable_bridge);
    let listener = tls_listener(
        &env::var("STEWARD_APISERVER_BIND").unwrap_or_else(|_| "0.0.0.0:8443".to_owned()),
        &required("STEWARD_TLS_CERT_DER")?,
        &required("STEWARD_TLS_KEY_DER")?,
    )
    .await?;
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(feature = "local-fixtures")]
struct LocalCanonicalUserFixture {
    user_id: CanonicalUserId,
    organization_id: OrganizationId,
    email: Email,
    actor: String,
}

#[cfg(feature = "local-fixtures")]
fn local_canonical_user_fixture_arguments(
    arguments: Vec<String>,
) -> Result<LocalCanonicalUserFixture, io::Error> {
    let mut user_id = None;
    let mut organization_id = None;
    let mut email = None;
    let mut actor = None;
    let mut values = arguments.into_iter();
    while let Some(flag) = values.next() {
        let value = values
            .next()
            .ok_or_else(local_canonical_user_fixture_usage)?;
        match flag.as_str() {
            "--user-id" if user_id.is_none() => {
                user_id = Some(
                    CanonicalUserId::parse(value)
                        .map_err(|_| local_canonical_user_fixture_usage())?,
                )
            }
            "--organization-id" if organization_id.is_none() => {
                organization_id = Some(
                    OrganizationId::parse(value)
                        .map_err(|_| local_canonical_user_fixture_usage())?,
                )
            }
            "--email" if email.is_none() => {
                email = Some(Email::parse(value).map_err(|_| local_canonical_user_fixture_usage())?)
            }
            "--actor" if actor.is_none() && !value.trim().is_empty() => actor = Some(value),
            _ => return Err(local_canonical_user_fixture_usage()),
        }
    }
    match (user_id, organization_id, email, actor) {
        (Some(user_id), Some(organization_id), Some(email), Some(actor)) => {
            Ok(LocalCanonicalUserFixture {
                user_id,
                organization_id,
                email,
                actor,
            })
        }
        _ => Err(local_canonical_user_fixture_usage()),
    }
}

#[cfg(feature = "local-fixtures")]
fn local_canonical_user_fixture_usage() -> io::Error {
    io::Error::other(
        "usage: steward-apiserver-bin bootstrap-local-canonical-user --user-id usr_<opaque-id> --organization-id org_<id> --email <fixture-email> --actor <audited-fixture-actor>",
    )
}

#[cfg(feature = "local-fixtures")]
async fn bootstrap_local_canonical_user(arguments: Vec<String>) -> Result<(), Box<dyn Error>> {
    let fixture = local_canonical_user_fixture_arguments(arguments)?;
    let store = PgStore::connect(&required("STEWARD_DATABASE_URL")?).await?;
    store.migrate().await?;
    let principal = store
        .register_local_fixture_canonical_principal(
            &fixture.user_id,
            &fixture.organization_id,
            &fixture.email,
            &fixture.actor,
        )
        .await?;
    println!(
        "local canonical fixture {} registered",
        principal.user_id.as_str()
    );
    Ok(())
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

struct BrowserApplication {
    router: axum::Router,
    auth: browser_auth::BrowserAuthService,
}

fn browser_application_router(
    store: PgStore,
) -> Result<Option<BrowserApplication>, Box<dyn Error>> {
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
    let router = browser_auth::browser_auth_router(auth.clone())
        .merge(user_envelopes::protected_router(
            user_envelopes::PgEnvelopeRequestBroker::new(store.clone()),
            auth.clone(),
        ))
        .merge(agent_runs_ui::protected_router(store.clone(), auth.clone()));
    Ok(Some(BrowserApplication { router, auth }))
}

fn application_router(
    app: axum::Router,
    browser: Option<axum::Router>,
    stable_bridge: Option<axum::Router>,
) -> axum::Router {
    match browser {
        Some(browser) => match stable_bridge {
            Some(stable_bridge) => app.merge(browser).merge(stable_bridge),
            None => app.merge(browser),
        },
        None => match stable_bridge {
            Some(stable_bridge) => app.merge(stable_bridge),
            None => app,
        },
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

    use axum::Router;
    use axum::routing::get;
    use axum::serve::Listener;
    use steward_store::BrowserRbacAssignment;
    use tokio::io::AsyncReadExt;
    use tokio::net::{TcpListener, TcpStream};
    use tokio::time::timeout;
    use tokio_rustls::TlsAcceptor;
    use tokio_rustls::rustls::ServerConfig;
    use tokio_rustls::rustls::server::ResolvesServerCertUsingSni;

    use super::{
        KubernetesTokenReviewAudience, TlsListener, application_router, bootstrap_rbac_arguments,
        decode_tls_material, install_rustls_crypto_provider, kubernetes_token_review_audience,
        stable_bridge_configuration_from_values,
    };

    #[cfg(feature = "local-fixtures")]
    #[test]
    fn local_fixture_requires_and_preserves_the_exact_canonical_user_id() -> Result<(), String> {
        let fixture = super::local_canonical_user_fixture_arguments(vec![
            "--user-id".to_owned(),
            "usr_0123456789abcdef0123456789abcdef".to_owned(),
            "--organization-id".to_owned(),
            "org_example".to_owned(),
            "--email".to_owned(),
            "alice@example.com".to_owned(),
            "--actor".to_owned(),
            "local-e2e".to_owned(),
        ])
        .map_err(|error| error.to_string())?;

        assert_eq!(
            fixture.user_id.as_str(),
            "usr_0123456789abcdef0123456789abcdef"
        );
        assert_eq!(fixture.organization_id.as_str(), "org_example");
        assert_eq!(fixture.email.as_str(), "alice@example.com");
        assert_eq!(fixture.actor, "local-e2e");
        Ok(())
    }

    #[cfg(feature = "local-fixtures")]
    #[test]
    fn local_fixture_rejects_incomplete_ambiguous_or_untyped_identity() {
        let valid = [
            ("--user-id", "usr_0123456789abcdef0123456789abcdef"),
            ("--organization-id", "org_example"),
            ("--email", "alice@example.com"),
            ("--actor", "local-e2e"),
        ];
        for arguments in [
            valid[..3].to_vec(),
            vec![
                ("--user-id", "alice@example.com"),
                ("--organization-id", "org_example"),
                ("--email", "alice@example.com"),
                ("--actor", "local-e2e"),
            ],
            vec![
                ("--user-id", "usr_0123456789abcdef0123456789abcdef"),
                ("--organization-id", "org_example"),
                ("--email", "not-an-email"),
                ("--actor", "local-e2e"),
            ],
            vec![
                ("--user-id", "usr_0123456789abcdef0123456789abcdef"),
                ("--organization-id", "org_example"),
                ("--email", "alice@example.com"),
                ("--actor", "   "),
            ],
            vec![
                ("--user-id", "usr_0123456789abcdef0123456789abcdef"),
                ("--organization-id", "org_example"),
                ("--email", "alice@example.com"),
                ("--actor", "local-e2e"),
                ("--user-id", "usr_abcdef0123456789abcdef0123456789"),
            ],
        ] {
            let flattened = arguments
                .into_iter()
                .flat_map(|(flag, value)| [flag.to_owned(), value.to_owned()])
                .collect();
            assert!(
                super::local_canonical_user_fixture_arguments(flattened).is_err(),
                "local fixture identity inputs must be complete, exact, typed, and unambiguous"
            );
        }
    }

    #[test]
    fn stable_bridge_route_is_mounted_without_a_google_browser_application() {
        let stable_bridge = Router::new().route(
            "/app/api/v1/stable-runtime-bridge",
            get(|| async { "authentication required" }),
        );
        let app = application_router(Router::new(), None, Some(stable_bridge));

        assert!(
            app.has_routes(),
            "complete stable bridge configuration must mount its fail-closed route even when Google browser OIDC is disabled"
        );
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
