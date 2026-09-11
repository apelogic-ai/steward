use std::path::Path;

use serde::de::DeserializeOwned;
use steward_types::direct_package::{
    DirectTaskBindingEvidence, DirectTaskDefinition, DirectTaskStatusResponse,
    DirectTaskSubmission, ExecutionLogMode, InstructionSkill, InvocationManifest, PackageClosure,
    RepositoryUrl, SOURCE_PROVENANCE_JWT_CLAIM, SourceProvenance, canonical_json_bytes,
};

const CONTRACT_ROOT: &str = "../../docs/contracts/task/v2";

fn fixture(path: &str) -> Result<String, String> {
    std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(CONTRACT_ROOT)
            .join(path),
    )
    .map_err(|error| format!("fixture {path} must be readable: {error}"))
}

fn parse<T: DeserializeOwned>(path: &str) -> Result<T, String> {
    serde_json::from_str(&fixture(path)?)
        .map_err(|error| format!("fixture {path} must parse: {error}"))
}

#[test]
fn direct_package_v2_contract_is_published() -> Result<(), String> {
    let contract =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/contracts/task/v2/manifest.json");

    if !contract.is_file() {
        return Err("the accepted direct-package v2 contract must be published".to_owned());
    }
    Ok(())
}

#[test]
fn positive_wire_fixtures_parse_and_validate() -> Result<(), String> {
    let submission: DirectTaskSubmission = parse("fixtures/positive/direct-task-submission.json")?;
    submission.validate()?;

    let invoking_repository = RepositoryUrl::parse("https://github.com/example-org/caller.git")?;
    let exact: InvocationManifest = parse("fixtures/positive/invocation-manifest.json")?;
    exact.validate_for_invoking_repository(&invoking_repository)?;
    assert_eq!(
        exact.effective_diagnostics().execution_log,
        ExecutionLogMode::Full
    );

    let trigger: InvocationManifest = parse("fixtures/positive/invocation-manifest-trigger.json")?;
    trigger.validate_for_invoking_repository(&invoking_repository)?;
    assert_eq!(
        trigger.effective_diagnostics().execution_log,
        ExecutionLogMode::Off
    );

    let status: DirectTaskStatusResponse = parse("fixtures/positive/direct-task-status.json")?;
    status.validate()?;

    let minimal: DirectTaskDefinition = parse("fixtures/positive/task-definition-no-skills.json")?;
    minimal.validate()?;
    assert!(minimal.skills.is_empty());
    assert!(minimal.requires.is_none());

    let narrowed: DirectTaskDefinition =
        parse("fixtures/positive/task-definition-with-requires.json")?;
    narrowed.validate()?;
    let Some(requirements) = narrowed.requires.as_ref() else {
        return Err("narrowed fixture must contain requirements".to_owned());
    };
    requirements.validate()?;

    let skill: InstructionSkill = parse("fixtures/positive/instruction-skill-default-kind.json")?;
    skill.validate()?;

    let provenance: SourceProvenance = parse("fixtures/positive/source-provenance.json")?;
    provenance.validate()?;
    assert_eq!(SOURCE_PROVENANCE_JWT_CLAIM, "source_provenance");

    let closure: PackageClosure = parse("fixtures/positive/package-closure.json")?;
    closure.validate()?;

    let evidence: DirectTaskBindingEvidence =
        parse("fixtures/positive/task-binding-evidence.json")?;
    evidence.validate()?;
    Ok(())
}

#[test]
fn malformed_and_privilege_bearing_inputs_fail_closed() -> Result<(), String> {
    let bad_submission: DirectTaskSubmission =
        parse("fixtures/negative/submission-unknown-contract.json")?;
    assert!(bad_submission.validate().is_err());

    for path in [
        "fixtures/negative/invocation-unknown-field.json",
        "fixtures/negative/invocation-mutable-ref.json",
        "fixtures/negative/invocation-path-traversal.json",
        "fixtures/negative/invocation-unknown-diagnostics.json",
    ] {
        assert!(
            serde_json::from_str::<InvocationManifest>(&fixture(path)?).is_err(),
            "{path}"
        );
    }

    let cross_source: InvocationManifest =
        parse("fixtures/negative/invocation-trigger-cross-repository.json")?;
    let invoking_repository = RepositoryUrl::parse("https://github.com/example-org/caller.git")?;
    assert!(
        cross_source
            .validate_for_invoking_repository(&invoking_repository)
            .is_err()
    );

    assert!(
        serde_json::from_str::<DirectTaskDefinition>(&fixture(
            "fixtures/negative/task-definition-partial-requires.json"
        )?)
        .is_err()
    );
    let duplicate_skills: DirectTaskDefinition =
        parse("fixtures/negative/task-definition-duplicate-skills.json")?;
    assert!(duplicate_skills.validate().is_err());
    let escaped_output: DirectTaskDefinition =
        parse("fixtures/negative/task-definition-output-escape.json")?;
    assert!(escaped_output.validate().is_err());
    assert!(
        serde_json::from_str::<InstructionSkill>(&fixture(
            "fixtures/negative/instruction-skill-executable.json"
        )?)
        .is_err()
    );
    for path in [
        "fixtures/negative/package-closure-duplicate-path.json",
        "fixtures/negative/package-closure-missing-task-definition.json",
        "fixtures/negative/package-closure-mismatched-task-definition.json",
    ] {
        let closure: PackageClosure = parse(path)?;
        assert!(closure.validate().is_err(), "{path}");
    }
    assert!(
        serde_json::from_str::<SourceProvenance>(&fixture(
            "fixtures/negative/source-provenance-token-injection.json"
        )?)
        .is_err()
    );
    Ok(())
}

#[test]
fn closure_canonicalization_matches_the_published_vector() -> Result<(), String> {
    let closure: PackageClosure = parse("fixtures/positive/package-closure.json")?;
    let actual = canonical_json_bytes(&closure)?;
    let expected = fixture("vectors/package-closure.canonical.json")?;
    assert_eq!(actual, expected.trim_end().as_bytes());
    Ok(())
}

#[test]
fn frozen_v1_fixture_remains_separate_and_unmodified() -> Result<(), String> {
    let frozen: serde_json::Value = parse("fixtures/compatibility/frozen-v1-task-definition.json")?;
    let authoritative: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../docs/contracts/m1/v1/fixtures/positive/task-definition.json"),
        )
        .map_err(|error| format!("frozen v1 fixture must be readable: {error}"))?,
    )
    .map_err(|error| format!("frozen v1 fixture must parse: {error}"))?;
    assert_eq!(frozen, authoritative);
    assert!(serde_json::from_value::<DirectTaskDefinition>(frozen).is_err());
    Ok(())
}
