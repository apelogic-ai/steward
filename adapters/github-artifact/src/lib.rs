//! GitHub Actions provenance verification for immutable Steward artifacts.
//!
//! This adapter owns GitHub/Sigstore semantics.  Callers receive an opaque
//! digest-pinned reference only after the signed statement binds its exact subject,
//! signer workflow, source repository, and resolved source commit.

use sigstore_verify::{
    VerificationPolicy, Verifier,
    trust_root::{SigstoreInstance, TrustedRoot},
    types::{Bundle, Sha256Hash, SignatureContent, intoto::Statement},
};

const GITHUB_ACTIONS_OIDC_ISSUER: &str = "https://token.actions.githubusercontent.com";
const SLSA_PROVENANCE_V1: &str = "https://slsa.dev/provenance/v1";
const IN_TOTO_STATEMENT_V1: &str = "https://in-toto.io/Statement/v1";

/// An OCI image reference which can only be constructed by a successful provenance verifier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedArtifact {
    image_reference: String,
    repository: String,
}

impl VerifiedArtifact {
    pub fn image_reference(&self) -> &str {
        &self.image_reference
    }

    pub fn repository(&self) -> &str {
        &self.repository
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArtifactVerificationError {
    InvalidConfiguration,
    Unverified,
}

/// Concrete offline verifier for a GitHub artifact-attestation JSONL bundle.
#[derive(Clone)]
pub struct GitHubArtifactVerifier {
    image_reference: String,
    signer_identity: String,
    source_repository: String,
    source_commit: String,
    bundles: Vec<Bundle>,
}

impl GitHubArtifactVerifier {
    pub fn from_jsonl(
        image_reference: String,
        signer_identity: String,
        source_repository: String,
        source_commit: String,
        bundle_jsonl: &str,
    ) -> Result<Self, ArtifactVerificationError> {
        let _ = parse_digest_pinned(image_reference.clone())?;
        if signer_identity.trim().is_empty()
            || !valid_github_source(&source_repository, &source_commit, &signer_identity)
        {
            return Err(ArtifactVerificationError::InvalidConfiguration);
        }
        let bundles = bundle_jsonl
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(Bundle::from_json)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| ArtifactVerificationError::InvalidConfiguration)?;
        if bundles.is_empty() {
            return Err(ArtifactVerificationError::InvalidConfiguration);
        }
        Ok(Self {
            image_reference,
            signer_identity,
            source_repository,
            source_commit,
            bundles,
        })
    }

    pub fn verify(&self) -> Result<VerifiedArtifact, ArtifactVerificationError> {
        self.verify_with_issuer(GITHUB_ACTIONS_OIDC_ISSUER)
    }

    fn verify_with_issuer(
        &self,
        issuer: &str,
    ) -> Result<VerifiedArtifact, ArtifactVerificationError> {
        let artifact = parse_digest_pinned(self.image_reference.clone())?;
        let digest_hex = artifact
            .image_reference()
            .rsplit_once('@')
            .map(|(_, digest)| &digest["sha256:".len()..])
            .ok_or(ArtifactVerificationError::Unverified)?;
        let digest =
            Sha256Hash::from_hex(digest_hex).map_err(|_| ArtifactVerificationError::Unverified)?;
        let trusted_root = TrustedRoot::from_embedded(SigstoreInstance::PublicGood)
            .map_err(|_| ArtifactVerificationError::Unverified)?;
        let policy = VerificationPolicy::default()
            .require_identity(self.signer_identity.clone())
            .require_issuer(issuer)
            // GitHub's published bundles lack compatible SCT/tlog material for this offline
            // validation path. Certificate chain, signer, issuer, DSSE, and SLSA bindings stay
            // mandatory.
            .skip_sct()
            .skip_tlog();
        let verifier = Verifier::new(&trusted_root);
        if self.bundles.iter().any(|bundle| {
            github_slsa_provenance_binds_artifact(
                bundle,
                artifact.repository(),
                digest_hex,
                &self.signer_identity,
                &self.source_repository,
                &self.source_commit,
            ) && verifier.verify(digest, bundle, &policy).is_ok()
        }) {
            Ok(artifact)
        } else {
            Err(ArtifactVerificationError::Unverified)
        }
    }
}

fn parse_digest_pinned(
    image_reference: String,
) -> Result<VerifiedArtifact, ArtifactVerificationError> {
    let Some((repository, digest)) = image_reference.rsplit_once('@') else {
        return Err(ArtifactVerificationError::InvalidConfiguration);
    };
    if repository.is_empty()
        || !digest.starts_with("sha256:")
        || digest.len() != "sha256:".len() + 64
        || !digest["sha256:".len()..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ArtifactVerificationError::InvalidConfiguration);
    }
    let repository = repository.to_owned();
    Ok(VerifiedArtifact {
        image_reference,
        repository,
    })
}

fn github_slsa_provenance_binds_artifact(
    bundle: &Bundle,
    artifact_repository: &str,
    digest_hex: &str,
    signer_identity: &str,
    source_repository: &str,
    source_commit: &str,
) -> bool {
    let SignatureContent::DsseEnvelope(envelope) = &bundle.content else {
        return false;
    };
    let Ok(statement) = serde_json::from_slice::<Statement>(&envelope.decode_payload()) else {
        return false;
    };
    github_slsa_provenance_statement_binds_artifact(
        &statement,
        artifact_repository,
        digest_hex,
        signer_identity,
        source_repository,
        source_commit,
    )
}

fn github_slsa_provenance_statement_binds_artifact(
    statement: &Statement,
    artifact_repository: &str,
    digest_hex: &str,
    signer_identity: &str,
    source_repository: &str,
    source_commit: &str,
) -> bool {
    statement.type_ == IN_TOTO_STATEMENT_V1
        && statement.predicate_type == SLSA_PROVENANCE_V1
        && statement.subject.iter().any(|subject| {
            subject.name == artifact_repository
                && subject.digest.sha256.as_deref() == Some(digest_hex)
        })
        && statement
            .predicate
            .pointer("/runDetails/builder/id")
            .and_then(serde_json::Value::as_str)
            == Some(signer_identity)
        && statement
            .predicate
            .pointer("/buildDefinition/externalParameters/workflow/repository")
            .and_then(serde_json::Value::as_str)
            == Some(source_repository)
        && statement
            .predicate
            .pointer("/buildDefinition/resolvedDependencies")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|dependencies| {
                dependencies.iter().any(|dependency| {
                    let Some(uri) = dependency.get("uri").and_then(serde_json::Value::as_str)
                    else {
                        return false;
                    };
                    let Some(repository) = uri
                        .strip_prefix("git+")
                        .and_then(|value| value.rsplit_once('@').map(|(repository, _)| repository))
                    else {
                        return false;
                    };
                    repository == source_repository
                        && dependency
                            .pointer("/digest/gitCommit")
                            .and_then(serde_json::Value::as_str)
                            == Some(source_commit)
                })
            })
}

fn valid_github_source(
    source_repository: &str,
    source_commit: &str,
    signer_identity: &str,
) -> bool {
    let Some(path) = source_repository.strip_prefix("https://github.com/") else {
        return false;
    };
    if path.split('/').count() != 2 || path.split('/').any(str::is_empty) {
        return false;
    }
    if source_commit.len() != 40
        || !source_commit
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return false;
    }
    let prefix = format!("{source_repository}/.github/workflows/");
    signer_identity
        .strip_prefix(&prefix)
        .and_then(|workflow| workflow.rsplit_once('@'))
        .is_some_and(|(path, reference)| !path.is_empty() && !reference.is_empty())
}

#[cfg(test)]
mod tests {
    use sigstore_verify::types::{Bundle, SignatureContent, intoto::Statement};

    use super::{
        ArtifactVerificationError, GitHubArtifactVerifier, github_slsa_provenance_binds_artifact,
        github_slsa_provenance_statement_binds_artifact,
    };

    const PUBLIC_GITHUB_FIXTURE: &str = include_str!(
        "../../../crates/steward-apiserver/testdata/stable-runtime-bridge/github-cli-v2.64.0.b64"
    );
    const PUBLIC_GITHUB_ARTIFACT: &str = "gh_2.64.0_linux_amd64.tar.gz@sha256:0e44a4c43014bd513550ec190b7c33f5f8b63d162927a1f6445ef38ea25cd2fa";
    const PUBLIC_GITHUB_SIGNER: &str =
        "https://github.com/cli/cli/.github/workflows/deployment.yml@refs/heads/trunk";
    const PUBLIC_GITHUB_SOURCE: &str = "https://github.com/cli/cli";
    const PUBLIC_GITHUB_COMMIT: &str = "5402e207ee89f2f3dc52779c3edde632485074cd";
    const PUBLIC_GITHUB_DIGEST: &str =
        "0e44a4c43014bd513550ec190b7c33f5f8b63d162927a1f6445ef38ea25cd2fa";

    fn decode_base64(input: &str) -> Result<Vec<u8>, String> {
        fn value(byte: u8) -> Option<u8> {
            match byte {
                b'A'..=b'Z' => Some(byte - b'A'),
                b'a'..=b'z' => Some(byte - b'a' + 26),
                b'0'..=b'9' => Some(byte - b'0' + 52),
                b'+' => Some(62),
                b'/' => Some(63),
                _ => None,
            }
        }
        let encoded = input
            .bytes()
            .filter(|byte| !byte.is_ascii_whitespace())
            .collect::<Vec<_>>();
        if encoded.len() % 4 != 0 {
            return Err("fixture base64 length".to_owned());
        }
        let mut decoded = Vec::with_capacity(encoded.len() / 4 * 3);
        for group in encoded.chunks_exact(4) {
            let first = value(group[0]).ok_or_else(|| "fixture base64 character".to_owned())?;
            let second = value(group[1]).ok_or_else(|| "fixture base64 character".to_owned())?;
            let third = if group[2] == b'=' {
                None
            } else {
                Some(value(group[2]).ok_or_else(|| "fixture base64 character".to_owned())?)
            };
            let fourth = if group[3] == b'=' {
                None
            } else {
                Some(value(group[3]).ok_or_else(|| "fixture base64 character".to_owned())?)
            };
            if third.is_none() && fourth.is_some() {
                return Err("fixture base64 padding".to_owned());
            }
            decoded.push((first << 2) | (second >> 4));
            if let Some(third) = third {
                decoded.push((second << 4) | (third >> 2));
                if let Some(fourth) = fourth {
                    decoded.push((third << 6) | fourth);
                }
            }
        }
        Ok(decoded)
    }

    fn fixture() -> Result<String, String> {
        String::from_utf8(decode_base64(PUBLIC_GITHUB_FIXTURE)?)
            .map_err(|error| format!("fixture utf8: {error}"))
    }

    fn verifier() -> Result<GitHubArtifactVerifier, String> {
        GitHubArtifactVerifier::from_jsonl(
            PUBLIC_GITHUB_ARTIFACT.to_owned(),
            PUBLIC_GITHUB_SIGNER.to_owned(),
            PUBLIC_GITHUB_SOURCE.to_owned(),
            PUBLIC_GITHUB_COMMIT.to_owned(),
            &fixture()?,
        )
        .map_err(|error| format!("fixture verifier: {error:?}"))
    }

    #[test]
    fn verifier_rejects_mutable_image_before_parsing_the_bundle() {
        assert!(matches!(
            GitHubArtifactVerifier::from_jsonl(
                "registry.example.test/steward-bridge:latest".to_owned(),
                "https://github.com/example-org/steward/.github/workflows/release.yml@refs/tags/v0.1.0".to_owned(),
                "https://github.com/example-org/steward".to_owned(),
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
                "{}",
            ),
            Err(ArtifactVerificationError::InvalidConfiguration)
        ),
            "a mutable image must never reach artifact verification"
        );
    }

    #[test]
    fn verifier_rejects_unparseable_bundle() {
        assert!(matches!(
            GitHubArtifactVerifier::from_jsonl(
                "registry.example.test/steward-bridge@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
                "https://github.com/example-org/steward/.github/workflows/release.yml@refs/tags/v0.1.0".to_owned(),
                "https://github.com/example-org/steward".to_owned(),
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
                "not-json",
            ),
            Err(ArtifactVerificationError::InvalidConfiguration)
        ),
            "a digest alone must never activate a bridge image"
        );
    }

    #[test]
    fn verifier_accepts_the_real_signed_bundle_and_binds_the_exact_source() -> Result<(), String> {
        let artifact = verifier()?
            .verify()
            .map_err(|error| format!("verified public bundle rejected: {error:?}"))?;
        assert_eq!(artifact.image_reference(), PUBLIC_GITHUB_ARTIFACT);
        assert_eq!(artifact.repository(), "gh_2.64.0_linux_amd64.tar.gz");
        Ok(())
    }

    #[test]
    fn verifier_rejects_wrong_issuer_signer_and_digest() -> Result<(), String> {
        let verifier = verifier()?;
        assert_eq!(
            verifier.verify_with_issuer("https://issuer.example.test"),
            Err(ArtifactVerificationError::Unverified),
            "a signed bundle from the wrong issuer is never acceptable"
        );
        let wrong_signer = GitHubArtifactVerifier::from_jsonl(
            PUBLIC_GITHUB_ARTIFACT.to_owned(),
            "https://github.com/cli/cli/.github/workflows/other.yml@refs/heads/trunk".to_owned(),
            PUBLIC_GITHUB_SOURCE.to_owned(),
            PUBLIC_GITHUB_COMMIT.to_owned(),
            &fixture()?,
        )
        .map_err(|error| format!("wrong signer configuration: {error:?}"))?;
        assert_eq!(
            wrong_signer.verify(),
            Err(ArtifactVerificationError::Unverified)
        );
        let wrong_digest = GitHubArtifactVerifier::from_jsonl(
            "gh_2.64.0_linux_amd64.tar.gz@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
            PUBLIC_GITHUB_SIGNER.to_owned(),
            PUBLIC_GITHUB_SOURCE.to_owned(),
            PUBLIC_GITHUB_COMMIT.to_owned(),
            &fixture()?,
        )
        .map_err(|error| format!("wrong digest configuration: {error:?}"))?;
        assert_eq!(
            wrong_digest.verify(),
            Err(ArtifactVerificationError::Unverified)
        );
        Ok(())
    }

    #[test]
    fn provenance_binding_rejects_wrong_subject_predicate_and_source() -> Result<(), String> {
        let bundle = Bundle::from_json(&fixture()?).map_err(|error| format!("bundle: {error}"))?;
        assert!(github_slsa_provenance_binds_artifact(
            &bundle,
            "gh_2.64.0_linux_amd64.tar.gz",
            PUBLIC_GITHUB_DIGEST,
            PUBLIC_GITHUB_SIGNER,
            PUBLIC_GITHUB_SOURCE,
            PUBLIC_GITHUB_COMMIT,
        ));
        let SignatureContent::DsseEnvelope(envelope) = &bundle.content else {
            return Err("fixture envelope".to_owned());
        };
        let mut statement: Statement = serde_json::from_slice(&envelope.decode_payload())
            .map_err(|error| format!("statement: {error}"))?;
        let subject = statement
            .subject
            .iter_mut()
            .find(|subject| {
                subject.name == "gh_2.64.0_linux_amd64.tar.gz"
                    && subject.digest.sha256.as_deref() == Some(PUBLIC_GITHUB_DIGEST)
            })
            .ok_or_else(|| "fixture artifact subject".to_owned())?;
        subject.name = "wrong-subject".to_owned();
        assert!(
            !github_slsa_provenance_statement_binds_artifact(
                &statement,
                "gh_2.64.0_linux_amd64.tar.gz",
                PUBLIC_GITHUB_DIGEST,
                PUBLIC_GITHUB_SIGNER,
                PUBLIC_GITHUB_SOURCE,
                PUBLIC_GITHUB_COMMIT,
            ),
            "the subject binding is exact"
        );

        let statement: Statement = serde_json::from_slice(&envelope.decode_payload())
            .map_err(|error| format!("statement: {error}"))?;
        let mut wrong_predicate = statement.clone();
        wrong_predicate.predicate_type = "https://example.test/not-slsa".to_owned();
        assert!(
            !github_slsa_provenance_statement_binds_artifact(
                &wrong_predicate,
                "gh_2.64.0_linux_amd64.tar.gz",
                PUBLIC_GITHUB_DIGEST,
                PUBLIC_GITHUB_SIGNER,
                PUBLIC_GITHUB_SOURCE,
                PUBLIC_GITHUB_COMMIT,
            ),
            "only the SLSA v1 predicate is accepted"
        );
        let mut wrong_source = statement;
        let dependencies = wrong_source
            .predicate
            .pointer_mut("/buildDefinition/resolvedDependencies")
            .and_then(serde_json::Value::as_array_mut)
            .ok_or_else(|| "fixture dependencies".to_owned())?;
        dependencies[0]["uri"] = serde_json::Value::String(
            "git+https://github.com/example-org/other@deadbeef".to_owned(),
        );
        assert!(
            !github_slsa_provenance_statement_binds_artifact(
                &wrong_source,
                "gh_2.64.0_linux_amd64.tar.gz",
                PUBLIC_GITHUB_DIGEST,
                PUBLIC_GITHUB_SIGNER,
                PUBLIC_GITHUB_SOURCE,
                PUBLIC_GITHUB_COMMIT,
            ),
            "the resolved source repository and commit binding is exact"
        );
        Ok(())
    }
}
