use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use steward_types::direct_package::{
    AgentRef, ContentDigest, Decimal, DirectTaskBindingEvidence, DirectTaskDefinition,
    DirectTaskStatusResponse, DirectTaskSubmission, Duration, ExactGitCommit, ExecutionLogMode,
    InstructionSkill, InvocationManifest, PackageClosure, RelativePath, RepositoryUrl,
    SOURCE_PROVENANCE_JWT_CLAIM, Slug, SourceProvenance, StableProviderId, Uuid,
    canonical_json_bytes,
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

fn parse_value(path: &str) -> Result<serde_json::Value, String> {
    parse(path)
}

fn contract_path(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(CONTRACT_ROOT)
        .join(path)
}

fn collect_json_paths(
    directory: &Path,
    root: &Path,
    paths: &mut BTreeSet<String>,
) -> Result<(), String> {
    for entry in std::fs::read_dir(directory).map_err(|error| {
        format!(
            "fixture directory {} must be readable: {error}",
            directory.display()
        )
    })? {
        let entry =
            entry.map_err(|error| format!("fixture directory entry must be readable: {error}"))?;
        let path = entry.path();
        if path.is_dir() {
            collect_json_paths(&path, root, paths)?;
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("json") {
            let relative = path.strip_prefix(root).map_err(|error| {
                format!("fixture path must remain beneath contract root: {error}")
            })?;
            paths.insert(relative.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}

fn rust_contract_accepts(definition: &str, value: serde_json::Value) -> Result<bool, String> {
    let result = match definition {
        "directTaskSubmission" => serde_json::from_value::<DirectTaskSubmission>(value)
            .map_err(|error| error.to_string())
            .and_then(|contract| contract.validate()),
        "invocationManifest" => serde_json::from_value::<InvocationManifest>(value)
            .map_err(|error| error.to_string())
            .and_then(|contract| {
                contract.validate_for_invoking_repository(&RepositoryUrl::parse(
                    "https://github.com/example-org/caller.git",
                )?)
            }),
        "directTaskStatusResponse" => serde_json::from_value::<DirectTaskStatusResponse>(value)
            .map_err(|error| error.to_string())
            .and_then(|contract| contract.validate()),
        "taskDefinition" => serde_json::from_value::<DirectTaskDefinition>(value)
            .map_err(|error| error.to_string())
            .and_then(|contract| contract.validate()),
        "instructionSkill" => serde_json::from_value::<InstructionSkill>(value)
            .map_err(|error| error.to_string())
            .and_then(|contract| contract.validate()),
        "sourceProvenance" => serde_json::from_value::<SourceProvenance>(value)
            .map_err(|error| error.to_string())
            .and_then(|contract| contract.validate()),
        "packageClosure" => serde_json::from_value::<PackageClosure>(value)
            .map_err(|error| error.to_string())
            .and_then(|contract| contract.validate()),
        "taskBindingEvidence" => serde_json::from_value::<DirectTaskBindingEvidence>(value)
            .map_err(|error| error.to_string())
            .and_then(|contract| contract.validate()),
        _ => return Err(format!("manifest uses unknown definition {definition}")),
    };
    Ok(result.is_ok())
}

fn validate_schema_instance(
    root: &serde_json::Value,
    schema: &serde_json::Value,
    instance: &serde_json::Value,
    path: &str,
) -> Result<(), String> {
    let schema = schema
        .as_object()
        .ok_or_else(|| format!("{path}: schema must be an object"))?;
    let allowed = [
        "$schema",
        "$id",
        "$defs",
        "$ref",
        "title",
        "type",
        "const",
        "enum",
        "oneOf",
        "required",
        "properties",
        "additionalProperties",
        "items",
        "minItems",
        "maxItems",
        "uniqueItems",
        "minLength",
        "maxLength",
        "pattern",
        "minimum",
        "maximum",
        "default",
    ];
    for keyword in schema.keys() {
        if !allowed.contains(&keyword.as_str()) {
            return Err(format!("{path}: unsupported schema keyword {keyword}"));
        }
    }

    if let Some(reference) = schema.get("$ref").and_then(serde_json::Value::as_str) {
        let pointer = reference
            .strip_prefix('#')
            .ok_or_else(|| format!("{path}: only local schema refs are supported"))?;
        let resolved = root
            .pointer(pointer)
            .ok_or_else(|| format!("{path}: schema ref {reference} does not resolve"))?;
        validate_schema_instance(root, resolved, instance, path)?;
    }
    if let Some(expected) = schema.get("const")
        && instance != expected
    {
        return Err(format!("{path}: value differs from required constant"));
    }
    if let Some(values) = schema.get("enum").and_then(serde_json::Value::as_array)
        && !values.contains(instance)
    {
        return Err(format!("{path}: value is outside the enum"));
    }
    if let Some(kind) = schema.get("type").and_then(serde_json::Value::as_str) {
        let matches = match kind {
            "object" => instance.is_object(),
            "array" => instance.is_array(),
            "string" => instance.is_string(),
            "integer" => instance.as_i64().is_some() || instance.as_u64().is_some(),
            "boolean" => instance.is_boolean(),
            "null" => instance.is_null(),
            _ => return Err(format!("{path}: unsupported schema type {kind}")),
        };
        if !matches {
            return Err(format!("{path}: value has the wrong JSON type"));
        }
    }
    if let Some(branches) = schema.get("oneOf") {
        let branches = branches
            .as_array()
            .ok_or_else(|| format!("{path}: oneOf must be an array"))?;
        let matched = branches
            .iter()
            .filter(|branch| validate_schema_instance(root, branch, instance, path).is_ok())
            .count();
        if matched != 1 {
            return Err(format!("{path}: value matched {matched} oneOf branches"));
        }
    }

    if let Some(object) = instance.as_object() {
        if let Some(required) = schema.get("required").and_then(serde_json::Value::as_array) {
            for field in required {
                let field = field
                    .as_str()
                    .ok_or_else(|| format!("{path}: required field names must be strings"))?;
                if !object.contains_key(field) {
                    return Err(format!("{path}: missing required field {field}"));
                }
            }
        }
        let properties = schema
            .get("properties")
            .and_then(serde_json::Value::as_object);
        for (field, value) in object {
            if let Some(field_schema) = properties.and_then(|properties| properties.get(field)) {
                validate_schema_instance(root, field_schema, value, &format!("{path}/{field}"))?;
            } else if schema.get("additionalProperties") == Some(&serde_json::Value::Bool(false)) {
                return Err(format!("{path}: unknown field {field}"));
            }
        }
    }

    if let Some(array) = instance.as_array() {
        if let Some(minimum) = schema.get("minItems").and_then(serde_json::Value::as_u64)
            && array.len() < minimum as usize
        {
            return Err(format!("{path}: too few array items"));
        }
        if let Some(maximum) = schema.get("maxItems").and_then(serde_json::Value::as_u64)
            && array.len() > maximum as usize
        {
            return Err(format!("{path}: too many array items"));
        }
        if schema.get("uniqueItems") == Some(&serde_json::Value::Bool(true)) {
            for (index, value) in array.iter().enumerate() {
                if array[..index].contains(value) {
                    return Err(format!("{path}/{index}: duplicate array item"));
                }
            }
        }
        if let Some(item_schema) = schema.get("items") {
            for (index, value) in array.iter().enumerate() {
                validate_schema_instance(root, item_schema, value, &format!("{path}/{index}"))?;
            }
        }
    }

    if let Some(value) = instance.as_str() {
        if let Some(minimum) = schema.get("minLength").and_then(serde_json::Value::as_u64)
            && value.chars().count() < minimum as usize
        {
            return Err(format!("{path}: string is too short"));
        }
        if let Some(maximum) = schema.get("maxLength").and_then(serde_json::Value::as_u64)
            && value.chars().count() > maximum as usize
        {
            return Err(format!("{path}: string is too long"));
        }
        if let Some(pattern) = schema.get("pattern").and_then(serde_json::Value::as_str)
            && !matches_schema_pattern(pattern, value)?
        {
            return Err(format!("{path}: string does not match its pattern"));
        }
    }

    if let Some(value) = instance.as_u64() {
        if let Some(minimum) = schema.get("minimum").and_then(serde_json::Value::as_u64)
            && value < minimum
        {
            return Err(format!("{path}: integer is below its minimum"));
        }
        if let Some(maximum) = schema.get("maximum").and_then(serde_json::Value::as_u64)
            && value > maximum
        {
            return Err(format!("{path}: integer is above its maximum"));
        }
    }
    Ok(())
}

fn matches_schema_pattern(pattern: &str, value: &str) -> Result<bool, String> {
    let matches = match pattern {
        "^(?!/)(?!.*(?:^|/)\\.{1,2}(?:/|$))(?!.*//)[A-Za-z0-9._-]+(?:/[A-Za-z0-9._-]+)*$" => {
            RelativePath::parse(value).is_ok()
        }
        "^https://(?:[a-z0-9](?:[a-z0-9-]*[a-z0-9])?\\.)+[a-z0-9](?:[a-z0-9-]*[a-z0-9])?/(?!\\.{1,2}(?:/|\\.git$))[A-Za-z0-9._-]+(?:/(?!\\.{1,2}(?:/|\\.git$))[A-Za-z0-9._-]+)+\\.git$" => {
            RepositoryUrl::parse(value).is_ok()
        }
        "^git:sha1:[0-9a-f]{40}$" => ExactGitCommit::parse(value).is_ok(),
        "^steward:sha256:[0-9a-f]{64}$" => ContentDigest::parse(value).is_ok(),
        "^[0-9]{1,20}$" => StableProviderId::parse(value).is_ok(),
        "^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$" => {
            Uuid::parse(value).is_ok()
        }
        "^[a-z][a-z0-9-]{0,62}$" => Slug::parse(value).is_ok(),
        "^\\S(?:.*\\S)?$" => {
            !value.is_empty()
                && value
                    .chars()
                    .next()
                    .is_some_and(|character| !character.is_whitespace())
                && value
                    .chars()
                    .last()
                    .is_some_and(|character| !character.is_whitespace())
                && !value.contains(['\n', '\r', '\u{2028}', '\u{2029}'])
        }
        concat!("^[a-z][a-z0-9._-]*", "\x40", "[0-9][a-z0-9._+\\-]*$") => {
            AgentRef::parse(value).is_ok()
        }
        "^(0|[1-9][0-9]*)(\\.[0-9]{1,6})?$" => Decimal::parse(value).is_ok(),
        "^[1-9][0-9]*(?:s|m|h)$" => Duration::parse(value).is_ok(),
        "^[A-Z]{3}$" => value.len() == 3 && value.bytes().all(|byte| byte.is_ascii_uppercase()),
        "^out(?:/[A-Za-z0-9._-]+)*$" => {
            RelativePath::parse(value).is_ok() && (value == "out" || value.starts_with("out/"))
        }
        _ => return Err(format!("unsupported schema pattern {pattern}")),
    };
    Ok(matches)
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
fn manifest_registers_every_fixture_against_a_resolved_schema_definition() -> Result<(), String> {
    let manifest = parse_value(&["manifest", "json"].join("."))?;
    let schema_path = manifest
        .get("schema")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "manifest schema path must be a string".to_owned())?;
    let schema = parse_value(schema_path)?;

    let mut definitions = BTreeMap::new();
    for definition in manifest
        .get("definitions")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "manifest definitions must be an array".to_owned())?
    {
        let name = definition
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "manifest definition name must be a string".to_owned())?;
        let reference = definition
            .get("ref")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("manifest definition {name} ref must be a string"))?;
        let pointer = reference
            .strip_prefix('#')
            .ok_or_else(|| format!("manifest definition {name} must use a local schema ref"))?;
        if schema.pointer(pointer).is_none() {
            return Err(format!("manifest definition {name} ref does not resolve"));
        }
        if definitions.insert(name, reference).is_some() {
            return Err(format!("manifest definition {name} is duplicated"));
        }
    }

    let mut declared_paths = BTreeSet::new();
    for declared_fixture in manifest
        .get("fixtures")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "manifest fixtures must be an array".to_owned())?
    {
        let path = declared_fixture
            .get("path")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "manifest fixture path must be a string".to_owned())?;
        let definition = declared_fixture
            .get("definition")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("manifest fixture {path} definition must be a string"))?;
        let expected = declared_fixture
            .get("valid")
            .and_then(serde_json::Value::as_bool)
            .ok_or_else(|| format!("manifest fixture {path} validity must be a boolean"))?;
        if !definitions.contains_key(definition) {
            return Err(format!(
                "manifest fixture {path} uses unknown definition {definition}"
            ));
        }
        if !declared_paths.insert(path.to_owned()) {
            return Err(format!("manifest fixture path {path} is duplicated"));
        }
        let value = parse_value(path)?;
        let reference = definitions
            .get(definition)
            .ok_or_else(|| format!("manifest fixture {path} schema ref must exist"))?;
        let definition_schema = schema
            .pointer(
                reference
                    .strip_prefix('#')
                    .ok_or_else(|| format!("manifest definition {definition} must be local"))?,
            )
            .ok_or_else(|| format!("manifest definition {definition} ref must resolve"))?;
        let schema_accepts =
            validate_schema_instance(&schema, definition_schema, &value, "$fixture").is_ok();
        let semantic = declared_fixture.get("semantic").is_some();
        if schema_accepts != (expected || semantic) {
            return Err(format!(
                "fixture {path} JSON Schema validity for {definition}: expected {}, got {schema_accepts}",
                expected || semantic
            ));
        }
        let actual = rust_contract_accepts(definition, value)?;
        if actual != expected {
            return Err(format!(
                "fixture {path} validity for schema definition {definition}: expected {expected}, got {actual}"
            ));
        }
    }

    let root = contract_path("");
    let mut repository_paths = BTreeSet::new();
    collect_json_paths(&root.join("fixtures"), &root, &mut repository_paths)?;
    if declared_paths != repository_paths {
        return Err(format!(
            "manifest fixture inventory differs from repository fixtures: declared={declared_paths:?}, repository={repository_paths:?}"
        ));
    }
    Ok(())
}

#[test]
fn schema_and_rust_keep_high_risk_constraints_in_parity() -> Result<(), String> {
    let schema = parse_value("schemas/direct-package.schema.json")?;
    let required = schema
        .pointer("/$defs/directTaskStatusResponse/required")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "status schema required fields must be an array".to_owned())?;
    assert!(required.contains(&serde_json::json!("runtimeUid")));
    assert_eq!(
        schema.pointer("/$defs/envelopeEvidence/properties/revision/minimum"),
        Some(&serde_json::json!(1))
    );
    assert_eq!(
        schema.pointer("/$defs/taskDefinition/properties/outputs/maxItems"),
        Some(&serde_json::json!(32))
    );
    for pointer in [
        "/$defs/authorityRequirements/properties/llms/uniqueItems",
        "/$defs/authorityRequirements/properties/tools/uniqueItems",
        "/$defs/runnerRequirement/properties/platforms/uniqueItems",
    ] {
        assert_eq!(
            schema.pointer(pointer),
            Some(&serde_json::Value::Bool(true))
        );
    }
    assert_eq!(
        schema.pointer("/$defs/repositoryUrl/pattern"),
        Some(&serde_json::json!(
            "^https://(?:[a-z0-9](?:[a-z0-9-]*[a-z0-9])?\\.)+[a-z0-9](?:[a-z0-9-]*[a-z0-9])?/(?!\\.{1,2}(?:/|\\.git$))[A-Za-z0-9._-]+(?:/(?!\\.{1,2}(?:/|\\.git$))[A-Za-z0-9._-]+)+\\.git$"
        ))
    );
    assert!(
        schema
            .pointer("/$defs/directRequirements/properties/execution")
            .is_none()
    );
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
    let evidence_json = serde_json::to_value(&evidence)
        .map_err(|error| format!("evidence must serialize: {error}"))?;
    for path in [
        "/effectiveRequirements/authority/budget/singleRunLimit",
        "/effectiveRequirements/authority/runner/memory",
        "/effectiveRequirements/authority/runner/compute",
        "/effectiveRequirements/authority/runner/storage",
    ] {
        assert_eq!(evidence_json.pointer(path), Some(&serde_json::Value::Null));
    }
    Ok(())
}

#[test]
fn direct_status_exposes_matching_source_authority_evidence() -> Result<(), String> {
    let status: DirectTaskStatusResponse = parse("fixtures/positive/direct-task-status.json")?;
    status.validate()?;

    let mut mismatched_task =
        serde_json::to_value(&status).map_err(|error| format!("status must serialize: {error}"))?;
    mismatched_task["taskUid"] = serde_json::json!("33333333-3333-4333-8333-333333333333");
    let mismatched_task: DirectTaskStatusResponse = serde_json::from_value(mismatched_task)
        .map_err(|error| format!("mismatched Task status must parse: {error}"))?;
    assert!(mismatched_task.validate().is_err());

    let mut mismatched_diagnostics =
        serde_json::to_value(&status).map_err(|error| format!("status must serialize: {error}"))?;
    mismatched_diagnostics["diagnostics"] = serde_json::json!({"executionLog": "off"});
    let mismatched_diagnostics: DirectTaskStatusResponse =
        serde_json::from_value(mismatched_diagnostics)
            .map_err(|error| format!("mismatched diagnostic status must parse: {error}"))?;
    assert!(mismatched_diagnostics.validate().is_err());
    Ok(())
}

#[test]
fn binding_evidence_rejects_tampered_source_and_package_relationships() -> Result<(), String> {
    let evidence: DirectTaskBindingEvidence =
        parse("fixtures/positive/task-binding-evidence.json")?;
    evidence.validate()?;

    let cases = [
        (
            "/closureDigest",
            serde_json::json!(
                "steward:sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
            ),
        ),
        (
            "/package/path",
            serde_json::json!("catalog/release-summary/v1/other-task.json"),
        ),
        (
            "/package/contentDigest",
            serde_json::json!(
                "steward:sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
            ),
        ),
        ("/invocation/repositoryId", serde_json::json!("999999")),
        ("/invocation/repositoryOwnerId", serde_json::json!("999999")),
        (
            "/invocation/commit",
            serde_json::json!("git:sha1:ffffffffffffffffffffffffffffffffffffffff"),
        ),
        (
            "/invocation/repository",
            serde_json::json!("https://github.com/example-org/other.git"),
        ),
        (
            "/invocation/repository",
            serde_json::json!("https://git.example.com/example-org/caller.git"),
        ),
        ("/envelope/revision", serde_json::json!(0)),
    ];

    for (pointer, replacement) in cases {
        let mut tampered = serde_json::to_value(&evidence)
            .map_err(|error| format!("evidence must serialize: {error}"))?;
        let target = tampered
            .pointer_mut(pointer)
            .ok_or_else(|| format!("test pointer {pointer} must exist"))?;
        *target = replacement;
        let tampered: DirectTaskBindingEvidence = serde_json::from_value(tampered)
            .map_err(|error| format!("tampered evidence at {pointer} must parse: {error}"))?;
        assert!(tampered.validate().is_err(), "tampering at {pointer}");
    }
    Ok(())
}

#[test]
fn task_definition_validation_matches_requires_and_output_schema_bounds() -> Result<(), String> {
    let definition = parse_value("fixtures/positive/task-definition-with-requires.json")?;
    let schema = parse_value("schemas/direct-package.schema.json")?;
    let definition_schema = schema
        .pointer("/$defs/taskDefinition")
        .ok_or_else(|| "TaskDefinition schema must exist".to_owned())?;

    for (collection, pointer) in [
        ("llms", "/requires/authority/llms"),
        ("tools", "/requires/authority/tools"),
        ("platforms", "/requires/authority/runner/platforms"),
    ] {
        let mut duplicate = definition.clone();
        let values = duplicate
            .pointer_mut(pointer)
            .and_then(serde_json::Value::as_array_mut)
            .ok_or_else(|| format!("test collection {pointer} must exist"))?;
        let first = values
            .first()
            .cloned()
            .ok_or_else(|| format!("test collection {pointer} must not be empty"))?;
        values.push(first);
        assert!(validate_schema_instance(&schema, definition_schema, &duplicate, "$task").is_err());
        let duplicate: DirectTaskDefinition = serde_json::from_value(duplicate)
            .map_err(|error| format!("duplicate {collection} definition must parse: {error}"))?;
        assert!(duplicate.validate().is_err(), "duplicate {collection}");
    }

    let mut too_many_outputs = definition;
    let outputs = too_many_outputs
        .pointer_mut("/outputs")
        .and_then(serde_json::Value::as_array_mut)
        .ok_or_else(|| "test outputs must exist".to_owned())?;
    let template = outputs
        .first()
        .cloned()
        .ok_or_else(|| "test outputs must not be empty".to_owned())?;
    while outputs.len() <= 32 {
        let mut output = template.clone();
        output["path"] = serde_json::json!(format!("out/result-{}.md", outputs.len()));
        outputs.push(output);
    }
    assert!(
        validate_schema_instance(&schema, definition_schema, &too_many_outputs, "$task").is_err()
    );
    let too_many_outputs: DirectTaskDefinition = serde_json::from_value(too_many_outputs)
        .map_err(|error| format!("oversized output definition must parse: {error}"))?;
    assert!(too_many_outputs.validate().is_err());
    Ok(())
}

#[test]
fn rust_parser_matches_required_and_canonical_schema_forms() -> Result<(), String> {
    let schema = parse_value("schemas/direct-package.schema.json")?;
    let repository_schema = schema
        .pointer("/$defs/repositoryUrl")
        .ok_or_else(|| "repository URL schema must exist".to_owned())?;
    for repository in [
        "https://-git.example.com/example-org/caller.git",
        "https://.git.example.com/example-org/caller.git",
        "https://git..example.com/example-org/caller.git",
        "https://git.example.com/example-org/../caller.git",
    ] {
        assert!(RepositoryUrl::parse(repository).is_err(), "{repository}");
        assert!(
            validate_schema_instance(
                &schema,
                repository_schema,
                &serde_json::json!(repository),
                "$repository"
            )
            .is_err(),
            "{repository}"
        );
    }
    let repository = "https://git.example.com/example-org/caller.git";
    RepositoryUrl::parse(repository)?;
    validate_schema_instance(
        &schema,
        repository_schema,
        &serde_json::json!(repository),
        "$repository",
    )?;

    assert!(Duration::parse("01h").is_err());
    assert!(
        validate_schema_instance(
            &schema,
            schema
                .pointer("/$defs/authorityRequirements/properties/ttl")
                .ok_or_else(|| "authority TTL schema must exist".to_owned())?,
            &serde_json::json!("01h"),
            "$ttl",
        )
        .is_err()
    );
    Duration::parse("1h")?;

    let mut status = parse_value("fixtures/positive/direct-task-status.json")?;
    status
        .as_object_mut()
        .ok_or_else(|| "status fixture must be an object".to_owned())?
        .remove("runtimeUid");
    assert!(serde_json::from_value::<DirectTaskStatusResponse>(status.clone()).is_err());
    assert!(
        validate_schema_instance(
            &schema,
            schema
                .pointer("/$defs/directTaskStatusResponse")
                .ok_or_else(|| "direct Task status schema must exist".to_owned())?,
            &status,
            "$status",
        )
        .is_err()
    );

    let mut evidence = parse_value("fixtures/positive/task-binding-evidence.json")?;
    evidence["envelope"]["revision"] = serde_json::json!(0);
    let evidence_contract: DirectTaskBindingEvidence = serde_json::from_value(evidence.clone())
        .map_err(|error| format!("zero-revision evidence must parse: {error}"))?;
    assert!(evidence_contract.validate().is_err());
    assert!(
        validate_schema_instance(
            &schema,
            schema
                .pointer("/$defs/taskBindingEvidence")
                .ok_or_else(|| "Task binding evidence schema must exist".to_owned())?,
            &evidence,
            "$evidence",
        )
        .is_err()
    );
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
    for path in [
        "fixtures/negative/task-definition-execution-capability.json",
        "fixtures/negative/task-definition-missing-optional-maxima.json",
    ] {
        assert!(
            serde_json::from_str::<DirectTaskDefinition>(&fixture(path)?).is_err(),
            "{path}"
        );
    }
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
    let digest = parse_value("vectors/package-closure-digest.json")?;
    let actual_digest = format!("steward:sha256:{:x}", Sha256::digest(actual));
    assert_eq!(
        digest.get("digest").and_then(serde_json::Value::as_str),
        Some(actual_digest.as_str())
    );
    Ok(())
}

#[test]
fn frozen_v1_fixture_remains_separate_and_unmodified() -> Result<(), String> {
    let frozen = fixture("fixtures/compatibility/frozen-v1-task-definition.json")?;
    let authoritative = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/contracts/m1/v1/fixtures/positive/task-definition.json"),
    )
    .map_err(|error| format!("authoritative v1 fixture must be readable: {error}"))?;
    assert_eq!(frozen.as_bytes(), authoritative.as_bytes());
    let frozen: serde_json::Value = serde_json::from_str(&frozen)
        .map_err(|error| format!("frozen v1 fixture must parse: {error}"))?;
    assert!(serde_json::from_value::<DirectTaskDefinition>(frozen).is_err());
    Ok(())
}
