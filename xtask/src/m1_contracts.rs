use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path};

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};

const CONTRACT_MANIFEST_SCHEMA: &str = "steward.contracts/manifest/v1";
const CONTRACT_VERSION: &str = "steward.m1/v1";
const JSON_SCHEMA_DIALECT: &str = "https://json-schema.org/draft/2020-12/schema";

const QUALIFIED_WORKFLOW_PATTERN: &str = "^[a-z][a-z0-9-]{0,62}/[a-z][a-z0-9-]{0,62}@[1-9][0-9]*$";
const VERSIONED_NAME_PATTERN: &str = "^[a-z][a-z0-9-]{0,62}@[1-9][0-9]*$";
const UUID_PATTERN: &str =
    "^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$";
const SHA256_PATTERN: &str = "^sha256:[0-9a-f]{64}$";
const GIT_SHA_PATTERN: &str = "^[0-9a-f]{40}$";
const SOURCE_ID_PATTERN: &str = "^[1-9][0-9]{0,19}$";
const CANONICAL_USER_ID_PATTERN: &str = "^usr_[0-9a-f]{32}$";
const CATALOG_ALIAS_PATTERN: &str = "^[a-z][a-z0-9-]{0,62}$";
const CAPABILITY_PATTERN: &str = "^[a-z][a-z0-9-]{0,31}:[a-z][a-z0-9._-]{0,95}$";
const RELATIVE_PATH_PATTERN: &str =
    "^(?!.*(?:^|/)\\.\\.?(?:/|$))[A-Za-z0-9._-]+(?:/[A-Za-z0-9._-]+)*$";
const OPAQUE_REFERENCE_PATTERN: &str = "^[a-z][a-z0-9-]{0,31}:[A-Za-z0-9_-]{8,128}$";
const RFC3339_UTC_PATTERN: &str = "^[0-9]{4}-(?:0[1-9]|1[0-2])-(?:0[1-9]|[12][0-9]|3[01])T(?:[01][0-9]|2[0-3]):[0-5][0-9]:(?:[0-5][0-9]|60)(?:\\.[0-9]+)?Z$";
const BASE64URL_PATTERN: &str = "^[A-Za-z0-9_-]+$";
const DECIMAL_PATTERN: &str = "^(0|[1-9][0-9]*)(\\.[0-9]{1,6})?$";

const REQUIRED_DEFINITIONS: &[&str] = &[
    "agent",
    "authorityRequirements",
    "catalogPublicationRequest",
    "catalogPublicationWitness",
    "dependencyLock",
    "evidenceVerificationMaterial",
    "executionRequirements",
    "identityClaims",
    "legacyTaskSubmission",
    "localSkill",
    "prompt",
    "revocationRecord",
    "signedTaskEvidence",
    "taskCreateResponse",
    "taskDefinition",
    "taskEvidencePayload",
    "taskFinalizationRequest",
    "taskInputReceipt",
    "taskStatus",
    "taskSubmission",
    "toolBindingStatus",
    "toolCapability",
];

#[derive(Debug, Eq, PartialEq)]
pub struct M1ContractSummary {
    pub definitions: usize,
    pub positive_fixtures: usize,
    pub negative_fixtures: usize,
    pub compatibility_fixtures: usize,
}

pub fn validate_m1_contract_directory(directory: &Path) -> Result<M1ContractSummary, String> {
    let manifest_path = directory.join("manifest.json");
    let manifest = read_json(&manifest_path, "M1 contract manifest")?;
    let manifest = object(&manifest, "M1 contract manifest")?;
    exact_fields(
        manifest,
        &[
            "schemaVersion",
            "contractVersion",
            "schema",
            "definitions",
            "fixtures",
        ],
        "M1 contract manifest",
    )?;
    exact_string(
        manifest,
        "schemaVersion",
        CONTRACT_MANIFEST_SCHEMA,
        "M1 contract manifest",
    )?;
    exact_string(
        manifest,
        "contractVersion",
        CONTRACT_VERSION,
        "M1 contract manifest",
    )?;

    let schema_relative = string(manifest, "schema", "M1 contract manifest")?;
    let schema_path = safe_child(directory, schema_relative, "contract schema")?;
    let schema = read_json(&schema_path, "M1 contract schema")?;
    let schema_object = object(&schema, "M1 contract schema")?;
    exact_string(
        schema_object,
        "$schema",
        JSON_SCHEMA_DIALECT,
        "M1 contract schema",
    )?;
    validate_schema_keywords(&schema, "#")?;

    let mut definitions = BTreeMap::new();
    for definition in array(manifest, "definitions", "M1 contract manifest")? {
        let definition = object(definition, "contract definition")?;
        exact_fields(definition, &["name", "ref"], "contract definition")?;
        let name = string(definition, "name", "contract definition")?;
        let reference = string(definition, "ref", "contract definition")?;
        if definitions.insert(name, reference).is_some() {
            return Err(format!("M1 contract manifest repeats definition {name}"));
        }
        resolve_schema_reference(&schema, reference)?;
    }

    let required = REQUIRED_DEFINITIONS
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let declared = definitions.keys().copied().collect::<BTreeSet<_>>();
    if declared != required {
        let missing = required.difference(&declared).copied().collect::<Vec<_>>();
        let unexpected = declared.difference(&required).copied().collect::<Vec<_>>();
        return Err(format!(
            "M1 contract definitions do not match the frozen set; missing={missing:?}, unexpected={unexpected:?}"
        ));
    }

    let mut positive_by_definition = BTreeSet::new();
    let mut negative_by_definition = BTreeSet::new();
    let mut positive_fixtures = 0;
    let mut negative_fixtures = 0;
    let mut compatibility_fixtures = 0;
    let mut fixture_paths = BTreeSet::new();
    let mut negative_categories = BTreeSet::new();

    for fixture in array(manifest, "fixtures", "M1 contract manifest")? {
        let fixture = object(fixture, "contract fixture declaration")?;
        exact_fields(
            fixture,
            &["path", "definition", "valid", "category"],
            "contract fixture declaration",
        )?;
        let relative = string(fixture, "path", "contract fixture declaration")?;
        if !fixture_paths.insert(relative.to_owned()) {
            return Err(format!(
                "M1 contract manifest repeats fixture path {relative}"
            ));
        }
        let definition = string(fixture, "definition", "contract fixture declaration")?;
        let reference = definitions.get(definition).ok_or_else(|| {
            format!("fixture {relative} names unknown contract definition {definition}")
        })?;
        let expected_valid = boolean(fixture, "valid", "contract fixture declaration")?;
        let category = string(fixture, "category", "contract fixture declaration")?;
        let allowed_categories = [
            "positive",
            "malformed",
            "unknown_field",
            "privilege_injection",
            "compatibility",
        ];
        if !allowed_categories.contains(&category) {
            return Err(format!(
                "fixture {relative} has unsupported category {category}"
            ));
        }
        if category == "positive" && !expected_valid {
            return Err(format!("positive fixture {relative} must expect validity"));
        }
        if matches!(
            category,
            "malformed" | "unknown_field" | "privilege_injection"
        ) && expected_valid
        {
            return Err(format!("negative fixture {relative} must expect rejection"));
        }

        let fixture_path = safe_child(directory, relative, "contract fixture")?;
        let instance = read_json(&fixture_path, "M1 contract fixture")?;
        let validation = validate_json_schema_instance(&schema, reference, &instance);
        match (expected_valid, validation) {
            (true, Ok(())) => {
                positive_by_definition.insert(definition);
                positive_fixtures += 1;
            }
            (false, Err(_)) => {
                negative_by_definition.insert(definition);
                negative_categories.insert(category);
                negative_fixtures += 1;
            }
            (true, Err(error)) => {
                return Err(format!("valid fixture {relative} was rejected: {error}"));
            }
            (false, Ok(())) => {
                return Err(format!("invalid fixture {relative} was accepted"));
            }
        }
        if category == "compatibility" {
            compatibility_fixtures += 1;
        }
    }

    let fixture_directory = directory.join("fixtures");
    let mut fixture_files = BTreeSet::new();
    collect_json_files(directory, &fixture_directory, &mut fixture_files)?;
    if fixture_files != fixture_paths {
        let unlisted = fixture_files
            .difference(&fixture_paths)
            .cloned()
            .collect::<Vec<_>>();
        let missing = fixture_paths
            .difference(&fixture_files)
            .cloned()
            .collect::<Vec<_>>();
        return Err(format!(
            "M1 fixture manifest must list every JSON fixture exactly once; unlisted={unlisted:?}, missing={missing:?}"
        ));
    }

    for definition in REQUIRED_DEFINITIONS {
        if !positive_by_definition.contains(definition) {
            return Err(format!(
                "contract definition {definition} has no valid fixture"
            ));
        }
        if !negative_by_definition.contains(definition) {
            return Err(format!(
                "contract definition {definition} has no invalid fixture"
            ));
        }
    }
    for category in ["malformed", "unknown_field", "privilege_injection"] {
        if !negative_categories.contains(category) {
            return Err(format!("M1 contracts have no {category} rejection fixture"));
        }
    }
    if compatibility_fixtures < 2 {
        return Err("M1 contracts require valid and rejected compatibility fixtures".to_owned());
    }

    Ok(M1ContractSummary {
        definitions: definitions.len(),
        positive_fixtures,
        negative_fixtures,
        compatibility_fixtures,
    })
}

fn validate_json_schema_instance(
    root_schema: &Value,
    schema_reference: &str,
    instance: &Value,
) -> Result<(), String> {
    let schema = resolve_schema_reference(root_schema, schema_reference)?;
    validate_instance(root_schema, schema, instance, "$", 0)
}

fn validate_instance(
    root_schema: &Value,
    schema: &Value,
    instance: &Value,
    path: &str,
    depth: usize,
) -> Result<(), String> {
    if depth > 128 {
        return Err(format!("{path}: schema reference nesting exceeds 128"));
    }
    if let Some(allowed) = schema.as_bool() {
        return if allowed {
            Ok(())
        } else {
            Err(format!("{path}: rejected by false schema"))
        };
    }
    let schema = schema
        .as_object()
        .ok_or_else(|| format!("{path}: schema must be an object or boolean"))?;

    if let Some(reference) = schema.get("$ref") {
        let reference = reference
            .as_str()
            .ok_or_else(|| format!("{path}: $ref must be a string"))?;
        let resolved = resolve_schema_reference(root_schema, reference)?;
        validate_instance(root_schema, resolved, instance, path, depth + 1)?;
    }

    if let Some(expected) = schema.get("const")
        && instance != expected
    {
        return Err(format!(
            "{path}: value does not equal the required constant"
        ));
    }
    if let Some(values) = schema.get("enum") {
        let values = values
            .as_array()
            .ok_or_else(|| format!("{path}: enum must be an array"))?;
        if !values.contains(instance) {
            return Err(format!("{path}: value is not in the allowed enum"));
        }
    }
    if let Some(expected_type) = schema.get("type")
        && !matches_type(instance, expected_type)?
    {
        return Err(format!(
            "{path}: expected JSON type {}, found {}",
            type_description(expected_type),
            instance_type(instance)
        ));
    }

    validate_composition(root_schema, schema, instance, path, depth)?;

    if let Some(value) = instance.as_object() {
        validate_object(root_schema, schema, value, path, depth)?;
    }
    if let Some(value) = instance.as_array() {
        validate_array(root_schema, schema, value, path, depth)?;
    }
    if let Some(value) = instance.as_str() {
        validate_string(schema, value, path)?;
    }
    if let Some(value) = instance.as_f64() {
        validate_number(schema, value, path)?;
    }
    Ok(())
}

fn validate_composition(
    root_schema: &Value,
    schema: &Map<String, Value>,
    instance: &Value,
    path: &str,
    depth: usize,
) -> Result<(), String> {
    if let Some(all_of) = schema.get("allOf") {
        for branch in schema_array(all_of, "allOf", path)? {
            validate_instance(root_schema, branch, instance, path, depth + 1)?;
        }
    }
    if let Some(any_of) = schema.get("anyOf") {
        let branches = schema_array(any_of, "anyOf", path)?;
        if !branches
            .iter()
            .any(|branch| validate_instance(root_schema, branch, instance, path, depth + 1).is_ok())
        {
            return Err(format!("{path}: value satisfies no anyOf branch"));
        }
    }
    if let Some(one_of) = schema.get("oneOf") {
        let branches = schema_array(one_of, "oneOf", path)?;
        let matches = branches
            .iter()
            .filter(|branch| {
                validate_instance(root_schema, branch, instance, path, depth + 1).is_ok()
            })
            .count();
        if matches != 1 {
            return Err(format!(
                "{path}: value must satisfy exactly one oneOf branch, matched {matches}"
            ));
        }
    }
    if let Some(not_schema) = schema.get("not")
        && validate_instance(root_schema, not_schema, instance, path, depth + 1).is_ok()
    {
        return Err(format!("{path}: value satisfies a forbidden schema"));
    }
    Ok(())
}

fn validate_object(
    root_schema: &Value,
    schema: &Map<String, Value>,
    value: &Map<String, Value>,
    path: &str,
    depth: usize,
) -> Result<(), String> {
    if let Some(required) = schema.get("required") {
        for field in required
            .as_array()
            .ok_or_else(|| format!("{path}: required must be an array"))?
        {
            let field = field
                .as_str()
                .ok_or_else(|| format!("{path}: required names must be strings"))?;
            if !value.contains_key(field) {
                return Err(format!("{path}: missing required field {field}"));
            }
        }
    }

    let properties = schema
        .get("properties")
        .map(|properties| {
            properties
                .as_object()
                .ok_or_else(|| format!("{path}: properties must be an object"))
        })
        .transpose()?
        .cloned()
        .unwrap_or_default();
    for (field, field_value) in value {
        let field_path = format!("{path}/{}", escape_pointer(field));
        if let Some(field_schema) = properties.get(field) {
            validate_instance(
                root_schema,
                field_schema,
                field_value,
                &field_path,
                depth + 1,
            )?;
            continue;
        }
        match schema.get("additionalProperties") {
            Some(Value::Bool(false)) => {
                return Err(format!("{field_path}: unknown field {field}"));
            }
            Some(additional) if !additional.is_boolean() => {
                validate_instance(root_schema, additional, field_value, &field_path, depth + 1)?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_array(
    root_schema: &Value,
    schema: &Map<String, Value>,
    value: &[Value],
    path: &str,
    depth: usize,
) -> Result<(), String> {
    if let Some(minimum) = schema.get("minItems").and_then(Value::as_u64)
        && value.len() < minimum as usize
    {
        return Err(format!("{path}: array has fewer than {minimum} items"));
    }
    if let Some(maximum) = schema.get("maxItems").and_then(Value::as_u64)
        && value.len() > maximum as usize
    {
        return Err(format!("{path}: array has more than {maximum} items"));
    }
    if schema.get("uniqueItems") == Some(&Value::Bool(true)) {
        for (index, item) in value.iter().enumerate() {
            if value[..index].contains(item) {
                return Err(format!("{path}/{index}: duplicate array item"));
            }
        }
    }
    if let Some(item_schema) = schema.get("items") {
        for (index, item) in value.iter().enumerate() {
            validate_instance(
                root_schema,
                item_schema,
                item,
                &format!("{path}/{index}"),
                depth + 1,
            )?;
        }
    }
    Ok(())
}

fn validate_string(schema: &Map<String, Value>, value: &str, path: &str) -> Result<(), String> {
    if let Some(minimum) = schema.get("minLength").and_then(Value::as_u64)
        && value.chars().count() < minimum as usize
    {
        return Err(format!(
            "{path}: string is shorter than {minimum} characters"
        ));
    }
    if let Some(maximum) = schema.get("maxLength").and_then(Value::as_u64)
        && value.chars().count() > maximum as usize
    {
        return Err(format!(
            "{path}: string is longer than {maximum} characters"
        ));
    }
    if let Some(pattern) = schema.get("pattern") {
        let pattern = pattern
            .as_str()
            .ok_or_else(|| format!("{path}: pattern must be a string"))?;
        if !matches_supported_pattern(pattern, value)? {
            return Err(format!("{path}: string does not match required pattern"));
        }
    }
    Ok(())
}

fn validate_number(schema: &Map<String, Value>, value: f64, path: &str) -> Result<(), String> {
    if let Some(minimum) = schema.get("minimum").and_then(Value::as_f64)
        && value < minimum
    {
        return Err(format!("{path}: number is below minimum {minimum}"));
    }
    if let Some(maximum) = schema.get("maximum").and_then(Value::as_f64)
        && value > maximum
    {
        return Err(format!("{path}: number is above maximum {maximum}"));
    }
    Ok(())
}

fn matches_supported_pattern(pattern: &str, value: &str) -> Result<bool, String> {
    let result = match pattern {
        QUALIFIED_WORKFLOW_PATTERN => valid_qualified_workflow(value),
        VERSIONED_NAME_PATTERN => valid_versioned_name(value),
        UUID_PATTERN => valid_uuid(value),
        SHA256_PATTERN => prefixed_lower_hex(value, "sha256:", 64),
        GIT_SHA_PATTERN => lower_hex(value, 40),
        SOURCE_ID_PATTERN => {
            !value.starts_with('0')
                && !value.is_empty()
                && value.len() <= 20
                && value.bytes().all(|byte| byte.is_ascii_digit())
        }
        CANONICAL_USER_ID_PATTERN => prefixed_lower_hex(value, "usr_", 32),
        CATALOG_ALIAS_PATTERN => valid_slug(value, 63),
        CAPABILITY_PATTERN => valid_capability(value),
        RELATIVE_PATH_PATTERN => valid_relative_path(value),
        OPAQUE_REFERENCE_PATTERN => valid_opaque_reference(value),
        RFC3339_UTC_PATTERN => valid_rfc3339_utc(value),
        BASE64URL_PATTERN => {
            !value.is_empty()
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        }
        DECIMAL_PATTERN => valid_decimal(value),
        _ => return Err(format!("unsupported JSON Schema pattern {pattern}")),
    };
    Ok(result)
}

fn valid_qualified_workflow(value: &str) -> bool {
    let Some((catalog, artifact)) = value.split_once('/') else {
        return false;
    };
    valid_slug(catalog, 63) && valid_versioned_name(artifact)
}

fn valid_versioned_name(value: &str) -> bool {
    let Some((name, version)) = value.rsplit_once('@') else {
        return false;
    };
    valid_slug(name, 63)
        && matches!(version.bytes().next(), Some(b'1'..=b'9'))
        && version.bytes().all(|byte| byte.is_ascii_digit())
}

fn valid_slug(value: &str, maximum: usize) -> bool {
    (1..=maximum).contains(&value.len())
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn valid_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte),
        })
        && matches!(value.as_bytes().get(14), Some(b'1'..=b'5'))
        && matches!(value.as_bytes().get(19), Some(b'8' | b'9' | b'a' | b'b'))
}

fn lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn prefixed_lower_hex(value: &str, prefix: &str, length: usize) -> bool {
    value
        .strip_prefix(prefix)
        .is_some_and(|suffix| lower_hex(suffix, length))
}

fn valid_capability(value: &str) -> bool {
    let Some((provider, capability)) = value.split_once(':') else {
        return false;
    };
    valid_slug(provider, 32)
        && (1..=96).contains(&capability.len())
        && capability
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase())
        && capability.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
}

fn valid_relative_path(value: &str) -> bool {
    !value.is_empty()
        && !value.contains('\\')
        && value.split('/').all(|segment| {
            !segment.is_empty()
                && !matches!(segment, "." | "..")
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        })
}

fn valid_opaque_reference(value: &str) -> bool {
    let Some((kind, id)) = value.split_once(':') else {
        return false;
    };
    valid_slug(kind, 32)
        && (8..=128).contains(&id.len())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn valid_rfc3339_utc(value: &str) -> bool {
    let Some(value) = value.strip_suffix('Z') else {
        return false;
    };
    let base = value.split_once('.').map_or(value, |(base, fraction)| {
        if fraction.is_empty() || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
            return "";
        }
        base
    });
    if base.len() != 19 {
        return false;
    }
    let bytes = base.as_bytes();
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return false;
    }
    let digits = [0..4, 5..7, 8..10, 11..13, 14..16, 17..19];
    if digits
        .iter()
        .any(|range| !bytes[range.clone()].iter().all(u8::is_ascii_digit))
    {
        return false;
    }
    let number = |range: std::ops::Range<usize>| base[range].parse::<u32>().ok();
    matches!(number(5..7), Some(1..=12))
        && matches!(number(8..10), Some(1..=31))
        && matches!(number(11..13), Some(0..=23))
        && matches!(number(14..16), Some(0..=59))
        && matches!(number(17..19), Some(0..=60))
}

fn valid_decimal(value: &str) -> bool {
    let (integer, fraction) = value
        .split_once('.')
        .map_or((value, None), |(integer, fraction)| {
            (integer, Some(fraction))
        });
    !integer.is_empty()
        && (integer == "0" || !integer.starts_with('0'))
        && integer.bytes().all(|byte| byte.is_ascii_digit())
        && fraction.is_none_or(|fraction| {
            (1..=6).contains(&fraction.len()) && fraction.bytes().all(|byte| byte.is_ascii_digit())
        })
}

fn validate_schema_keywords(schema: &Value, path: &str) -> Result<(), String> {
    let Some(schema) = schema.as_object() else {
        if schema.is_boolean() {
            return Ok(());
        }
        return Err(format!("{path}: schema must be an object or boolean"));
    };
    let allowed = [
        "$schema",
        "$id",
        "$defs",
        "$ref",
        "title",
        "description",
        "type",
        "const",
        "enum",
        "allOf",
        "anyOf",
        "oneOf",
        "not",
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
    ];
    for keyword in schema.keys() {
        if !allowed.contains(&keyword.as_str()) {
            return Err(format!("{path}: unsupported schema keyword {keyword}"));
        }
    }
    if let Some(definitions) = schema.get("$defs") {
        for (name, definition) in definitions
            .as_object()
            .ok_or_else(|| format!("{path}/$defs: must be an object"))?
        {
            validate_schema_keywords(definition, &format!("{path}/$defs/{name}"))?;
        }
    }
    if let Some(properties) = schema.get("properties") {
        for (name, property) in properties
            .as_object()
            .ok_or_else(|| format!("{path}/properties: must be an object"))?
        {
            validate_schema_keywords(property, &format!("{path}/properties/{name}"))?;
        }
    }
    for keyword in ["allOf", "anyOf", "oneOf"] {
        if let Some(branches) = schema.get(keyword) {
            for (index, branch) in schema_array(branches, keyword, path)?.iter().enumerate() {
                validate_schema_keywords(branch, &format!("{path}/{keyword}/{index}"))?;
            }
        }
    }
    for keyword in ["not", "items"] {
        if let Some(child) = schema.get(keyword) {
            validate_schema_keywords(child, &format!("{path}/{keyword}"))?;
        }
    }
    if let Some(additional) = schema.get("additionalProperties")
        && !additional.is_boolean()
    {
        validate_schema_keywords(additional, &format!("{path}/additionalProperties"))?;
    }
    if let Some(pattern) = schema.get("pattern").and_then(Value::as_str) {
        matches_supported_pattern(pattern, "")?;
    }
    Ok(())
}

fn matches_type(instance: &Value, expected: &Value) -> Result<bool, String> {
    if let Some(expected) = expected.as_str() {
        return matches_single_type(instance, expected);
    }
    let expected = expected
        .as_array()
        .ok_or_else(|| "schema type must be a string or array".to_owned())?;
    for candidate in expected {
        let candidate = candidate
            .as_str()
            .ok_or_else(|| "schema type alternatives must be strings".to_owned())?;
        if matches_single_type(instance, candidate)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn matches_single_type(instance: &Value, expected: &str) -> Result<bool, String> {
    Ok(match expected {
        "object" => instance.is_object(),
        "array" => instance.is_array(),
        "string" => instance.is_string(),
        "integer" => instance.as_i64().is_some() || instance.as_u64().is_some(),
        "number" => instance.is_number(),
        "boolean" => instance.is_boolean(),
        "null" => instance.is_null(),
        _ => return Err(format!("unsupported JSON Schema type {expected}")),
    })
}

fn resolve_schema_reference<'a>(root: &'a Value, reference: &str) -> Result<&'a Value, String> {
    if reference == "#" {
        return Ok(root);
    }
    let pointer = reference
        .strip_prefix('#')
        .ok_or_else(|| format!("only local JSON Schema references are supported: {reference}"))?;
    root.pointer(pointer)
        .ok_or_else(|| format!("JSON Schema reference does not resolve: {reference}"))
}

fn schema_array<'a>(value: &'a Value, keyword: &str, path: &str) -> Result<&'a [Value], String> {
    value
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| format!("{path}: {keyword} must be an array"))
}

fn read_json(path: &Path, description: &str) -> Result<Value, String> {
    let content = fs::read_to_string(path)
        .map_err(|error| format!("{description} {} is required: {error}", path.display()))?;
    parse_unique_json(&content)
        .map_err(|error| format!("{description} {} is invalid JSON: {error}", path.display()))
}

fn parse_unique_json(content: &str) -> Result<Value, serde_json::Error> {
    let mut deserializer = serde_json::Deserializer::from_str(content);
    let value = UniqueValue::deserialize(&mut deserializer)?.0;
    deserializer.end()?;
    Ok(value)
}

struct UniqueValue(Value);

impl<'de> Deserialize<'de> for UniqueValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(UniqueValueVisitor)
    }
}

struct UniqueValueVisitor;

impl<'de> Visitor<'de> for UniqueValueVisitor {
    type Value = UniqueValue;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a JSON value without duplicate object keys")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::Bool(value)))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::Number(value.into())))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::Number(value.into())))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .map(UniqueValue)
            .ok_or_else(|| E::custom("JSON number must be finite"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.visit_string(value.to_owned())
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::String(value)))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::Null))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::Null))
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element::<UniqueValue>()? {
            values.push(value.0);
        }
        Ok(UniqueValue(Value::Array(values)))
    }

    fn visit_map<A>(self, mut object: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = Map::new();
        while let Some(key) = object.next_key::<String>()? {
            if values.contains_key(&key) {
                return Err(de::Error::custom(format!("duplicate object key {key}")));
            }
            let value = object.next_value::<UniqueValue>()?;
            values.insert(key, value.0);
        }
        Ok(UniqueValue(Value::Object(values)))
    }
}

fn collect_json_files(
    contract_root: &Path,
    directory: &Path,
    files: &mut BTreeSet<String>,
) -> Result<(), String> {
    let entries = fs::read_dir(directory).map_err(|error| {
        format!(
            "M1 contract fixture directory {} is required: {error}",
            directory.display()
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "failed to read M1 contract fixture directory {}: {error}",
                directory.display()
            )
        })?;
        let file_type = entry.file_type().map_err(|error| {
            format!(
                "failed to inspect M1 contract fixture {}: {error}",
                entry.path().display()
            )
        })?;
        if file_type.is_dir() {
            collect_json_files(contract_root, &entry.path(), files)?;
        } else if file_type.is_file()
            && entry
                .path()
                .extension()
                .and_then(|extension| extension.to_str())
                == Some("json")
        {
            let path = entry.path();
            let relative = path.strip_prefix(contract_root).map_err(|error| {
                format!(
                    "M1 contract fixture {} is outside {}: {error}",
                    path.display(),
                    contract_root.display()
                )
            })?;
            let relative = relative
                .to_str()
                .ok_or_else(|| format!("M1 fixture path {} must be UTF-8", relative.display()))?
                .replace(std::path::MAIN_SEPARATOR, "/");
            files.insert(relative);
        }
    }
    Ok(())
}

fn safe_child(
    directory: &Path,
    relative: &str,
    description: &str,
) -> Result<std::path::PathBuf, String> {
    let relative_path = Path::new(relative);
    if relative.is_empty()
        || relative.contains('\\')
        || !relative_path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err(format!(
            "{description} path must remain beneath the contract root: {relative}"
        ));
    }
    Ok(directory.join(relative_path))
}

fn object<'a>(value: &'a Value, description: &str) -> Result<&'a Map<String, Value>, String> {
    value
        .as_object()
        .ok_or_else(|| format!("{description} must be an object"))
}

fn array<'a>(
    object: &'a Map<String, Value>,
    field: &str,
    description: &str,
) -> Result<&'a [Value], String> {
    object
        .get(field)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| format!("{description}.{field} must be an array"))
}

fn string<'a>(
    object: &'a Map<String, Value>,
    field: &str,
    description: &str,
) -> Result<&'a str, String> {
    object
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{description}.{field} must be a string"))
}

fn boolean(object: &Map<String, Value>, field: &str, description: &str) -> Result<bool, String> {
    object
        .get(field)
        .and_then(Value::as_bool)
        .ok_or_else(|| format!("{description}.{field} must be a boolean"))
}

fn exact_string(
    object: &Map<String, Value>,
    field: &str,
    expected: &str,
    description: &str,
) -> Result<(), String> {
    let actual = string(object, field, description)?;
    if actual == expected {
        Ok(())
    } else {
        Err(format!("{description}.{field} must equal {expected}"))
    }
}

fn exact_fields(
    object: &Map<String, Value>,
    expected: &[&str],
    description: &str,
) -> Result<(), String> {
    let actual = object.keys().map(String::as_str).collect::<BTreeSet<_>>();
    let expected = expected.iter().copied().collect::<BTreeSet<_>>();
    if actual == expected {
        Ok(())
    } else {
        let missing = expected.difference(&actual).copied().collect::<Vec<_>>();
        let unexpected = actual.difference(&expected).copied().collect::<Vec<_>>();
        Err(format!(
            "{description} fields must be exact; missing={missing:?}, unexpected={unexpected:?}"
        ))
    }
}

fn escape_pointer(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

fn type_description(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}

fn instance_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(number) if number.is_i64() || number.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::{
        QUALIFIED_WORKFLOW_PATTERN, parse_unique_json, valid_relative_path, valid_versioned_name,
        validate_json_schema_instance,
    };
    use serde_json::json;

    #[test]
    fn schema_validation_rejects_privilege_injection_as_an_unknown_field() {
        let schema = json!({
            "$defs": {
                "submission": {
                    "type": "object",
                    "required": ["workflow"],
                    "properties": {
                        "workflow": {"type": "string"}
                    },
                    "additionalProperties": false
                }
            }
        });
        let injected = json!({
            "workflow": "applications/repository-review@1",
            "resolvedPrincipal": {"kind": "admin"}
        });

        let result = validate_json_schema_instance(&schema, "#/$defs/submission", &injected);
        assert!(
            result.is_err(),
            "a caller-owned principal must be rejected as an unknown field"
        );
        let error = result.err().unwrap_or_default();

        assert!(
            error.contains("resolvedPrincipal"),
            "the failure must identify the injected field: {error}"
        );
    }

    #[test]
    fn schema_validation_rejects_malformed_qualified_workflow() {
        let schema = json!({
            "$defs": {
                "submission": {
                    "type": "object",
                    "required": ["workflow"],
                    "properties": {
                        "workflow": {
                            "type": "string",
                            "pattern": QUALIFIED_WORKFLOW_PATTERN
                        }
                    },
                    "additionalProperties": false
                }
            }
        });
        let malformed = json!({"workflow": "repository-review@1"});

        let result = validate_json_schema_instance(&schema, "#/$defs/submission", &malformed);
        assert!(
            result.is_err(),
            "an unqualified workflow must not satisfy the M1 submission schema"
        );
        let error = result.err().unwrap_or_default();

        assert!(
            error.contains("workflow"),
            "the failure must identify the malformed field: {error}"
        );
    }

    #[test]
    fn schema_validation_resolves_one_of_and_local_references() {
        let schema = json!({
            "$defs": {
                "slug": {"type": "string", "pattern": "^[a-z][a-z0-9-]{0,62}$"},
                "principal": {
                    "oneOf": [
                        {
                            "type": "object",
                            "required": ["kind", "service"],
                            "properties": {
                                "kind": {"const": "service"},
                                "service": {"$ref": "#/$defs/slug"}
                            },
                            "additionalProperties": false
                        },
                        {
                            "type": "object",
                            "required": ["kind", "user"],
                            "properties": {
                                "kind": {"const": "human_derived"},
                                "user": {"type": "string"}
                            },
                            "additionalProperties": false
                        }
                    ]
                }
            }
        });

        let result = validate_json_schema_instance(
            &schema,
            "#/$defs/principal",
            &json!({"kind": "service", "service": "repository-review"}),
        );
        assert!(
            result.is_ok(),
            "exactly one referenced principal branch must validate: {result:?}"
        );
    }

    #[test]
    fn json_parser_rejects_duplicate_object_keys() {
        let result = parse_unique_json(
            r#"{"workflow":"applications/review@1","workflow":"applications/other@1"}"#,
        );
        assert!(result.is_err(), "duplicate public fields must fail closed");
        let error = result
            .err()
            .map(|error| error.to_string())
            .unwrap_or_default();
        assert!(
            error.contains("duplicate object key workflow"),
            "the failure must identify the duplicate key: {error}"
        );
    }

    #[test]
    fn versioned_name_rejects_an_empty_version() {
        assert!(
            !valid_versioned_name("repository-review@"),
            "the Rust checker must enforce the schema's positive-decimal version"
        );
    }

    #[test]
    fn relative_path_rejects_empty_segments() {
        for path in ["catalog//task.json", "catalog/task.json/"] {
            assert!(
                !valid_relative_path(path),
                "the Rust checker must reject path empty segments: {path}"
            );
        }
    }
}
