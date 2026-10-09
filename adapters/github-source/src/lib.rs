//! GitHub implementation of Steward's provider-neutral exact Git source read port.

use std::{collections::BTreeMap, sync::Arc, time::SystemTime};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures::{Stream, StreamExt as _, stream};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use reqwest::{Client, Response, StatusCode, redirect::Policy};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use steward_ports::{
    GitFile, GitFileRequest, GitHostingPlane, GitPack, GitPackRequest, GitRepositoryDescription,
    GitRepositoryIdentity, GitRepositoryReference, GitRevisionRequest, PortError,
};
use steward_types::direct_package::{
    ExactGitCommit, MAX_PACKAGE_FILE_BYTES, RepositoryUrl, StableProviderId,
};

pub const IMPLEMENTED_PORTS: [&str; 1] = ["GitHostingPlane"];

const API_ORIGIN: &str = "https://api.github.com";
const CLONE_ORIGIN: &str = "https://github.com";
const METADATA_RESPONSE_BYTES: u64 = 1024 * 1024;
const TREE_RESPONSE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_GIT_PACK_BYTES: u64 = 256 * 1024 * 1024;
const GIT_UPLOAD_PACK_OVERHEAD_BYTES: u64 = 4 * 1024 * 1024;
const TOKEN_RESPONSE_BYTES: u64 = 1024 * 1024;
const MAX_INSTALLATION_TOKEN_BYTES: usize = 4096;
const INSTALLATIONS_PER_PAGE: usize = 100;
const MAX_INSTALLATION_PAGES: usize = 10;
const DESCRIBE_CONCURRENCY: usize = 8;
/// Revocation is best effort and must not hold a result back for long.
const REVOKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

fn encode_upload_pack_request(
    commit: &ExactGitCommit,
    depth: Option<u32>,
) -> Result<Vec<u8>, PortError> {
    if depth == Some(0) {
        return Err(rejected("Git history depth must be positive"));
    }
    let sha = commit
        .as_str()
        .strip_prefix("git:sha1:")
        .ok_or_else(|| rejected("exact Git commit is invalid"))?;
    let mut request = Vec::new();
    append_pkt_line(
        &mut request,
        format!(
            "want {sha} multi_ack_detailed no-done side-band-64k thin-pack ofs-delta agent=steward-github-source/0.0.0\n"
        )
        .as_bytes(),
    )?;
    if let Some(depth) = depth {
        append_pkt_line(&mut request, format!("deepen {depth}\n").as_bytes())?;
    }
    request.extend_from_slice(b"0000");
    append_pkt_line(&mut request, b"done\n")?;
    Ok(request)
}

fn append_pkt_line(target: &mut Vec<u8>, payload: &[u8]) -> Result<(), PortError> {
    let length = payload
        .len()
        .checked_add(4)
        .filter(|length| *length <= 0xffff)
        .ok_or_else(|| rejected("Git protocol packet is too large"))?;
    target.extend_from_slice(format!("{length:04x}").as_bytes());
    target.extend_from_slice(payload);
    Ok(())
}

fn decode_upload_pack_result(
    response: &[u8],
    max_bytes: u64,
) -> Result<(Vec<u8>, Vec<ExactGitCommit>), PortError> {
    let mut cursor = 0_usize;
    let mut pack = Vec::new();
    let mut shallow = Vec::new();
    while cursor < response.len() {
        if response[cursor..].starts_with(b"PACK") {
            append_pack_bytes(&mut pack, &response[cursor..], max_bytes)?;
            cursor = response.len();
            continue;
        }
        let header = response
            .get(cursor..cursor.saturating_add(4))
            .ok_or_else(|| rejected("GitHub returned a truncated Git protocol packet"))?;
        let header = std::str::from_utf8(header)
            .ok()
            .and_then(|value| usize::from_str_radix(value, 16).ok())
            .ok_or_else(|| rejected("GitHub returned an invalid Git protocol packet"))?;
        cursor += 4;
        if header <= 2 {
            continue;
        }
        if header < 4 {
            return Err(rejected("GitHub returned an invalid Git protocol packet"));
        }
        let payload_length = header - 4;
        let payload = response
            .get(cursor..cursor.saturating_add(payload_length))
            .ok_or_else(|| rejected("GitHub returned a truncated Git protocol packet"))?;
        cursor += payload_length;
        match payload.first() {
            Some(1) => append_pack_bytes(&mut pack, &payload[1..], max_bytes)?,
            Some(2) => {}
            Some(3) => return Err(rejected("GitHub rejected the exact Git object request")),
            _ if payload == b"NAK\n" || payload.starts_with(b"unshallow ") => {}
            _ if payload.starts_with(b"shallow ") => {
                let commit = payload
                    .strip_prefix(b"shallow ")
                    .and_then(|value| value.strip_suffix(b"\n"))
                    .and_then(|value| std::str::from_utf8(value).ok())
                    .ok_or_else(|| rejected("GitHub returned an invalid shallow boundary"))?;
                shallow.push(
                    ExactGitCommit::parse(format!("git:sha1:{commit}"))
                        .map_err(|_| rejected("GitHub returned an invalid shallow boundary"))?,
                );
            }
            _ => {
                return Err(rejected(
                    "GitHub returned an unexpected Git protocol packet",
                ));
            }
        }
    }
    if !pack.starts_with(b"PACK") {
        return Err(rejected("GitHub response did not contain a Git pack"));
    }
    shallow.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    shallow.dedup();
    Ok((pack, shallow))
}

fn append_pack_bytes(target: &mut Vec<u8>, bytes: &[u8], max_bytes: u64) -> Result<(), PortError> {
    let next = target
        .len()
        .checked_add(bytes.len())
        .ok_or_else(|| rejected("Git pack exceeds the requested byte bound"))?;
    if next as u64 > max_bytes {
        return Err(rejected("Git pack exceeds the requested byte bound"));
    }
    target.extend_from_slice(bytes);
    Ok(())
}

/// GitHub App credentials. Deliberately implements neither `Debug` nor `Display`.
pub struct GitHubAppCredentials {
    app_id: u64,
    private_key_pem: Vec<u8>,
}

impl GitHubAppCredentials {
    pub fn new(app_id: u64, private_key_pem: Vec<u8>) -> Result<Self, PortError> {
        if app_id == 0 || private_key_pem.is_empty() {
            return Err(rejected("GitHub App credentials are invalid"));
        }
        Ok(Self {
            app_id,
            private_key_pem,
        })
    }
}

/// A secret wrapper that cannot be rendered through `Debug` or `Display`.
#[derive(Clone)]
struct SecretValue(String);

impl SecretValue {
    fn expose(&self) -> &str {
        &self.0
    }
}

enum AppAssertionIssuer {
    Rsa {
        app_id: u64,
        encoding_key: Arc<EncodingKey>,
    },
    #[cfg(test)]
    Fixed(SecretValue),
}

impl AppAssertionIssuer {
    fn issue(&self) -> Result<SecretValue, PortError> {
        match self {
            Self::Rsa {
                app_id,
                encoding_key,
            } => {
                let now = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map_err(|_| failed("system clock is unavailable"))?
                    .as_secs();
                let claims = AppClaims {
                    issued_at: now.saturating_sub(60),
                    expires_at: now.saturating_add(9 * 60),
                    issuer: app_id.to_string(),
                };
                encode(
                    &Header::new(Algorithm::RS256),
                    &claims,
                    encoding_key.as_ref(),
                )
                .map(SecretValue)
                .map_err(|_| failed("GitHub App authentication failed"))
            }
            #[cfg(test)]
            Self::Fixed(value) => Ok(value.clone()),
        }
    }
}

#[derive(Serialize)]
struct AppClaims {
    #[serde(rename = "iat")]
    issued_at: u64,
    #[serde(rename = "exp")]
    expires_at: u64,
    #[serde(rename = "iss")]
    issuer: String,
}

/// Reads exact files through a server-controlled, read-only GitHub App installation.
pub struct GitHubSourceAdapter {
    client: Client,
    api_origin: String,
    clone_origin: String,
    assertion_issuer: AppAssertionIssuer,
}

impl GitHubSourceAdapter {
    pub fn new(credentials: GitHubAppCredentials) -> Result<Self, PortError> {
        let encoding_key = EncodingKey::from_rsa_pem(&credentials.private_key_pem)
            .map_err(|_| rejected("GitHub App credentials are invalid"))?;
        Self::build(
            API_ORIGIN,
            CLONE_ORIGIN,
            AppAssertionIssuer::Rsa {
                app_id: credentials.app_id,
                encoding_key: Arc::new(encoding_key),
            },
        )
    }

    fn build(
        api_origin: &str,
        clone_origin: &str,
        assertion_issuer: AppAssertionIssuer,
    ) -> Result<Self, PortError> {
        validate_origin(api_origin, cfg!(test))?;
        validate_origin(clone_origin, cfg!(test))?;
        let client = Client::builder()
            .redirect(Policy::none())
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(20))
            .user_agent("steward-github-source/0.0.0")
            .build()
            .map_err(|_| failed("GitHub source client initialization failed"))?;
        Ok(Self {
            client,
            api_origin: api_origin.trim_end_matches('/').to_owned(),
            clone_origin: clone_origin.trim_end_matches('/').to_owned(),
            assertion_issuer,
        })
    }

    #[cfg(test)]
    fn for_test(api_origin: &str, clone_origin: &str) -> Result<Self, PortError> {
        Self::build(
            api_origin,
            clone_origin,
            AppAssertionIssuer::Fixed(SecretValue("fixture-app-assertion".to_owned())),
        )
    }

    async fn authenticate_repository(
        &self,
        coordinates: &RepositoryCoordinates,
    ) -> Result<AuthorizedRepository, PortError> {
        let assertion = self.assertion_issuer.issue()?;
        let installation: Installation = self
            .get_json(
                &format!(
                    "{}/repos/{}/{}/installation",
                    self.api_origin, coordinates.owner, coordinates.name
                ),
                &assertion,
                Authentication::App,
                METADATA_RESPONSE_BYTES,
                "resolve GitHub App installation",
            )
            .await?;
        if installation.id == 0 || installation.account.id == 0 {
            return Err(rejected("GitHub installation identity is invalid"));
        }

        let token_response: InstallationTokenResponse = self
            .post_json(
                &format!(
                    "{}/app/installations/{}/access_tokens",
                    self.api_origin, installation.id
                ),
                &assertion,
                &InstallationTokenRequest {
                    repositories: [&coordinates.name],
                    permissions: InstallationPermissions {
                        contents: "read",
                        metadata: "read",
                    },
                },
                TOKEN_RESPONSE_BYTES,
                "mint repository-scoped GitHub installation token",
            )
            .await?;
        let scoped_token = validate_installation_token(token_response, coordinates)?;
        let metadata = self
            .repository_metadata(coordinates, &scoped_token.value)
            .await?;
        let identity =
            validate_repository_metadata(metadata, coordinates, &installation, &self.clone_origin)?;
        if identity.repository_id.as_str() != scoped_token.repository_id.to_string() {
            return Err(rejected(
                "GitHub installation token is scoped to a different repository identity",
            ));
        }
        Ok(AuthorizedRepository {
            identity,
            installation_id: installation.id,
            token: scoped_token.value,
        })
    }

    async fn repository_metadata(
        &self,
        coordinates: &RepositoryCoordinates,
        token: &SecretValue,
    ) -> Result<RepositoryMetadata, PortError> {
        self.get_json(
            &format!(
                "{}/repos/{}/{}",
                self.api_origin, coordinates.owner, coordinates.name
            ),
            token,
            Authentication::Installation,
            METADATA_RESPONSE_BYTES,
            "read GitHub repository metadata",
        )
        .await
    }

    async fn revalidate_repository(
        &self,
        coordinates: &RepositoryCoordinates,
        authorized: &AuthorizedRepository,
    ) -> Result<(), PortError> {
        let metadata = self
            .repository_metadata(coordinates, &authorized.token)
            .await?;
        let current_identity =
            repository_identity_from_metadata(metadata, coordinates, &self.clone_origin)?;
        if current_identity != authorized.identity {
            return Err(rejected(
                "GitHub repository stable identity changed during read",
            ));
        }

        let assertion = self.assertion_issuer.issue()?;
        let installation: Installation = self
            .get_json(
                &format!(
                    "{}/repos/{}/{}/installation",
                    self.api_origin, coordinates.owner, coordinates.name
                ),
                &assertion,
                Authentication::App,
                METADATA_RESPONSE_BYTES,
                "revalidate GitHub App installation",
            )
            .await?;
        if installation.id != authorized.installation_id
            || installation.account.id.to_string()
                != authorized.identity.repository_owner_id.as_str()
        {
            return Err(rejected("GitHub App installation changed during read"));
        }
        Ok(())
    }

    async fn read_exact_file(
        &self,
        request: &GitFileRequest,
        coordinates: &RepositoryCoordinates,
        authorized: &AuthorizedRepository,
    ) -> Result<Vec<u8>, PortError> {
        let commit_sha = request
            .commit
            .as_str()
            .strip_prefix("git:sha1:")
            .ok_or_else(|| rejected("exact Git commit is invalid"))?;
        let commit: GitCommit = self
            .get_json(
                &format!(
                    "{}/repos/{}/{}/git/commits/{}",
                    self.api_origin, coordinates.owner, coordinates.name, commit_sha
                ),
                &authorized.token,
                Authentication::Installation,
                METADATA_RESPONSE_BYTES,
                "read exact Git commit",
            )
            .await?;
        if commit.sha != commit_sha || !valid_sha1(&commit.tree.sha) {
            return Err(rejected("GitHub returned an inconsistent commit object"));
        }

        let components: Vec<&str> = request.path.as_str().split('/').collect();
        let mut tree_sha = commit.tree.sha;
        for (index, component) in components.iter().enumerate() {
            let tree: GitTree = self
                .get_json(
                    &format!(
                        "{}/repos/{}/{}/git/trees/{}",
                        self.api_origin, coordinates.owner, coordinates.name, tree_sha
                    ),
                    &authorized.token,
                    Authentication::Installation,
                    TREE_RESPONSE_BYTES,
                    "read exact Git tree",
                )
                .await?;
            if tree.sha != tree_sha || tree.truncated {
                return Err(rejected("GitHub returned an inconsistent Git tree"));
            }
            let mut matches = tree
                .tree
                .into_iter()
                .filter(|entry| entry.path == *component);
            let entry = matches
                .next()
                .ok_or_else(|| rejected("requested Git path does not exist"))?;
            if matches.next().is_some() || !valid_sha1(&entry.sha) {
                return Err(rejected("GitHub returned an ambiguous Git tree entry"));
            }
            let final_component = index + 1 == components.len();
            if final_component {
                if entry.kind == "commit" || entry.mode == "160000" {
                    return Err(rejected("Git submodules are not valid package files"));
                }
                if entry.mode == "120000" {
                    return Err(rejected("Git symbolic links are not valid package files"));
                }
                if entry.kind != "blob" || !matches!(entry.mode.as_str(), "100644" | "100755") {
                    return Err(rejected("requested Git path is not a regular file"));
                }
                return self
                    .read_blob(
                        coordinates,
                        &authorized.token,
                        &entry.sha,
                        request.max_bytes,
                    )
                    .await;
            }
            if entry.kind != "tree" || entry.mode != "040000" {
                return Err(rejected("requested Git path traverses a non-directory"));
            }
            tree_sha = entry.sha;
        }
        Err(rejected("requested Git path is invalid"))
    }

    async fn read_blob(
        &self,
        coordinates: &RepositoryCoordinates,
        token: &SecretValue,
        expected_sha: &str,
        max_bytes: u64,
    ) -> Result<Vec<u8>, PortError> {
        let encoded_bound = max_bytes
            .checked_mul(2)
            .and_then(|value| value.checked_add(METADATA_RESPONSE_BYTES))
            .ok_or_else(|| rejected("Git file byte bound is invalid"))?;
        let blob: GitBlob = self
            .get_json(
                &format!(
                    "{}/repos/{}/{}/git/blobs/{}",
                    self.api_origin, coordinates.owner, coordinates.name, expected_sha
                ),
                token,
                Authentication::Installation,
                encoded_bound,
                "read exact Git blob",
            )
            .await?;
        if blob.sha != expected_sha || blob.encoding != "base64" {
            return Err(rejected("GitHub returned an inconsistent Git blob"));
        }
        if blob.size > max_bytes {
            return Err(rejected("Git file exceeds the requested byte bound"));
        }
        let compact: String = blob
            .content
            .chars()
            .filter(|character| !matches!(character, '\r' | '\n'))
            .collect();
        let bytes = STANDARD
            .decode(compact.as_bytes())
            .map_err(|_| rejected("GitHub returned invalid blob encoding"))?;
        if bytes.len() as u64 != blob.size || bytes.len() as u64 > max_bytes {
            return Err(rejected("GitHub returned an inconsistent Git blob size"));
        }
        Ok(bytes)
    }

    async fn get_json<T: DeserializeOwned>(
        &self,
        url: &str,
        credential: &SecretValue,
        authentication: Authentication,
        max_bytes: u64,
        operation: &'static str,
    ) -> Result<T, PortError> {
        let request = self
            .client
            .get(url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .bearer_auth(credential.expose());
        let response = request.send().await.map_err(|_| failed(operation))?;
        decode_response(response, authentication, max_bytes, operation).await
    }

    async fn post_json<B: Serialize, T: DeserializeOwned>(
        &self,
        url: &str,
        credential: &SecretValue,
        body: &B,
        max_bytes: u64,
        operation: &'static str,
    ) -> Result<T, PortError> {
        let response = self
            .client
            .post(url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .bearer_auth(credential.expose())
            .json(body)
            .send()
            .await
            .map_err(|_| failed(operation))?;
        decode_response(response, Authentication::App, max_bytes, operation).await
    }
}

impl GitHubSourceAdapter {
    /// Maps each installation account ID to its installation ID, bounded to a fixed page count.
    async fn installations_by_owner(
        &self,
        assertion: &SecretValue,
    ) -> Result<BTreeMap<u64, u64>, PortError> {
        let mut owners = BTreeMap::new();
        // One page past the bound distinguishes exactly the bound from more than it.
        for page in 1..=MAX_INSTALLATION_PAGES + 1 {
            let installations: Vec<ListedInstallation> = self
                .get_json(
                    &format!(
                        "{}/app/installations?per_page={INSTALLATIONS_PER_PAGE}&page={page}",
                        self.api_origin
                    ),
                    assertion,
                    Authentication::App,
                    METADATA_RESPONSE_BYTES,
                    "list GitHub App installations",
                )
                .await?;
            let count = installations.len();
            if count > INSTALLATIONS_PER_PAGE {
                return Err(rejected("GitHub returned an oversized installation page"));
            }
            if page > MAX_INSTALLATION_PAGES {
                return if count == 0 {
                    Ok(owners)
                } else {
                    Err(rejected(
                        "GitHub App installation listing exceeds the supported bound",
                    ))
                };
            }
            for installation in installations {
                // GitHub documents a nullable account; such an installation owns nothing.
                let Some(account) = installation.account else {
                    continue;
                };
                if installation.id == 0 || account.id == 0 {
                    return Err(rejected("GitHub installation identity is invalid"));
                }
                owners.insert(account.id, installation.id);
            }
            if count < INSTALLATIONS_PER_PAGE {
                return Ok(owners);
            }
        }
        Err(rejected(
            "GitHub App installation listing exceeds the supported bound",
        ))
    }

    /// Best-effort revocation of a short-lived token, bounded by a short timeout. A failure
    /// is reported in one token-free line; the token still expires on its own.
    async fn revoke_installation_token(&self, token: &SecretValue) {
        let outcome = self
            .client
            .delete(format!("{}/installation/token", self.api_origin))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .bearer_auth(token.expose())
            .timeout(REVOKE_TIMEOUT)
            .send()
            .await;
        let failure = match outcome {
            Ok(response) if response.status().is_success() => return,
            Ok(response) => format!("status {}", response.status().as_u16()),
            Err(error) if error.is_timeout() => "timed out".to_owned(),
            Err(_) => "request failed".to_owned(),
        };
        eprintln!("github source: metadata token revocation failed: {failure}");
    }

    async fn describe_repository(
        &self,
        reference: &GitRepositoryReference,
        installations: &BTreeMap<u64, u64>,
        assertion: &SecretValue,
    ) -> Result<GitRepositoryDescription, PortError> {
        let owner_id = reference
            .repository_owner_id
            .as_str()
            .parse::<u64>()
            .ok()
            .filter(|value| *value != 0)
            .ok_or_else(|| rejected("GitHub repository owner identity is invalid"))?;
        let repository_id = reference
            .repository_id
            .as_str()
            .parse::<u64>()
            .ok()
            .filter(|value| *value != 0)
            .ok_or_else(|| rejected("GitHub repository stable identity is invalid"))?;
        let installation_id = installations
            .get(&owner_id)
            .ok_or_else(|| rejected("GitHub App is not installed for the repository owner"))?;
        let token_response: InstallationTokenResponse = self
            .post_json(
                &format!(
                    "{}/app/installations/{installation_id}/access_tokens",
                    self.api_origin
                ),
                assertion,
                &MetadataTokenRequest {
                    repository_ids: [repository_id],
                    permissions: MetadataPermissions { metadata: "read" },
                },
                TOKEN_RESPONSE_BYTES,
                "mint repository-scoped GitHub metadata token",
            )
            .await?;
        let minted = (!token_response.token.is_empty()
            && token_response.token.len() <= MAX_INSTALLATION_TOKEN_BYTES)
            .then(|| SecretValue(token_response.token.clone()));
        let token = match validate_metadata_token(token_response, repository_id) {
            Ok(token) => token,
            Err(error) => {
                // A token minted with an unexpected scope is revoked, not left alive.
                if let Some(minted) = minted {
                    self.revoke_installation_token(&minted).await;
                }
                return Err(error);
            }
        };
        let metadata: Result<RepositoryListingMetadata, PortError> = self
            .get_json(
                &format!("{}/repositories/{repository_id}", self.api_origin),
                &token,
                Authentication::Installation,
                METADATA_RESPONSE_BYTES,
                "read GitHub repository metadata",
            )
            .await;
        self.revoke_installation_token(&token).await;
        repository_description(metadata?, owner_id, repository_id, &self.clone_origin)
    }
}

impl GitHostingPlane for GitHubSourceAdapter {
    fn describe_repositories<'a>(
        &'a self,
        repositories: &'a [GitRepositoryReference],
    ) -> impl Stream<Item = (usize, Result<GitRepositoryDescription, PortError>)> + Send + 'a {
        stream::once(async move {
            // Installations are listed once per resolution, not once per repository.
            let prepared = async {
                let assertion = self.assertion_issuer.issue()?;
                let installations = self.installations_by_owner(&assertion).await?;
                Ok::<_, PortError>(Arc::new((assertion, installations)))
            }
            .await;
            match prepared {
                Err(error) => stream::iter(
                    (0..repositories.len()).map(move |index| (index, Err(error.clone()))),
                )
                .left_stream(),
                Ok(prepared) => stream::iter(repositories.iter().enumerate())
                    .map(move |(index, reference)| {
                        let prepared = Arc::clone(&prepared);
                        async move {
                            let (assertion, installations) = prepared.as_ref();
                            (
                                index,
                                self.describe_repository(reference, installations, assertion)
                                    .await,
                            )
                        }
                    })
                    .buffer_unordered(DESCRIBE_CONCURRENCY)
                    .right_stream(),
            }
        })
        .flatten()
    }

    async fn resolve_repository(
        &self,
        repository: &RepositoryUrl,
    ) -> Result<GitRepositoryIdentity, PortError> {
        let coordinates = RepositoryCoordinates::parse(repository, &self.clone_origin)?;
        Ok(self.authenticate_repository(&coordinates).await?.identity)
    }

    async fn read_file(&self, request: &GitFileRequest) -> Result<GitFile, PortError> {
        if request.max_bytes == 0 || request.max_bytes > MAX_PACKAGE_FILE_BYTES {
            return Err(rejected("Git file byte bound is invalid"));
        }
        let coordinates =
            RepositoryCoordinates::parse(&request.repository.repository, &self.clone_origin)?;
        let authorized = self.authenticate_repository(&coordinates).await?;
        if authorized.identity != request.repository {
            return Err(rejected(
                "GitHub repository stable identity does not match request",
            ));
        }
        let bytes = self
            .read_exact_file(request, &coordinates, &authorized)
            .await?;
        self.revalidate_repository(&coordinates, &authorized)
            .await?;
        Ok(GitFile {
            repository: authorized.identity,
            commit: request.commit.clone(),
            path: request.path.clone(),
            bytes,
        })
    }

    async fn read_pack(&self, request: &GitPackRequest) -> Result<GitPack, PortError> {
        if request.max_bytes == 0
            || request.max_bytes > MAX_GIT_PACK_BYTES
            || request.depth == Some(0)
        {
            return Err(rejected("Git pack bounds are invalid"));
        }
        let coordinates =
            RepositoryCoordinates::parse(&request.repository.repository, &self.clone_origin)?;
        let authorized = self.authenticate_repository(&coordinates).await?;
        if authorized.identity != request.repository {
            return Err(rejected(
                "GitHub repository stable identity does not match request",
            ));
        }
        let body = encode_upload_pack_request(&request.commit, request.depth)?;
        let response = self
            .client
            .post(format!(
                "{}/{}/{}.git/git-upload-pack",
                self.clone_origin, coordinates.owner, coordinates.name
            ))
            .header("Accept", "application/x-git-upload-pack-result")
            .header("Content-Type", "application/x-git-upload-pack-request")
            .header("Git-Protocol", "version=0")
            .basic_auth("x-access-token", Some(authorized.token.expose()))
            .body(body)
            .send()
            .await
            .map_err(|_| failed("read exact Git pack"))?;
        if response.status().is_redirection() {
            return Err(rejected("GitHub Git redirects are forbidden"));
        }
        if !response.status().is_success() {
            if response.status().is_client_error() {
                return Err(rejected("GitHub rejected the exact Git pack request"));
            }
            return Err(failed("read exact Git pack"));
        }
        let response_bound = request
            .max_bytes
            .checked_add(GIT_UPLOAD_PACK_OVERHEAD_BYTES)
            .ok_or_else(|| rejected("Git pack bounds are invalid"))?;
        let response = read_bounded(response, response_bound, "read exact Git pack").await?;
        let (bytes, shallow) = decode_upload_pack_result(&response, request.max_bytes)?;
        self.revalidate_repository(&coordinates, &authorized)
            .await?;
        Ok(GitPack {
            repository: authorized.identity,
            commit: request.commit.clone(),
            shallow,
            bytes,
        })
    }

    async fn resolve_revision(
        &self,
        request: &GitRevisionRequest,
    ) -> Result<ExactGitCommit, PortError> {
        let reference = request
            .reference
            .strip_prefix("git:ref:")
            .ok_or_else(|| rejected("Git reference is invalid"))?;
        if reference.is_empty() || reference.len() > 512 || reference.chars().any(char::is_control)
        {
            return Err(rejected("Git reference is invalid"));
        }
        let coordinates =
            RepositoryCoordinates::parse(&request.repository.repository, &self.clone_origin)?;
        let authorized = self.authenticate_repository(&coordinates).await?;
        if authorized.identity != request.repository {
            return Err(rejected(
                "GitHub repository stable identity does not match request",
            ));
        }
        let encoded = percent_encode_path_segment(reference);
        let commit: ResolvedCommit = self
            .get_json(
                &format!(
                    "{}/repos/{}/{}/commits/{encoded}",
                    self.api_origin, coordinates.owner, coordinates.name
                ),
                &authorized.token,
                Authentication::Installation,
                METADATA_RESPONSE_BYTES,
                "resolve GitHub ref",
            )
            .await?;
        if !valid_sha1(&commit.sha) {
            return Err(rejected("GitHub returned an invalid commit identity"));
        }
        self.revalidate_repository(&coordinates, &authorized)
            .await?;
        ExactGitCommit::parse(format!("git:sha1:{}", commit.sha))
            .map_err(|_| rejected("GitHub returned an invalid commit identity"))
    }
}

fn percent_encode_path_segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

#[derive(Clone, Copy)]
enum Authentication {
    App,
    Installation,
}

struct RepositoryCoordinates {
    owner: String,
    name: String,
    repository: RepositoryUrl,
}

impl RepositoryCoordinates {
    fn parse(repository: &RepositoryUrl, clone_origin: &str) -> Result<Self, PortError> {
        let prefix = format!("{}/", clone_origin.trim_end_matches('/'));
        let path = repository
            .as_str()
            .strip_prefix(&prefix)
            .and_then(|value| value.strip_suffix(".git"))
            .ok_or_else(|| rejected("repository URL is outside the configured GitHub host"))?;
        let mut components = path.split('/');
        let owner = components
            .next()
            .filter(|value| valid_repository_component(value))
            .ok_or_else(|| rejected("GitHub repository URL is invalid"))?;
        let name = components
            .next()
            .filter(|value| valid_repository_component(value))
            .ok_or_else(|| rejected("GitHub repository URL is invalid"))?;
        if components.next().is_some() {
            return Err(rejected("GitHub repository URL is invalid"));
        }
        Ok(Self {
            owner: owner.to_owned(),
            name: name.to_owned(),
            repository: repository.clone(),
        })
    }

    fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

struct AuthorizedRepository {
    identity: GitRepositoryIdentity,
    installation_id: u64,
    token: SecretValue,
}

struct ScopedToken {
    value: SecretValue,
    repository_id: u64,
}

#[derive(Deserialize)]
struct Installation {
    id: u64,
    account: ProviderIdentity,
}

#[derive(Deserialize)]
struct ProviderIdentity {
    id: u64,
}

#[derive(Serialize)]
struct InstallationTokenRequest<'a> {
    repositories: [&'a str; 1],
    permissions: InstallationPermissions<'a>,
}

#[derive(Serialize)]
struct InstallationPermissions<'a> {
    contents: &'a str,
    metadata: &'a str,
}

#[derive(Deserialize)]
struct ListedInstallation {
    id: u64,
    account: Option<ProviderIdentity>,
}

#[derive(Serialize)]
struct MetadataTokenRequest<'a> {
    repository_ids: [u64; 1],
    permissions: MetadataPermissions<'a>,
}

#[derive(Serialize)]
struct MetadataPermissions<'a> {
    metadata: &'a str,
}

#[derive(Deserialize)]
struct RepositoryListingMetadata {
    id: u64,
    name: String,
    full_name: String,
    private: bool,
    html_url: String,
    default_branch: String,
    owner: RepositoryOwner,
}

#[derive(Deserialize)]
struct RepositoryOwner {
    id: u64,
    login: String,
}

#[derive(Deserialize)]
struct InstallationTokenResponse {
    token: String,
    expires_at: String,
    permissions: BTreeMap<String, String>,
    repositories: Vec<TokenRepository>,
}

#[derive(Deserialize)]
struct TokenRepository {
    id: u64,
    full_name: String,
}

#[derive(Deserialize)]
struct RepositoryMetadata {
    id: u64,
    full_name: String,
    clone_url: String,
    owner: ProviderIdentity,
}

#[derive(Deserialize)]
struct GitCommit {
    sha: String,
    tree: GitObjectReference,
}

#[derive(Deserialize)]
struct ResolvedCommit {
    sha: String,
}

#[derive(Deserialize)]
struct GitObjectReference {
    sha: String,
}

#[derive(Deserialize)]
struct GitTree {
    sha: String,
    truncated: bool,
    tree: Vec<GitTreeEntry>,
}

#[derive(Deserialize)]
struct GitTreeEntry {
    path: String,
    mode: String,
    #[serde(rename = "type")]
    kind: String,
    sha: String,
}

#[derive(Deserialize)]
struct GitBlob {
    sha: String,
    size: u64,
    encoding: String,
    content: String,
}

fn validate_installation_token(
    response: InstallationTokenResponse,
    coordinates: &RepositoryCoordinates,
) -> Result<ScopedToken, PortError> {
    if response.token.is_empty()
        || response.token.len() > MAX_INSTALLATION_TOKEN_BYTES
        || !valid_expiration(&response.expires_at)
        || response.permissions.len() != 2
        || response.permissions.get("contents").map(String::as_str) != Some("read")
        || response.permissions.get("metadata").map(String::as_str) != Some("read")
        || response.repositories.len() != 1
    {
        return Err(rejected(
            "GitHub returned an invalid repository-scoped installation token",
        ));
    }
    let repository = &response.repositories[0];
    if repository.id == 0 || repository.full_name != coordinates.full_name() {
        return Err(rejected(
            "GitHub installation token is not scoped to the requested repository",
        ));
    }
    Ok(ScopedToken {
        value: SecretValue(response.token),
        repository_id: repository.id,
    })
}

fn validate_metadata_token(
    response: InstallationTokenResponse,
    repository_id: u64,
) -> Result<SecretValue, PortError> {
    if response.token.is_empty()
        || response.token.len() > MAX_INSTALLATION_TOKEN_BYTES
        || !valid_expiration(&response.expires_at)
        || response.permissions.len() != 1
        || response.permissions.get("metadata").map(String::as_str) != Some("read")
        || response.repositories.len() != 1
        || response.repositories[0].id != repository_id
    {
        return Err(rejected(
            "GitHub returned an invalid repository-scoped metadata token",
        ));
    }
    Ok(SecretValue(response.token))
}

fn repository_description(
    metadata: RepositoryListingMetadata,
    owner_id: u64,
    repository_id: u64,
    clone_origin: &str,
) -> Result<GitRepositoryDescription, PortError> {
    if metadata.id != repository_id || metadata.owner.id != owner_id {
        return Err(rejected("GitHub repository identity is inconsistent"));
    }
    if !valid_repository_component(&metadata.owner.login)
        || metadata.owner.login.len() > 39
        || !valid_repository_component(&metadata.name)
        || metadata.name.len() > 100
        || metadata.full_name != format!("{}/{}", metadata.owner.login, metadata.name)
        || metadata.html_url != format!("{clone_origin}/{}", metadata.full_name)
        || metadata.default_branch.is_empty()
        || metadata.default_branch.len() > 255
        || metadata.default_branch.chars().any(char::is_control)
    {
        return Err(rejected("GitHub repository metadata is invalid"));
    }
    Ok(GitRepositoryDescription {
        owner: metadata.owner.login,
        repository_owner_id: StableProviderId::parse(owner_id.to_string())
            .map_err(|_| rejected("GitHub repository owner identity is invalid"))?,
        name: metadata.name,
        repository_id: StableProviderId::parse(repository_id.to_string())
            .map_err(|_| rejected("GitHub repository stable identity is invalid"))?,
        default_branch: metadata.default_branch,
        private: metadata.private,
        web_url: metadata.html_url,
    })
}

fn validate_repository_metadata(
    metadata: RepositoryMetadata,
    coordinates: &RepositoryCoordinates,
    installation: &Installation,
    clone_origin: &str,
) -> Result<GitRepositoryIdentity, PortError> {
    let identity = repository_identity_from_metadata(metadata, coordinates, clone_origin)?;
    if identity.repository_owner_id.as_str() != installation.account.id.to_string() {
        return Err(rejected(
            "GitHub installation owner does not match repository owner",
        ));
    }
    Ok(identity)
}

fn repository_identity_from_metadata(
    metadata: RepositoryMetadata,
    coordinates: &RepositoryCoordinates,
    clone_origin: &str,
) -> Result<GitRepositoryIdentity, PortError> {
    let expected_clone = format!("{}/{}.git", clone_origin, coordinates.full_name());
    if metadata.id == 0
        || metadata.owner.id == 0
        || metadata.full_name != coordinates.full_name()
        || metadata.clone_url != expected_clone
    {
        return Err(rejected("GitHub repository identity is inconsistent"));
    }
    Ok(GitRepositoryIdentity {
        repository: coordinates.repository.clone(),
        repository_id: StableProviderId::parse(metadata.id.to_string())
            .map_err(|_| rejected("GitHub repository stable identity is invalid"))?,
        repository_owner_id: StableProviderId::parse(metadata.owner.id.to_string())
            .map_err(|_| rejected("GitHub repository owner identity is invalid"))?,
    })
}

async fn decode_response<T: DeserializeOwned>(
    response: Response,
    authentication: Authentication,
    max_bytes: u64,
    operation: &'static str,
) -> Result<T, PortError> {
    let status = response.status();
    if status.is_redirection() {
        return Err(rejected("GitHub API redirects are forbidden"));
    }
    if !status.is_success() {
        if status == StatusCode::NOT_FOUND && matches!(authentication, Authentication::App) {
            return Err(rejected(
                "GitHub App is not installed on the requested repository",
            ));
        }
        if status.is_client_error() {
            return Err(rejected(operation));
        }
        return Err(failed(operation));
    }
    let bytes = read_bounded(response, max_bytes, operation).await?;
    serde_json::from_slice(&bytes).map_err(|_| rejected("GitHub returned malformed JSON"))
}

async fn read_bounded(
    mut response: Response,
    max_bytes: u64,
    operation: &'static str,
) -> Result<Vec<u8>, PortError> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes)
    {
        return Err(rejected(
            "GitHub response exceeds the configured byte bound",
        ));
    }
    let capacity = usize::try_from(max_bytes.min(64 * 1024)).unwrap_or(0);
    let mut bytes = Vec::with_capacity(capacity);
    while let Some(chunk) = response.chunk().await.map_err(|_| failed(operation))? {
        let next_len = bytes
            .len()
            .checked_add(chunk.len())
            .ok_or_else(|| rejected("GitHub response exceeds the configured byte bound"))?;
        if next_len as u64 > max_bytes {
            return Err(rejected(
                "GitHub response exceeds the configured byte bound",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn validate_origin(origin: &str, allow_loopback_http: bool) -> Result<(), PortError> {
    let parsed =
        reqwest::Url::parse(origin).map_err(|_| rejected("GitHub source origin is invalid"))?;
    let permitted_scheme = parsed.scheme() == "https"
        || allow_loopback_http
            && parsed.scheme() == "http"
            && parsed
                .host_str()
                .is_some_and(|host| matches!(host, "127.0.0.1" | "localhost" | "[::1]" | "::1"));
    if !permitted_scheme
        || parsed.username() != ""
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || parsed.path() != "/"
    {
        return Err(rejected("GitHub source origin is invalid"));
    }
    Ok(())
}

fn valid_repository_component(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn valid_sha1(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_expiration(value: &str) -> bool {
    value.len() == 20
        && value.as_bytes().get(4) == Some(&b'-')
        && value.as_bytes().get(7) == Some(&b'-')
        && value.as_bytes().get(10) == Some(&b'T')
        && value.as_bytes().get(13) == Some(&b':')
        && value.as_bytes().get(16) == Some(&b':')
        && value.ends_with('Z')
        && value.bytes().enumerate().all(|(index, byte)| {
            matches!(index, 4 | 7 | 10 | 13 | 16 | 19) || byte.is_ascii_digit()
        })
}

fn rejected(reason: &str) -> PortError {
    PortError::Rejected {
        reason: reason.to_owned(),
    }
}

fn failed(reason: &str) -> PortError {
    PortError::Failed {
        reason: reason.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::{Arc, Mutex},
        thread,
    };

    use futures::StreamExt as _;
    use serde_json::json;
    use steward_ports::{
        GitFileRequest, GitHostingPlane, GitRepositoryIdentity, GitRepositoryReference,
        GitRevisionRequest, PortError,
    };
    use steward_types::direct_package::{
        ExactGitCommit, RelativePath, RepositoryUrl, StableProviderId,
    };

    use super::{GitHubSourceAdapter, decode_upload_pack_result, encode_upload_pack_request};

    const CLONE_ORIGIN: &str = "https://github.example.test";
    const REPOSITORY: &str = "https://github.example.test/example-org/source-a.git";
    const COMMIT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const ROOT_TREE: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const CATALOG_TREE: &str = "cccccccccccccccccccccccccccccccccccccccc";
    const BLOB: &str = "dddddddddddddddddddddddddddddddddddddddd";

    #[test]
    fn upload_pack_request_pins_the_exact_commit_and_depth() -> Result<(), String> {
        let commit = ExactGitCommit::parse(format!("git:sha1:{COMMIT}"))?;
        let request = encode_upload_pack_request(&commit, Some(20))
            .map_err(|error| format!("encode upload-pack request: {error:?}"))?;
        let rendered = String::from_utf8(request).map_err(|error| error.to_string())?;
        assert!(rendered.contains(&format!("want {COMMIT} ")));
        assert!(rendered.contains("deepen 20\n"));
        assert!(rendered.ends_with("00000009done\n"));
        Ok(())
    }

    #[test]
    fn upload_pack_result_extracts_only_pack_sideband() -> Result<(), String> {
        fn packet(payload: &[u8]) -> Vec<u8> {
            let mut encoded = format!("{:04x}", payload.len() + 4).into_bytes();
            encoded.extend_from_slice(payload);
            encoded
        }
        let mut response = packet(b"NAK\n");
        response.extend(packet(
            b"shallow bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n",
        ));
        response.extend(packet(b"\x02counting objects\n"));
        response.extend(packet(b"\x01PACKfixture-one"));
        response.extend(packet(b"\x01fixture-two"));
        response.extend_from_slice(b"0000");
        assert_eq!(
            decode_upload_pack_result(&response, 64)
                .map_err(|error| format!("decode upload-pack result: {error:?}"))?,
            (
                b"PACKfixture-onefixture-two".to_vec(),
                vec![ExactGitCommit::parse(format!("git:sha1:{ROOT_TREE}"))?]
            )
        );
        assert!(decode_upload_pack_result(&response, 8).is_err());
        Ok(())
    }

    struct ResponseSpec {
        status: u16,
        headers: Vec<(&'static str, &'static str)>,
        body: String,
    }

    impl ResponseSpec {
        fn json(body: serde_json::Value) -> Self {
            Self {
                status: 200,
                headers: vec![("Content-Type", "application/json")],
                body: body.to_string(),
            }
        }

        fn status(status: u16, body: &str) -> Self {
            Self {
                status,
                headers: Vec::new(),
                body: body.to_owned(),
            }
        }

        fn redirect() -> Self {
            Self {
                status: 302,
                headers: vec![("Location", "http://127.0.0.1:9/forbidden")],
                body: String::new(),
            }
        }
    }

    struct MockGitHub {
        origin: String,
        requests: Arc<Mutex<Vec<String>>>,
        thread: thread::JoinHandle<Result<(), String>>,
    }

    impl MockGitHub {
        fn start(responses: Vec<ResponseSpec>) -> Result<Self, String> {
            let listener = TcpListener::bind("127.0.0.1:0").map_err(|error| error.to_string())?;
            let origin = format!(
                "http://{}",
                listener.local_addr().map_err(|error| error.to_string())?
            );
            let requests = Arc::new(Mutex::new(Vec::new()));
            let captured = Arc::clone(&requests);
            let thread = thread::spawn(move || -> Result<(), String> {
                for response in responses {
                    let (mut stream, _) = listener.accept().map_err(|error| error.to_string())?;
                    let request = read_request(&mut stream)?;
                    captured
                        .lock()
                        .map_err(|error| error.to_string())?
                        .push(request);
                    let reason = match response.status {
                        200 => "OK",
                        302 => "Found",
                        404 => "Not Found",
                        _ => "Error",
                    };
                    let extra_headers = response
                        .headers
                        .into_iter()
                        .map(|(name, value)| format!("{name}: {value}\r\n"))
                        .collect::<String>();
                    write!(
                        stream,
                        "HTTP/1.1 {} {}\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                        response.status,
                        reason,
                        extra_headers,
                        response.body.len(),
                        response.body
                    )
                    .map_err(|error| error.to_string())?;
                }
                Ok(())
            });
            Ok(Self {
                origin,
                requests,
                thread,
            })
        }

        fn finish(self) -> Result<Vec<String>, String> {
            self.thread
                .join()
                .map_err(|_| "mock GitHub thread panicked".to_owned())??;
            let requests = match Arc::try_unwrap(self.requests) {
                Ok(requests) => requests,
                Err(_) => return Err("captured requests remain shared".to_owned()),
            };
            requests.into_inner().map_err(|error| error.to_string())
        }
    }

    fn read_request(stream: &mut std::net::TcpStream) -> Result<String, String> {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 4096];
        let mut header_end = None;
        loop {
            let count = stream
                .read(&mut buffer)
                .map_err(|error| error.to_string())?;
            if count == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..count]);
            if header_end.is_none() {
                header_end = bytes
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                    .map(|index| index + 4);
            }
            if let Some(end) = header_end {
                let headers = String::from_utf8_lossy(&bytes[..end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if bytes.len() >= end + content_length {
                    break;
                }
            }
        }
        String::from_utf8(bytes).map_err(|error| error.to_string())
    }

    fn adapter(mock: &MockGitHub) -> Result<GitHubSourceAdapter, String> {
        GitHubSourceAdapter::for_test(&mock.origin, CLONE_ORIGIN)
            .map_err(|error| format!("construct test GitHub source adapter: {error:?}"))
    }

    fn repository_url(value: &str) -> Result<RepositoryUrl, String> {
        RepositoryUrl::parse(value.to_owned())
    }

    fn identity(
        repository: &str,
        repository_id: u64,
        owner_id: u64,
    ) -> Result<GitRepositoryIdentity, String> {
        Ok(GitRepositoryIdentity {
            repository: repository_url(repository)?,
            repository_id: StableProviderId::parse(repository_id.to_string())?,
            repository_owner_id: StableProviderId::parse(owner_id.to_string())?,
        })
    }

    fn request() -> Result<GitFileRequest, String> {
        Ok(GitFileRequest {
            repository: identity(REPOSITORY, 1001, 1000)?,
            commit: ExactGitCommit::parse(format!("git:sha1:{COMMIT}"))?,
            path: RelativePath::parse("catalog/task.json".to_owned())?,
            max_bytes: 64,
        })
    }

    fn installation(id: u64, owner_id: u64) -> ResponseSpec {
        ResponseSpec::json(json!({"id": id, "account": {"id": owner_id}}))
    }

    fn token(repository_id: u64, full_name: &str, value: &str) -> ResponseSpec {
        ResponseSpec::json(json!({
            "token": value,
            "expires_at": "2030-01-01T00:00:00Z",
            "permissions": {"contents": "read", "metadata": "read"},
            "repositories": [{"id": repository_id, "full_name": full_name}]
        }))
    }

    fn metadata(repository_id: u64, owner_id: u64, full_name: &str) -> ResponseSpec {
        ResponseSpec::json(json!({
            "id": repository_id,
            "full_name": full_name,
            "clone_url": format!("{CLONE_ORIGIN}/{full_name}.git"),
            "owner": {"id": owner_id}
        }))
    }

    fn authentication_responses() -> Vec<ResponseSpec> {
        vec![
            installation(7001, 1000),
            token(1001, "example-org/source-a", "fixture-installation-value"),
            metadata(1001, 1000, "example-org/source-a"),
        ]
    }

    fn successful_read_responses() -> Vec<ResponseSpec> {
        let mut responses = authentication_responses();
        responses.extend([
            ResponseSpec::json(json!({"sha": COMMIT, "tree": {"sha": ROOT_TREE}})),
            ResponseSpec::json(json!({
                "sha": ROOT_TREE,
                "truncated": false,
                "tree": [{"path": "catalog", "mode": "040000", "type": "tree", "sha": CATALOG_TREE}]
            })),
            ResponseSpec::json(json!({
                "sha": CATALOG_TREE,
                "truncated": false,
                "tree": [{"path": "task.json", "mode": "100644", "type": "blob", "sha": BLOB}]
            })),
            ResponseSpec::json(json!({
                "sha": BLOB,
                "size": 4,
                "encoding": "base64",
                "content": "dGVz\ndA==\n"
            })),
            metadata(1001, 1000, "example-org/source-a"),
            installation(7001, 1000),
        ]);
        responses
    }

    fn assert_rejected(result: Result<impl Sized, PortError>, expected: &str) {
        assert!(
            matches!(result, Err(PortError::Rejected { ref reason }) if reason == expected),
            "expected rejection {expected:?}"
        );
    }

    fn port<T>(result: Result<T, PortError>) -> Result<T, String> {
        result.map_err(|error| format!("unexpected port error: {error:?}"))
    }

    #[tokio::test]
    async fn app_without_repository_installation_fails_closed() -> Result<(), String> {
        let mock = MockGitHub::start(vec![ResponseSpec::status(
            404,
            r#"{"message":"fixture-sensitive-provider-body"}"#,
        )])?;
        let result = adapter(&mock)?
            .resolve_repository(&repository_url(REPOSITORY)?)
            .await;
        assert_rejected(
            result,
            "GitHub App is not installed on the requested repository",
        );
        let requests = mock.finish()?;
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("GET /repos/example-org/source-a/installation "));
        assert!(requests[0].contains("authorization: Bearer fixture-app-assertion"));
        Ok(())
    }

    #[tokio::test]
    async fn reads_regular_file_at_exact_commit_with_one_repository_token() -> Result<(), String> {
        let mock = MockGitHub::start(successful_read_responses())?;
        let request = request()?;
        let file = port(adapter(&mock)?.read_file(&request).await)?;
        assert_eq!(file.repository, request.repository);
        assert_eq!(file.commit, request.commit);
        assert_eq!(file.path, request.path);
        assert_eq!(file.bytes, b"test");

        let requests = mock.finish()?;
        assert_eq!(requests.len(), 9);
        let token_request = &requests[1];
        assert!(token_request.starts_with("POST /app/installations/7001/access_tokens "));
        assert!(token_request.contains("authorization: Bearer fixture-app-assertion"));
        assert!(token_request.contains(r#""repositories":["source-a"]"#));
        assert!(token_request.contains(r#""contents":"read""#));
        assert!(token_request.contains(r#""metadata":"read""#));
        assert!(!token_request.contains("write"));
        assert!(requests[3].contains("/git/commits/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
        assert!(requests[3].contains("authorization: Bearer fixture-installation-value"));
        assert!(
            requests
                .iter()
                .all(|request| !request.contains("caller-credential"))
        );
        Ok(())
    }

    #[tokio::test]
    async fn resolves_a_branch_to_one_exact_commit_before_package_reads() -> Result<(), String> {
        let mut responses = authentication_responses();
        responses.extend([
            ResponseSpec::json(json!({"sha": COMMIT})),
            metadata(1001, 1000, "example-org/source-a"),
            installation(7001, 1000),
        ]);
        let mock = MockGitHub::start(responses)?;
        let repository = identity(REPOSITORY, 1001, 1000)?;
        let commit = port(
            adapter(&mock)?
                .resolve_revision(&GitRevisionRequest {
                    repository,
                    reference: "git:ref:release/v1".to_owned(),
                })
                .await,
        )?;
        assert_eq!(commit.as_str(), format!("git:sha1:{COMMIT}"));
        let requests = mock.finish()?;
        assert!(requests[3].contains("/commits/release%2Fv1"));
        Ok(())
    }

    #[tokio::test]
    async fn independently_authenticates_multiple_installed_repositories() -> Result<(), String> {
        let second_repository = "https://github.example.test/team-a/source-b.git";
        let mock = MockGitHub::start(vec![
            installation(7001, 1000),
            token(1001, "example-org/source-a", "fixture-installation-a"),
            metadata(1001, 1000, "example-org/source-a"),
            installation(7002, 2000),
            token(2001, "team-a/source-b", "fixture-installation-b"),
            metadata(2001, 2000, "team-a/source-b"),
        ])?;
        let adapter = adapter(&mock)?;
        let first = port(
            adapter
                .resolve_repository(&repository_url(REPOSITORY)?)
                .await,
        )?;
        let second = port(
            adapter
                .resolve_repository(&repository_url(second_repository)?)
                .await,
        )?;
        assert_eq!(first, identity(REPOSITORY, 1001, 1000)?);
        assert_eq!(second, identity(second_repository, 2001, 2000)?);
        let requests = mock.finish()?;
        assert!(requests[1].contains(r#""repositories":["source-a"]"#));
        assert!(requests[4].contains(r#""repositories":["source-b"]"#));
        Ok(())
    }

    #[tokio::test]
    async fn rejects_token_scoped_to_the_wrong_stable_repository() -> Result<(), String> {
        let mock = MockGitHub::start(vec![
            installation(7001, 1000),
            token(1002, "example-org/source-a", "fixture-installation-value"),
            metadata(1001, 1000, "example-org/source-a"),
        ])?;
        let result = adapter(&mock)?
            .resolve_repository(&repository_url(REPOSITORY)?)
            .await;
        assert_rejected(
            result,
            "GitHub installation token is scoped to a different repository identity",
        );
        mock.finish()?;
        Ok(())
    }

    #[tokio::test]
    async fn rejects_request_bound_to_a_different_stable_repository() -> Result<(), String> {
        let mock = MockGitHub::start(authentication_responses())?;
        let mut mismatched = request()?;
        mismatched.repository.repository_id = StableProviderId::parse("1002")?;
        assert_rejected(
            adapter(&mock)?.read_file(&mismatched).await,
            "GitHub repository stable identity does not match request",
        );
        mock.finish()?;
        Ok(())
    }

    #[tokio::test]
    async fn missing_commit_or_path_cannot_produce_a_read() -> Result<(), String> {
        let mut missing_commit = authentication_responses();
        missing_commit.push(ResponseSpec::status(404, r#"{"message":"not found"}"#));
        let mock = MockGitHub::start(missing_commit)?;
        assert_rejected(
            adapter(&mock)?.read_file(&request()?).await,
            "read exact Git commit",
        );
        mock.finish()?;

        let mut missing_path = authentication_responses();
        missing_path.extend([
            ResponseSpec::json(json!({"sha": COMMIT, "tree": {"sha": ROOT_TREE}})),
            ResponseSpec::json(json!({
                "sha": ROOT_TREE,
                "truncated": false,
                "tree": []
            })),
        ]);
        let mock = MockGitHub::start(missing_path)?;
        assert_rejected(
            adapter(&mock)?.read_file(&request()?).await,
            "requested Git path does not exist",
        );
        mock.finish()?;
        Ok(())
    }

    #[tokio::test]
    async fn rejects_stable_identity_replacement_during_read() -> Result<(), String> {
        let mut responses = successful_read_responses();
        responses[7] = metadata(1002, 1000, "example-org/source-a");
        responses.pop();
        let mock = MockGitHub::start(responses)?;
        let result = adapter(&mock)?.read_file(&request()?).await;
        assert_rejected(
            result,
            "GitHub repository stable identity changed during read",
        );
        mock.finish()?;
        Ok(())
    }

    #[tokio::test]
    async fn rejects_app_removal_during_read() -> Result<(), String> {
        let mut responses = successful_read_responses();
        responses[8] = ResponseSpec::status(404, r#"{"message":"removed"}"#);
        let mock = MockGitHub::start(responses)?;
        let result = adapter(&mock)?.read_file(&request()?).await;
        assert_rejected(
            result,
            "GitHub App is not installed on the requested repository",
        );
        mock.finish()?;
        Ok(())
    }

    #[tokio::test]
    async fn rejects_inconsistent_commit_tree_and_blob_objects() -> Result<(), String> {
        let cases = ["commit", "tree", "blob"];
        for inconsistent in cases {
            let mut responses = successful_read_responses();
            match inconsistent {
                "commit" => {
                    responses[3] =
                        ResponseSpec::json(json!({"sha": BLOB, "tree": {"sha": ROOT_TREE}}));
                    responses.truncate(4);
                }
                "tree" => {
                    responses[4] = ResponseSpec::json(json!({
                        "sha": CATALOG_TREE,
                        "truncated": false,
                        "tree": []
                    }));
                    responses.truncate(5);
                }
                "blob" => {
                    responses[6] = ResponseSpec::json(json!({
                        "sha": ROOT_TREE,
                        "size": 4,
                        "encoding": "base64",
                        "content": "dGVzdA=="
                    }));
                    responses.truncate(7);
                }
                _ => return Err("unknown inconsistent object fixture".to_owned()),
            }
            let mock = MockGitHub::start(responses)?;
            let result = adapter(&mock)?.read_file(&request()?).await;
            assert!(
                matches!(result, Err(PortError::Rejected { ref reason }) if reason.contains("inconsistent")),
                "{inconsistent} mismatch must reject"
            );
            mock.finish()?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn rejects_symlink_submodule_and_ambiguous_tree_entries() -> Result<(), String> {
        let cases = [
            (
                "120000",
                "blob",
                "Git symbolic links are not valid package files",
            ),
            (
                "160000",
                "commit",
                "Git submodules are not valid package files",
            ),
        ];
        for (mode, kind, expected) in cases {
            let mut responses = authentication_responses();
            responses.extend([
                ResponseSpec::json(json!({"sha": COMMIT, "tree": {"sha": ROOT_TREE}})),
                ResponseSpec::json(json!({
                    "sha": ROOT_TREE,
                    "truncated": false,
                    "tree": [{"path": "catalog", "mode": mode, "type": kind, "sha": BLOB}]
                })),
            ]);
            let mock = MockGitHub::start(responses)?;
            let mut one_component = request()?;
            one_component.path = RelativePath::parse("catalog".to_owned())?;
            assert_rejected(adapter(&mock)?.read_file(&one_component).await, expected);
            mock.finish()?;
        }

        let mut responses = authentication_responses();
        responses.extend([
            ResponseSpec::json(json!({"sha": COMMIT, "tree": {"sha": ROOT_TREE}})),
            ResponseSpec::json(json!({
                "sha": ROOT_TREE,
                "truncated": false,
                "tree": [
                    {"path": "catalog", "mode": "040000", "type": "tree", "sha": CATALOG_TREE},
                    {"path": "catalog", "mode": "040000", "type": "tree", "sha": BLOB}
                ]
            })),
        ]);
        let mock = MockGitHub::start(responses)?;
        assert_rejected(
            adapter(&mock)?.read_file(&request()?).await,
            "GitHub returned an ambiguous Git tree entry",
        );
        mock.finish()?;
        Ok(())
    }

    #[tokio::test]
    async fn rejects_mutable_refs_traversal_redirects_and_oversized_files() -> Result<(), String> {
        assert!(ExactGitCommit::parse("git:sha1:main".to_owned()).is_err());
        assert!(RelativePath::parse("../task.json".to_owned()).is_err());

        let redirect = MockGitHub::start(vec![ResponseSpec::redirect()])?;
        assert_rejected(
            adapter(&redirect)?
                .resolve_repository(&repository_url(REPOSITORY)?)
                .await,
            "GitHub API redirects are forbidden",
        );
        assert_eq!(redirect.finish()?.len(), 1);

        let mut responses = authentication_responses();
        responses.extend([
            ResponseSpec::json(json!({"sha": COMMIT, "tree": {"sha": ROOT_TREE}})),
            ResponseSpec::json(json!({
                "sha": ROOT_TREE,
                "truncated": false,
                "tree": [{"path": "catalog", "mode": "040000", "type": "tree", "sha": CATALOG_TREE}]
            })),
            ResponseSpec::json(json!({
                "sha": CATALOG_TREE,
                "truncated": false,
                "tree": [{"path": "task.json", "mode": "100644", "type": "blob", "sha": BLOB}]
            })),
            ResponseSpec::json(json!({
                "sha": BLOB,
                "size": 65,
                "encoding": "base64",
                "content": ""
            })),
        ]);
        let oversized = MockGitHub::start(responses)?;
        assert_rejected(
            adapter(&oversized)?.read_file(&request()?).await,
            "Git file exceeds the requested byte bound",
        );
        oversized.finish()?;
        Ok(())
    }

    #[tokio::test]
    async fn provider_error_body_and_credentials_are_not_returned() -> Result<(), String> {
        let sensitive_body = "fixture-sensitive-provider-body";
        let mock = MockGitHub::start(vec![ResponseSpec::status(401, sensitive_body)])?;
        let result = adapter(&mock)?
            .resolve_repository(&repository_url(REPOSITORY)?)
            .await;
        let rendered = format!("{result:?}");
        assert!(!rendered.contains(sensitive_body));
        assert!(!rendered.contains("fixture-app-assertion"));
        assert!(!rendered.contains("fixture-installation-value"));
        mock.finish()?;
        Ok(())
    }

    fn reference(owner_id: &str, repository_id: &str) -> Result<GitRepositoryReference, String> {
        Ok(GitRepositoryReference {
            repository_owner_id: StableProviderId::parse(owner_id)?,
            repository_id: StableProviderId::parse(repository_id)?,
        })
    }

    fn metadata_token(repository_id: u64) -> ResponseSpec {
        ResponseSpec::json(json!({
            "token": "fixture-metadata-value",
            "expires_at": "2030-01-01T00:00:00Z",
            "permissions": {"metadata": "read"},
            "repositories": [{"id": repository_id, "full_name": "example-org/source-a"}]
        }))
    }

    fn listing_metadata(repository_id: u64, owner_id: u64, url: &str) -> ResponseSpec {
        ResponseSpec::json(json!({
            "id": repository_id,
            "name": "source-a",
            "full_name": "example-org/source-a",
            "private": true,
            "html_url": url,
            "default_branch": "main",
            "owner": {"id": owner_id, "login": "example-org"}
        }))
    }

    async fn describe(
        adapter: &GitHubSourceAdapter,
        references: &[GitRepositoryReference],
    ) -> Vec<Result<steward_ports::GitRepositoryDescription, PortError>> {
        let mut described = adapter
            .describe_repositories(references)
            .collect::<Vec<_>>()
            .await;
        described.sort_by_key(|(index, _)| *index);
        assert_eq!(
            described
                .iter()
                .map(|(index, _)| *index)
                .collect::<Vec<_>>(),
            (0..references.len()).collect::<Vec<_>>(),
            "every reference yields exactly one result"
        );
        described.into_iter().map(|(_, result)| result).collect()
    }

    #[tokio::test]
    async fn installation_listing_beyond_its_bound_is_a_distinct_failure() -> Result<(), String> {
        let full_page = |page: u64| {
            ResponseSpec::json(serde_json::Value::Array(
                (0..100)
                    .map(|index| {
                        json!({"id": page * 1000 + index + 1, "account": {"id": page * 1000 + index + 1}})
                    })
                    .collect(),
            ))
        };
        let mock = MockGitHub::start((1..=11).map(full_page).collect())?;
        let described = describe(&adapter(&mock)?, &[reference("1000", "1001")?]).await;
        assert_rejected(
            described[0].clone(),
            "GitHub App installation listing exceeds the supported bound",
        );
        assert_eq!(mock.finish()?.len(), 11);

        // Exactly the bound is accepted: the page past it is empty.
        let mut responses = (1..=10).map(full_page).collect::<Vec<_>>();
        responses.push(ResponseSpec::json(json!([])));
        let mock = MockGitHub::start(responses)?;
        let described = describe(&adapter(&mock)?, &[reference("1000", "1001")?]).await;
        assert_rejected(
            described[0].clone(),
            "GitHub App is not installed for the repository owner",
        );
        assert_eq!(mock.finish()?.len(), 11);
        Ok(())
    }

    #[tokio::test]
    async fn describes_admitted_repositories_by_stable_identity() -> Result<(), String> {
        let mock = MockGitHub::start(vec![
            ResponseSpec::json(json!([
                {"id": 7001, "account": {"id": 1000}},
                {"id": 7002, "account": {"id": 2000}}
            ])),
            metadata_token(1001),
            listing_metadata(1001, 1000, &format!("{CLONE_ORIGIN}/example-org/source-a")),
            ResponseSpec::status(204, ""),
        ])?;
        let described = describe(
            &adapter(&mock)?,
            &[reference("1000", "1001")?, reference("3000", "3001")?],
        )
        .await;
        assert_eq!(described.len(), 2);
        let first = port(described[0].clone())?;
        assert_eq!(first.owner, "example-org");
        assert_eq!(first.repository_owner_id.as_str(), "1000");
        assert_eq!(first.name, "source-a");
        assert_eq!(first.repository_id.as_str(), "1001");
        assert_eq!(first.default_branch, "main");
        assert!(first.private);
        assert_eq!(
            first.web_url,
            format!("{CLONE_ORIGIN}/example-org/source-a")
        );
        assert_rejected(
            described[1].clone(),
            "GitHub App is not installed for the repository owner",
        );

        let requests = mock.finish()?;
        assert_eq!(requests.len(), 4);
        assert!(requests[0].starts_with("GET /app/installations?per_page=100&page=1 "));
        assert!(requests[0].contains("authorization: Bearer fixture-app-assertion"));
        assert!(requests[1].starts_with("POST /app/installations/7001/access_tokens "));
        assert!(requests[1].contains(r#""repository_ids":[1001]"#));
        assert!(requests[1].contains(r#""permissions":{"metadata":"read"}"#));
        assert!(!requests[1].contains("contents"));
        assert!(requests[2].starts_with("GET /repositories/1001 "));
        assert!(requests[2].contains("authorization: Bearer fixture-metadata-value"));
        assert!(requests[3].starts_with("DELETE /installation/token "));
        assert!(requests[3].contains("authorization: Bearer fixture-metadata-value"));
        Ok(())
    }

    #[tokio::test]
    async fn rejects_inconsistent_repository_descriptions() -> Result<(), String> {
        for (owner_id, url) in [
            (1999, format!("{CLONE_ORIGIN}/example-org/source-a")),
            (
                1000,
                "https://elsewhere.example.com/example-org/source-a".to_owned(),
            ),
        ] {
            let mock = MockGitHub::start(vec![
                ResponseSpec::json(json!([{"id": 7001, "account": {"id": 1000}}])),
                metadata_token(1001),
                listing_metadata(1001, owner_id, &url),
                ResponseSpec::status(204, ""),
            ])?;
            let described = describe(&adapter(&mock)?, &[reference("1000", "1001")?]).await;
            assert_eq!(described.len(), 1);
            assert!(matches!(described[0], Err(PortError::Rejected { .. })));
            mock.finish()?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn rejects_metadata_token_scoped_to_another_repository() -> Result<(), String> {
        let mock = MockGitHub::start(vec![
            ResponseSpec::json(json!([{"id": 7001, "account": {"id": 1000}}])),
            metadata_token(1002),
            ResponseSpec::status(204, ""),
        ])?;
        let described = describe(&adapter(&mock)?, &[reference("1000", "1001")?]).await;
        assert_rejected(
            described[0].clone(),
            "GitHub returned an invalid repository-scoped metadata token",
        );
        let requests = mock.finish()?;
        assert_eq!(requests.len(), 3);
        assert!(
            requests[2].starts_with("DELETE /installation/token "),
            "a token with an unexpected scope is revoked"
        );
        assert!(requests[2].contains("authorization: Bearer fixture-metadata-value"));
        assert!(
            !requests
                .iter()
                .any(|request| request.starts_with("GET /repositories/")),
            "the invalid token is never used"
        );
        Ok(())
    }

    #[tokio::test]
    async fn installation_listing_failure_fails_every_description() -> Result<(), String> {
        let mock = MockGitHub::start(vec![ResponseSpec::status(500, "fixture-provider-body")])?;
        let described = describe(
            &adapter(&mock)?,
            &[reference("1000", "1001")?, reference("1000", "1002")?],
        )
        .await;
        assert_eq!(described.len(), 2);
        assert!(described.iter().all(|result| matches!(
            result,
            Err(PortError::Failed { reason }) if reason == "list GitHub App installations"
        )));
        mock.finish()?;
        Ok(())
    }

    #[tokio::test]
    async fn installation_listing_pages_and_skips_accountless_installations() -> Result<(), String>
    {
        let mut first_page = (0..100)
            .map(|index| json!({"id": 8000 + index, "account": {"id": 5000 + index}}))
            .collect::<Vec<_>>();
        first_page[0] = json!({"id": 8000, "account": null});
        let mock = MockGitHub::start(vec![
            ResponseSpec::json(serde_json::Value::Array(first_page)),
            ResponseSpec::json(json!([{"id": 7001, "account": {"id": 1000}}])),
            metadata_token(1001),
            listing_metadata(1001, 1000, &format!("{CLONE_ORIGIN}/example-org/source-a")),
            ResponseSpec::status(204, ""),
        ])?;
        let described = describe(&adapter(&mock)?, &[reference("1000", "1001")?]).await;
        assert_eq!(port(described[0].clone())?.repository_id.as_str(), "1001");
        let requests = mock.finish()?;
        assert!(requests[0].starts_with("GET /app/installations?per_page=100&page=1 "));
        assert!(requests[1].starts_with("GET /app/installations?per_page=100&page=2 "));
        assert!(requests[2].starts_with("POST /app/installations/7001/access_tokens "));
        Ok(())
    }
}
