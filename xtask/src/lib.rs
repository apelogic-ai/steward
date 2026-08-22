use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;

use serde_json::Value;

pub const PROVIDER_PROFILE_BUNDLE_SCHEMA: &str = "steward.provider-profile-bundle/v1";
pub const PROVIDER_PROFILE_TEMPLATE_SCHEMA: &str = "steward.provider-profile-template/v1";
pub const PROVIDER_PROFILE_INPUTS_SCHEMA: &str = "steward.provider-profile-inputs/v1";
pub const PROVIDER_PROFILE_INSTALL_STATE_SCHEMA: &str = "steward.provider-profile-install-state/v1";

/// The concrete, environment-bound profiles generated from one portable
/// release bundle.  The renderer deliberately returns values rather than
/// writing them: callers own the installation boundary and can make its
/// filesystem transition atomic.
#[derive(Debug, PartialEq)]
pub struct RenderedProviderProfileBundle {
    pub profiles: BTreeMap<String, Value>,
    pub state: Value,
}

pub fn validate_provider_profile_bundle_directory(directory: &Path) -> Result<(), String> {
    let manifest_path = directory.join("bundle.json");
    let bundle_content = fs::read_to_string(&manifest_path).map_err(|error| {
        format!(
            "provider profile bundle manifest {} is required: {error}",
            manifest_path.display()
        )
    })?;
    let profile_directory = directory.join("profiles");
    let entries = fs::read_dir(&profile_directory).map_err(|error| {
        format!(
            "provider profile bundle templates {} are required: {error}",
            profile_directory.display()
        )
    })?;
    let mut template_paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "failed to read provider profile bundle template directory {}: {error}",
                profile_directory.display()
            )
        })?;
        let file_type = entry.file_type().map_err(|error| {
            format!(
                "failed to inspect provider profile bundle template {}: {error}",
                entry.path().display()
            )
        })?;
        if file_type.is_file()
            && entry
                .path()
                .extension()
                .and_then(|extension| extension.to_str())
                == Some("json")
        {
            let name = entry.file_name();
            let name = name
                .to_str()
                .ok_or_else(|| "provider profile template filename must be UTF-8".to_owned())?;
            let content = fs::read_to_string(entry.path()).map_err(|error| {
                format!(
                    "failed to read provider profile bundle template {}: {error}",
                    entry.path().display()
                )
            })?;
            template_paths.push((format!("profiles/{name}"), content));
        }
    }
    template_paths.sort_by(|left, right| left.0.cmp(&right.0));
    validate_provider_profile_bundle(
        &bundle_content,
        template_paths
            .iter()
            .map(|(path, content)| (path.as_str(), content.as_str())),
    )
}

pub fn render_provider_profile_bundle_directory(
    directory: &Path,
    inputs_content: &str,
) -> Result<RenderedProviderProfileBundle, String> {
    let manifest_path = directory.join("bundle.json");
    let bundle_content = fs::read_to_string(&manifest_path).map_err(|error| {
        format!(
            "provider profile bundle manifest {} is required: {error}",
            manifest_path.display()
        )
    })?;
    let profile_directory = directory.join("profiles");
    let entries = fs::read_dir(&profile_directory).map_err(|error| {
        format!(
            "provider profile bundle templates {} are required: {error}",
            profile_directory.display()
        )
    })?;
    let mut template_paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "failed to read provider profile bundle template directory {}: {error}",
                profile_directory.display()
            )
        })?;
        let file_type = entry.file_type().map_err(|error| {
            format!(
                "failed to inspect provider profile bundle template {}: {error}",
                entry.path().display()
            )
        })?;
        if file_type.is_file()
            && entry
                .path()
                .extension()
                .and_then(|extension| extension.to_str())
                == Some("json")
        {
            let name = entry.file_name();
            let name = name
                .to_str()
                .ok_or_else(|| "provider profile template filename must be UTF-8".to_owned())?;
            let content = fs::read_to_string(entry.path()).map_err(|error| {
                format!(
                    "failed to read provider profile bundle template {}: {error}",
                    entry.path().display()
                )
            })?;
            template_paths.push((format!("profiles/{name}"), content));
        }
    }
    template_paths.sort_by(|left, right| left.0.cmp(&right.0));
    render_provider_profile_bundle(
        &bundle_content,
        template_paths
            .iter()
            .map(|(path, content)| (path.as_str(), content.as_str())),
        inputs_content,
    )
}

/// Validates the portable, release-owned portion of the OpenShell provider
/// profile contract.  Deployment adapters deliberately supply concrete
/// endpoints and network ranges later; those values cannot appear here.
pub fn validate_provider_profile_bundle<'a, I>(
    bundle_content: &str,
    templates: I,
) -> Result<(), String>
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    let bundle = parse_json(bundle_content, "provider profile bundle")?;
    reject_nonportable_values(&bundle, "bundle")?;

    let object = bundle
        .as_object()
        .ok_or_else(|| "provider profile bundle must be a JSON object".to_owned())?;
    require_string(
        object,
        "schema",
        PROVIDER_PROFILE_BUNDLE_SCHEMA,
        "provider profile bundle",
    )?;
    let bundle_metadata = require_object(object, "bundle", "provider profile bundle")?;
    let bundle_id = require_nonempty_string(bundle_metadata, "id", "bundle metadata")?;
    ensure_identifier(bundle_id, "bundle id")?;
    validate_semver(require_nonempty_string(
        bundle_metadata,
        "version",
        "bundle metadata",
    )?)?;

    let profiles = require_array(object, "profiles", "provider profile bundle")?;
    if profiles.is_empty() {
        return Err("provider profile bundle must declare at least one profile".to_owned());
    }
    validate_transition_contract(require_object(
        object,
        "transitions",
        "provider profile bundle",
    )?)?;

    let template_map = templates.into_iter().collect::<BTreeMap<_, _>>();
    let mut declared_templates = BTreeSet::new();
    let mut profile_ids = BTreeSet::new();

    for profile in profiles {
        let profile = profile
            .as_object()
            .ok_or_else(|| "each provider profile declaration must be an object".to_owned())?;
        let id = require_nonempty_string(profile, "id", "provider profile declaration")?;
        ensure_identifier(id, "provider profile id")?;
        if !profile_ids.insert(id) {
            return Err(format!(
                "provider profile bundle declares duplicate profile id {id}"
            ));
        }
        let template_path =
            require_nonempty_string(profile, "template", "provider profile declaration")?;
        let expected_template_path = format!("profiles/{id}.json");
        if template_path != expected_template_path {
            return Err(format!(
                "provider profile {id} must use its canonical template path {expected_template_path}"
            ));
        }
        declared_templates.insert(template_path);
        let inputs = require_array(profile, "inputs", "provider profile declaration")?;
        validate_input_declarations(inputs, id)?;
        let template_content = template_map.get(template_path).ok_or_else(|| {
            format!("provider profile {id} references missing template {template_path}")
        })?;
        validate_provider_profile_template(template_content, id, inputs)?;
    }

    let supplied_templates = template_map.keys().copied().collect::<BTreeSet<_>>();
    if supplied_templates != declared_templates {
        let unexpected = supplied_templates
            .difference(&declared_templates)
            .copied()
            .collect::<Vec<_>>();
        let missing = declared_templates
            .difference(&supplied_templates)
            .copied()
            .collect::<Vec<_>>();
        return Err(format!(
            "provider profile bundle templates must exactly match the manifest; unexpected={unexpected:?}, missing={missing:?}"
        ));
    }

    Ok(())
}

/// Renders the portable provider-profile contract using a separately supplied
/// environment-input document.  The input document is bundle-bound and exact:
/// a deployment may select concrete HTTPS origins and CIDRs, but cannot add a
/// profile, inject a credential, or change policy-bearing template fields.
pub fn render_provider_profile_bundle<'a, I>(
    bundle_content: &str,
    templates: I,
    inputs_content: &str,
) -> Result<RenderedProviderProfileBundle, String>
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    let templates = templates.into_iter().collect::<BTreeMap<_, _>>();
    validate_provider_profile_bundle(
        bundle_content,
        templates.iter().map(|(path, content)| (*path, *content)),
    )?;

    let bundle = parse_json(bundle_content, "provider profile bundle")?;
    let bundle_object = bundle
        .as_object()
        .ok_or_else(|| "provider profile bundle must be a JSON object".to_owned())?;
    let bundle_metadata = require_object(bundle_object, "bundle", "provider profile bundle")?;
    let bundle_id = require_nonempty_string(bundle_metadata, "id", "bundle metadata")?;
    let bundle_version = require_nonempty_string(bundle_metadata, "version", "bundle metadata")?;
    let bundle_profiles = require_array(bundle_object, "profiles", "provider profile bundle")?;

    let inputs = parse_json(inputs_content, "provider profile environment inputs")?;
    let inputs_object = inputs
        .as_object()
        .ok_or_else(|| "provider profile environment inputs must be a JSON object".to_owned())?;
    require_exact_keys(
        inputs_object,
        &["schema", "bundle", "profiles"],
        "provider profile environment inputs",
    )?;
    require_string(
        inputs_object,
        "schema",
        PROVIDER_PROFILE_INPUTS_SCHEMA,
        "provider profile environment inputs",
    )?;
    let input_bundle = require_object(
        inputs_object,
        "bundle",
        "provider profile environment inputs",
    )?;
    require_exact_keys(
        input_bundle,
        &["id", "version"],
        "provider profile environment input bundle",
    )?;
    if require_nonempty_string(
        input_bundle,
        "id",
        "provider profile environment input bundle",
    )? != bundle_id
        || require_nonempty_string(
            input_bundle,
            "version",
            "provider profile environment input bundle",
        )? != bundle_version
    {
        return Err(
            "provider profile environment inputs must bind to the exact release bundle id and version"
                .to_owned(),
        );
    }

    let input_profiles = require_array(
        inputs_object,
        "profiles",
        "provider profile environment inputs",
    )?;
    let input_profiles = collect_input_profiles(input_profiles)?;
    let mut expected_profile_ids = BTreeSet::new();
    let mut rendered_profiles = BTreeMap::new();

    for declaration in bundle_profiles {
        let declaration = declaration
            .as_object()
            .ok_or_else(|| "each provider profile declaration must be an object".to_owned())?;
        let profile_id =
            require_nonempty_string(declaration, "id", "provider profile declaration")?;
        expected_profile_ids.insert(profile_id);
        let declared_inputs = require_array(declaration, "inputs", "provider profile declaration")?;
        let supplied_inputs = input_profiles.get(profile_id).ok_or_else(|| {
            format!("provider profile environment inputs are missing profile {profile_id}")
        })?;
        let bound_inputs = validate_bound_inputs(profile_id, declared_inputs, supplied_inputs)?;
        let template_path =
            require_nonempty_string(declaration, "template", "provider profile declaration")?;
        let template_content = templates.get(template_path).ok_or_else(|| {
            format!("provider profile {profile_id} references missing template {template_path}")
        })?;
        let template = parse_json(template_content, "provider profile template")?;
        rendered_profiles.insert(
            profile_id.to_owned(),
            render_provider_profile(profile_id, &template, &bound_inputs)?,
        );
    }

    let supplied_profile_ids = input_profiles.keys().copied().collect::<BTreeSet<_>>();
    if supplied_profile_ids != expected_profile_ids {
        return Err(format!(
            "provider profile environment input profiles must exactly match the release bundle; unexpected={:?}, missing={:?}",
            supplied_profile_ids
                .difference(&expected_profile_ids)
                .copied()
                .collect::<Vec<_>>(),
            expected_profile_ids
                .difference(&supplied_profile_ids)
                .copied()
                .collect::<Vec<_>>(),
        ));
    }

    let state = serde_json::json!({
        "schema": PROVIDER_PROFILE_INSTALL_STATE_SCHEMA,
        "bundle": {"id": bundle_id, "version": bundle_version},
        "profiles": rendered_profiles,
    });
    let profiles = state
        .get("profiles")
        .and_then(Value::as_object)
        .ok_or_else(|| "rendered provider profile state must contain profiles".to_owned())?
        .iter()
        .map(|(id, profile)| (id.clone(), profile.clone()))
        .collect();

    Ok(RenderedProviderProfileBundle { profiles, state })
}

/// Installs rendered profiles only into an absent destination.  The temporary
/// sibling followed by rename gives consumers either no installation or a
/// complete, self-describing installation; a partial directory is never a
/// valid predecessor for the bundle's `absent` transition.
pub fn install_rendered_provider_profile_bundle(
    output_directory: &Path,
    rendered: &RenderedProviderProfileBundle,
) -> Result<(), String> {
    if output_directory.exists() {
        return Err(format!(
            "provider profile install requires an absent destination; {} already exists",
            output_directory.display()
        ));
    }
    let parent = output_directory.parent().ok_or_else(|| {
        format!(
            "provider profile install destination {} must have a parent directory",
            output_directory.display()
        )
    })?;
    if !parent.is_dir() {
        return Err(format!(
            "provider profile install parent {} must be an existing directory",
            parent.display()
        ));
    }
    let output_name = output_directory
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            format!(
                "provider profile install destination {} must have a UTF-8 basename",
                output_directory.display()
            )
        })?;
    let temporary = parent.join(format!(".{output_name}.install-{}", std::process::id()));
    if temporary.exists() {
        return Err(format!(
            "provider profile install temporary destination {} already exists; classify and remove only its owner-created state",
            temporary.display()
        ));
    }
    fs::create_dir(&temporary).map_err(|error| {
        format!(
            "failed to create provider profile installation directory {}: {error}",
            temporary.display()
        )
    })?;
    let result = write_rendered_provider_profile_bundle(&temporary, rendered).and_then(|()| {
        fs::rename(&temporary, output_directory).map_err(|error| {
            format!(
                "failed to atomically publish provider profile installation to {}: {error}",
                output_directory.display()
            )
        })
    });
    if result.is_err() {
        let _ = fs::remove_dir_all(&temporary);
    }
    result
}

/// Reconciles only an exact installation from the same bundle.  It is
/// deliberately verification-only: profile policy changes require a new
/// signed bundle and an explicit migration contract rather than an in-place
/// deployment rewrite.
pub fn reconcile_rendered_provider_profile_bundle(
    output_directory: &Path,
    rendered: &RenderedProviderProfileBundle,
) -> Result<(), String> {
    let state_path = output_directory.join("install-state.json");
    let state_content = fs::read_to_string(&state_path).map_err(|error| {
        format!(
            "provider profile reconcile requires installation state {}: {error}",
            state_path.display()
        )
    })?;
    let actual_state = parse_json(&state_content, "provider profile installation state")?;
    if actual_state != rendered.state {
        return Err(
            "provider profile reconcile accepts only the exact same rendered bundle state; use an explicit signed migration for any change"
                .to_owned(),
        );
    }
    let expected_root_entries =
        BTreeSet::from(["install-state.json".to_owned(), "profiles".to_owned()]);
    let root_entries = directory_entry_names(output_directory, "provider profile installation")?;
    if root_entries != expected_root_entries {
        return Err(format!(
            "provider profile reconcile installation contents are not exact; unexpected={:?}, missing={:?}",
            root_entries
                .difference(&expected_root_entries)
                .cloned()
                .collect::<Vec<_>>(),
            expected_root_entries
                .difference(&root_entries)
                .cloned()
                .collect::<Vec<_>>(),
        ));
    }
    let profile_directory = output_directory.join("profiles");
    let expected_profiles = rendered
        .profiles
        .keys()
        .map(|id| format!("{id}.json"))
        .collect::<BTreeSet<_>>();
    let actual_profiles =
        directory_entry_names(&profile_directory, "provider profile installation profiles")?;
    if actual_profiles != expected_profiles {
        return Err(format!(
            "provider profile reconcile profile files are not exact; unexpected={:?}, missing={:?}",
            actual_profiles
                .difference(&expected_profiles)
                .cloned()
                .collect::<Vec<_>>(),
            expected_profiles
                .difference(&actual_profiles)
                .cloned()
                .collect::<Vec<_>>(),
        ));
    }
    for (id, expected) in &rendered.profiles {
        let path = profile_directory.join(format!("{id}.json"));
        let actual = parse_json(
            &fs::read_to_string(&path).map_err(|error| {
                format!(
                    "provider profile reconcile requires rendered profile {}: {error}",
                    path.display()
                )
            })?,
            "rendered provider profile",
        )?;
        if &actual != expected {
            return Err(format!(
                "provider profile reconcile found drift in {id}; in-place policy rewrites are forbidden"
            ));
        }
    }
    Ok(())
}

fn write_rendered_provider_profile_bundle(
    directory: &Path,
    rendered: &RenderedProviderProfileBundle,
) -> Result<(), String> {
    let profile_directory = directory.join("profiles");
    fs::create_dir(&profile_directory).map_err(|error| {
        format!(
            "failed to create rendered provider profile directory {}: {error}",
            profile_directory.display()
        )
    })?;
    for (id, profile) in &rendered.profiles {
        write_json_file(&profile_directory.join(format!("{id}.json")), profile)?;
    }
    write_json_file(&directory.join("install-state.json"), &rendered.state)
}

fn write_json_file(path: &Path, value: &Value) -> Result<(), String> {
    let content = serde_json::to_string_pretty(value)
        .map_err(|error| format!("failed to serialize provider profile content: {error}"))?;
    fs::write(path, format!("{content}\n")).map_err(|error| {
        format!(
            "failed to write provider profile file {}: {error}",
            path.display()
        )
    })
}

fn directory_entry_names(directory: &Path, description: &str) -> Result<BTreeSet<String>, String> {
    let entries = fs::read_dir(directory).map_err(|error| {
        format!(
            "{description} directory {} is required: {error}",
            directory.display()
        )
    })?;
    entries
        .map(|entry| {
            let entry = entry.map_err(|error| {
                format!(
                    "failed to read {description} directory {}: {error}",
                    directory.display()
                )
            })?;
            entry.file_name().into_string().map_err(|_| {
                format!(
                    "{description} directory {} has a non-UTF-8 entry",
                    directory.display()
                )
            })
        })
        .collect()
}

fn collect_input_profiles(
    profiles: &[Value],
) -> Result<BTreeMap<&str, &serde_json::Map<String, Value>>, String> {
    let mut result = BTreeMap::new();
    for profile in profiles {
        let profile = profile.as_object().ok_or_else(|| {
            "provider profile environment input profile must be an object".to_owned()
        })?;
        require_exact_keys(
            profile,
            &["id", "inputs"],
            "provider profile environment input profile",
        )?;
        let id =
            require_nonempty_string(profile, "id", "provider profile environment input profile")?;
        ensure_identifier(id, "provider profile environment input id")?;
        let inputs = require_object(
            profile,
            "inputs",
            "provider profile environment input profile",
        )?;
        if result.insert(id, inputs).is_some() {
            return Err(format!(
                "provider profile environment inputs declare duplicate profile {id}"
            ));
        }
    }
    Ok(result)
}

fn validate_bound_inputs(
    profile_id: &str,
    declarations: &[Value],
    supplied: &serde_json::Map<String, Value>,
) -> Result<BTreeMap<String, Value>, String> {
    let mut declared = BTreeMap::new();
    for declaration in declarations {
        let declaration = declaration.as_object().ok_or_else(|| {
            format!("provider profile {profile_id} input declaration must be an object")
        })?;
        let name = require_nonempty_string(declaration, "name", "provider profile input")?;
        let kind = require_nonempty_string(declaration, "kind", "provider profile input")?;
        if declared.insert(name, kind).is_some() {
            return Err(format!(
                "provider profile {profile_id} declares duplicate input {name}"
            ));
        }
    }
    let declared_names = declared.keys().copied().collect::<BTreeSet<_>>();
    let supplied_names = supplied.keys().map(String::as_str).collect::<BTreeSet<_>>();
    if declared_names != supplied_names {
        return Err(format!(
            "provider profile {profile_id} environment inputs must exactly match the release bundle; unexpected={:?}, missing={:?}",
            supplied_names
                .difference(&declared_names)
                .copied()
                .collect::<Vec<_>>(),
            declared_names
                .difference(&supplied_names)
                .copied()
                .collect::<Vec<_>>(),
        ));
    }

    let mut normalized = BTreeMap::new();
    for (name, kind) in declared {
        let value = supplied.get(name).ok_or_else(|| {
            format!("provider profile {profile_id} environment input {name} is required")
        })?;
        let value = match kind {
            "https-origin" => Value::String(parse_https_origin(
                value.as_str().ok_or_else(|| {
                    format!(
                        "provider profile {profile_id} environment input {name} must be an HTTPS origin"
                    )
                })?,
            )?
            .origin),
            "cidr-list" => Value::Array(normalize_cidrs(
                value.as_array().ok_or_else(|| {
                    format!(
                        "provider profile {profile_id} environment input {name} must be a CIDR list"
                    )
                })?,
            )?),
            _ => {
                return Err(format!(
                    "provider profile {profile_id} input {name} has unsupported kind {kind}"
                ));
            }
        };
        normalized.insert(name.to_owned(), value);
    }
    Ok(normalized)
}

fn render_provider_profile(
    profile_id: &str,
    template: &Value,
    inputs: &BTreeMap<String, Value>,
) -> Result<Value, String> {
    let template = template
        .as_object()
        .ok_or_else(|| "provider profile template must be a JSON object".to_owned())?;
    let metadata = require_object(template, "metadata", "provider profile template")?;
    let network = require_object(template, "network", "provider profile template")?;
    let authorization = require_object(template, "authorization", "provider profile template")?;
    let runtime = require_object(template, "runtime", "provider profile template")?;
    let endpoint_input = require_nonempty_string(
        network,
        "endpointInput",
        "provider profile template network",
    )?;
    let endpoint = inputs
        .get(endpoint_input)
        .and_then(Value::as_str)
        .ok_or_else(|| {
            format!("provider profile {profile_id} is missing endpoint input {endpoint_input}")
        })?;
    let endpoint = parse_https_origin(endpoint)?;
    let cidr_input = require_nonempty_string(
        network,
        "allowedCidrsInput",
        "provider profile template network",
    )?;
    let allowed_ips = inputs
        .get(cidr_input)
        .and_then(Value::as_array)
        .ok_or_else(|| {
            format!("provider profile {profile_id} is missing CIDR input {cidr_input}")
        })?;
    let grant_input = require_nonempty_string(
        authorization,
        "tokenGrantOriginInput",
        "provider profile template authorization",
    )?;
    let grant_origin = inputs
        .get(grant_input)
        .and_then(Value::as_str)
        .ok_or_else(|| {
            format!("provider profile {profile_id} is missing grant input {grant_input}")
        })?;
    let grant_origin = parse_https_origin(grant_origin)?;
    let token_path = require_nonempty_string(
        authorization,
        "tokenPath",
        "provider profile template authorization",
    )?;
    let token_endpoint = join_origin_path(&grant_origin.origin, token_path)?;
    let capabilities = require_array(template, "capabilities", "provider profile template")?;
    let category =
        require_nonempty_string(metadata, "category", "provider profile template metadata")?;
    let inference_capable = capabilities
        .iter()
        .any(|capability| capability.as_str() == Some("inference.completions"));
    let access = if capabilities
        .iter()
        .any(|capability| capability.as_str() == Some("tool.read"))
    {
        "read-only"
    } else {
        "read-write"
    };
    let binaries = require_array(
        runtime,
        "requiredBinaries",
        "provider profile template runtime",
    )?;

    let mut profile = serde_json::json!({
        "id": profile_id,
        "display_name": require_nonempty_string(metadata, "displayName", "provider profile template metadata")?,
        "description": require_nonempty_string(metadata, "description", "provider profile template metadata")?,
        "category": category,
        "credentials": [{
            "name": require_nonempty_string(authorization, "authName", "provider profile template authorization")?,
            "description": require_nonempty_string(authorization, "authDescription", "provider profile template authorization")?,
            "required": false,
            "auth_style": "bearer",
            "header_name": "Authorization",
            "token_grant": {
                "token_endpoint": token_endpoint,
                "audience": require_nonempty_string(authorization, "audience", "provider profile template authorization")?,
                "jwt_svid_audience": require_nonempty_string(authorization, "jwtSvidAudience", "provider profile template authorization")?,
                "client_assertion_type": "urn:ietf:params:oauth:client-assertion-type:jwt-spiffe",
                "scopes": require_array(authorization, "scopes", "provider profile template authorization")?,
                "cache_ttl_seconds": authorization.get("cacheTtlSeconds").cloned().ok_or_else(|| "provider profile template authorization must declare cacheTtlSeconds".to_owned())?,
            }
        }],
        "endpoints": [{
            "host": endpoint.host,
            "port": endpoint.port,
            "protocol": "rest",
            "tls": "required",
            "access": access,
            "enforcement": "enforce",
            "allowed_ips": allowed_ips,
        }],
        "binaries": binaries,
    });
    if inference_capable {
        profile["inference_capable"] = Value::Bool(true);
    }
    Ok(profile)
}

struct HttpsOrigin {
    origin: String,
    host: String,
    port: u16,
}

fn parse_https_origin(value: &str) -> Result<HttpsOrigin, String> {
    let value = value.strip_suffix('/').unwrap_or(value);
    let authority = value
        .strip_prefix("https://")
        .filter(|authority| !authority.is_empty())
        .ok_or_else(|| "provider profile HTTPS origin must start with https://".to_owned())?;
    if authority.contains(['/', '?', '#', '@']) {
        return Err(
            "provider profile HTTPS origin must not contain a path, query, fragment, or userinfo"
                .to_owned(),
        );
    }
    let (host, port) = if authority.starts_with('[') {
        let closing = authority.find(']').ok_or_else(|| {
            "provider profile HTTPS origin IPv6 host must have a closing bracket".to_owned()
        })?;
        let host = &authority[1..closing];
        let suffix = &authority[closing + 1..];
        let port = suffix
            .strip_prefix(':')
            .map(parse_port)
            .transpose()?
            .unwrap_or(443);
        if !suffix.is_empty() && !suffix.starts_with(':') {
            return Err("provider profile HTTPS origin has an invalid IPv6 authority".to_owned());
        }
        host.parse::<Ipv6Addr>()
            .map_err(|_| "provider profile HTTPS origin has an invalid IPv6 host".to_owned())?;
        (host.to_owned(), port)
    } else if let Some((host, port)) = authority.rsplit_once(':') {
        if host.contains(':') {
            return Err("provider profile HTTPS origin IPv6 hosts must use brackets".to_owned());
        }
        (validate_origin_host(host)?, parse_port(port)?)
    } else {
        (validate_origin_host(authority)?, 443)
    };
    let rendered_authority = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.clone()
    };
    let origin = if port == 443 {
        format!("https://{rendered_authority}")
    } else {
        format!("https://{rendered_authority}:{port}")
    };
    Ok(HttpsOrigin { origin, host, port })
}

fn validate_origin_host(host: &str) -> Result<String, String> {
    if host.is_empty()
        || host.len() > 253
        || !host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
        || host.starts_with('-')
        || host.ends_with('-')
        || host.starts_with('.')
        || host.ends_with('.')
    {
        return Err("provider profile HTTPS origin has an invalid host".to_owned());
    }
    Ok(host.to_ascii_lowercase())
}

fn parse_port(port: &str) -> Result<u16, String> {
    port.parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or_else(|| "provider profile HTTPS origin has an invalid port".to_owned())
}

fn join_origin_path(origin: &str, path: &str) -> Result<String, String> {
    if !path.starts_with('/')
        || path.starts_with("//")
        || path.contains(['?', '#'])
        || path.split('/').any(|part| part == "." || part == "..")
    {
        return Err("provider profile token path must be an absolute normalized path".to_owned());
    }
    Ok(format!("{origin}{path}"))
}

fn normalize_cidrs(values: &[Value]) -> Result<Vec<Value>, String> {
    if values.is_empty() {
        return Err("provider profile CIDR list must not be empty".to_owned());
    }
    let mut normalized = BTreeSet::new();
    for value in values {
        let value = value
            .as_str()
            .ok_or_else(|| "provider profile CIDR list entries must be strings".to_owned())?;
        let (address, prefix) = value.split_once('/').ok_or_else(|| {
            "provider profile CIDR list entries must use address/prefix".to_owned()
        })?;
        let address = address
            .parse::<IpAddr>()
            .map_err(|_| "provider profile CIDR list contains an invalid address".to_owned())?;
        let prefix = prefix
            .parse::<u8>()
            .map_err(|_| "provider profile CIDR list contains an invalid prefix".to_owned())?;
        let canonical = match address {
            IpAddr::V4(address) => {
                if prefix > 32 {
                    return Err("provider profile CIDR IPv4 prefix must be at most 32".to_owned());
                }
                let network = u32::from(address) & prefix_mask_v4(prefix);
                if network != u32::from(address) {
                    return Err(
                        "provider profile CIDR entries must use canonical network addresses"
                            .to_owned(),
                    );
                }
                format!("{address}/{prefix}")
            }
            IpAddr::V6(address) => {
                if prefix > 128 {
                    return Err("provider profile CIDR IPv6 prefix must be at most 128".to_owned());
                }
                let network = u128::from(address) & prefix_mask_v6(prefix);
                if network != u128::from(address) {
                    return Err(
                        "provider profile CIDR entries must use canonical network addresses"
                            .to_owned(),
                    );
                }
                format!("{address}/{prefix}")
            }
        };
        normalized.insert(canonical);
    }
    Ok(normalized.into_iter().map(Value::String).collect())
}

fn prefix_mask_v4(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    }
}

fn prefix_mask_v6(prefix: u8) -> u128 {
    if prefix == 0 {
        0
    } else {
        u128::MAX << (128 - prefix)
    }
}

fn validate_provider_profile_template(
    content: &str,
    expected_id: &str,
    inputs: &[Value],
) -> Result<(), String> {
    let template = parse_json(content, "provider profile template")?;
    reject_nonportable_values(&template, "provider profile template")?;
    let object = template
        .as_object()
        .ok_or_else(|| "provider profile template must be a JSON object".to_owned())?;
    require_string(
        object,
        "schema",
        PROVIDER_PROFILE_TEMPLATE_SCHEMA,
        "provider profile template",
    )?;
    let actual_id = require_nonempty_string(object, "id", "provider profile template")?;
    if actual_id != expected_id {
        return Err(format!(
            "provider profile template id {actual_id} does not match manifest id {expected_id}"
        ));
    }
    let metadata = require_object(object, "metadata", "provider profile template")?;
    require_exact_keys(
        metadata,
        &["displayName", "description", "category"],
        "provider profile template metadata",
    )?;
    require_nonempty_string(
        metadata,
        "displayName",
        "provider profile template metadata",
    )?;
    require_nonempty_string(
        metadata,
        "description",
        "provider profile template metadata",
    )?;
    let category =
        require_nonempty_string(metadata, "category", "provider profile template metadata")?;
    if !matches!(category, "source_control" | "inference") {
        return Err(
            "provider profile template category must be source_control or inference".to_owned(),
        );
    }
    let capabilities = require_array(object, "capabilities", "provider profile template")?;
    if capabilities.is_empty()
        || capabilities
            .iter()
            .any(|capability| !capability.is_string())
    {
        return Err(
            "provider profile template must declare non-empty string capabilities".to_owned(),
        );
    }
    let input_names = inputs
        .iter()
        .filter_map(|input| input.get("name").and_then(Value::as_str))
        .collect::<BTreeSet<_>>();
    let network = require_object(object, "network", "provider profile template")?;
    let endpoint_input = require_nonempty_string(
        network,
        "endpointInput",
        "provider profile template network",
    )?;
    if !input_names.contains(endpoint_input) {
        return Err(format!(
            "provider profile template endpointInput {endpoint_input} is not a declared manifest input"
        ));
    }
    let allowed_cidrs_input = require_nonempty_string(
        network,
        "allowedCidrsInput",
        "provider profile template network",
    )?;
    if !input_names.contains(allowed_cidrs_input) {
        return Err(format!(
            "provider profile template allowedCidrsInput {allowed_cidrs_input} is not a declared manifest input"
        ));
    }
    require_string(
        network,
        "protocol",
        "https",
        "provider profile template network",
    )?;
    let authorization = require_object(object, "authorization", "provider profile template")?;
    require_exact_keys(
        authorization,
        &[
            "authName",
            "authDescription",
            "tokenGrantOriginInput",
            "tokenPath",
            "audience",
            "jwtSvidAudience",
            "scopes",
            "cacheTtlSeconds",
        ],
        "provider profile template authorization",
    )?;
    require_nonempty_string(
        authorization,
        "authName",
        "provider profile template authorization",
    )?;
    require_nonempty_string(
        authorization,
        "authDescription",
        "provider profile template authorization",
    )?;
    let grant_origin_input = require_nonempty_string(
        authorization,
        "tokenGrantOriginInput",
        "provider profile template authorization",
    )?;
    if !input_names.contains(grant_origin_input) {
        return Err(format!(
            "provider profile template tokenGrantOriginInput {grant_origin_input} is not a declared manifest input"
        ));
    }
    join_origin_path(
        "https://origin.test",
        require_nonempty_string(
            authorization,
            "tokenPath",
            "provider profile template authorization",
        )?,
    )?;
    ensure_identifier(
        require_nonempty_string(
            authorization,
            "audience",
            "provider profile template authorization",
        )?,
        "provider profile template authorization audience",
    )?;
    ensure_identifier(
        require_nonempty_string(
            authorization,
            "jwtSvidAudience",
            "provider profile template authorization",
        )?,
        "provider profile template authorization JWT-SVID audience",
    )?;
    let scopes = require_array(
        authorization,
        "scopes",
        "provider profile template authorization",
    )?;
    if scopes.is_empty() {
        return Err(
            "provider profile template authorization must declare at least one scope".to_owned(),
        );
    }
    for scope in scopes {
        ensure_identifier(
            scope.as_str().ok_or_else(|| {
                "provider profile template authorization scopes must be strings".to_owned()
            })?,
            "provider profile template authorization scope",
        )?;
    }
    let cache_ttl = authorization
        .get("cacheTtlSeconds")
        .and_then(Value::as_u64)
        .filter(|ttl| (1..=120).contains(ttl))
        .ok_or_else(|| {
            "provider profile template authorization cacheTtlSeconds must be an integer from 1 to 120"
                .to_owned()
        })?;
    if cache_ttl == 0 {
        return Err(
            "provider profile template authorization cacheTtlSeconds must be positive".to_owned(),
        );
    }
    let runtime = require_object(object, "runtime", "provider profile template")?;
    let binaries = require_array(
        runtime,
        "requiredBinaries",
        "provider profile template runtime",
    )?;
    if binaries.is_empty() || binaries.iter().any(|binary| !binary.is_string()) {
        return Err(
            "provider profile template runtime must declare non-empty string requiredBinaries"
                .to_owned(),
        );
    }
    Ok(())
}

fn require_exact_keys(
    object: &serde_json::Map<String, Value>,
    expected: &[&str],
    description: &str,
) -> Result<(), String> {
    let actual = object.keys().map(String::as_str).collect::<BTreeSet<_>>();
    let expected = expected.iter().copied().collect::<BTreeSet<_>>();
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "{description} fields must exactly match the release schema; unexpected={:?}, missing={:?}",
            actual.difference(&expected).copied().collect::<Vec<_>>(),
            expected.difference(&actual).copied().collect::<Vec<_>>(),
        ))
    }
}

fn validate_input_declarations(inputs: &[Value], profile_id: &str) -> Result<(), String> {
    if inputs.is_empty() {
        return Err(format!(
            "provider profile {profile_id} must declare its adapter inputs"
        ));
    }
    let mut input_names = BTreeSet::new();
    for input in inputs {
        let input = input
            .as_object()
            .ok_or_else(|| format!("provider profile {profile_id} input must be an object"))?;
        if input.len() != 2 || !input.contains_key("name") || !input.contains_key("kind") {
            return Err(format!(
                "provider profile {profile_id} inputs may contain only name and kind, never deployment values"
            ));
        }
        let name = require_nonempty_string(input, "name", "provider profile input")?;
        ensure_identifier(name, "provider profile input name")?;
        if !input_names.insert(name) {
            return Err(format!(
                "provider profile {profile_id} declares duplicate input {name}"
            ));
        }
        let kind = require_nonempty_string(input, "kind", "provider profile input")?;
        if !matches!(kind, "https-origin" | "cidr-list") {
            return Err(format!(
                "provider profile {profile_id} input {name} uses unsupported portable input kind {kind}"
            ));
        }
    }
    Ok(())
}

fn validate_transition_contract(
    transitions: &serde_json::Map<String, Value>,
) -> Result<(), String> {
    let install = require_array(
        transitions,
        "install",
        "provider profile bundle transitions",
    )?;
    let reconcile = require_array(
        transitions,
        "reconcile",
        "provider profile bundle transitions",
    )?;
    if install.len() != 1
        || install.first().and_then(Value::as_str) != Some("absent")
        || reconcile.len() != 1
        || reconcile.first().and_then(Value::as_str) != Some("same-bundle")
    {
        return Err(
            "provider profile bundle transitions must allow only install from absent and reconcile from same-bundle"
                .to_owned(),
        );
    }
    Ok(())
}

fn reject_nonportable_values(value: &Value, location: &str) -> Result<(), String> {
    match value {
        Value::Object(object) => {
            for (key, nested) in object {
                let normalized = key.to_ascii_lowercase();
                if ["credential", "oauth", "secret", "certificate"]
                    .iter()
                    .any(|term| normalized.contains(term))
                    || matches!(normalized.as_str(), "ca" | "cabundle" | "capath")
                {
                    return Err(format!(
                        "{location} contains forbidden secret, OAuth, credential, or CA field {key}"
                    ));
                }
                reject_nonportable_values(nested, location)?;
            }
        }
        Value::Array(values) => {
            for nested in values {
                reject_nonportable_values(nested, location)?;
            }
        }
        Value::String(text) => {
            let normalized = text.to_ascii_lowercase();
            if normalized.contains(".svc.") || normalized.contains(".cluster.local") {
                return Err(format!("{location} contains cluster DNS value"));
            }
            if normalized.contains("://") {
                return Err(format!("{location} contains a concrete endpoint value"));
            }
            if normalized.contains("-----begin") {
                return Err(format!("{location} contains certificate or key material"));
            }
        }
        _ => {}
    }
    Ok(())
}

fn parse_json(content: &str, description: &str) -> Result<Value, String> {
    serde_json::from_str(content).map_err(|error| format!("{description} is invalid JSON: {error}"))
}

fn require_object<'a>(
    object: &'a serde_json::Map<String, Value>,
    field: &str,
    description: &str,
) -> Result<&'a serde_json::Map<String, Value>, String> {
    object
        .get(field)
        .and_then(Value::as_object)
        .ok_or_else(|| format!("{description} must declare object {field}"))
}

fn require_array<'a>(
    object: &'a serde_json::Map<String, Value>,
    field: &str,
    description: &str,
) -> Result<&'a [Value], String> {
    object
        .get(field)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| format!("{description} must declare array {field}"))
}

fn require_nonempty_string<'a>(
    object: &'a serde_json::Map<String, Value>,
    field: &str,
    description: &str,
) -> Result<&'a str, String> {
    object
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{description} must declare non-empty string {field}"))
}

fn require_string(
    object: &serde_json::Map<String, Value>,
    field: &str,
    expected: &str,
    description: &str,
) -> Result<(), String> {
    let actual = require_nonempty_string(object, field, description)?;
    if actual == expected {
        Ok(())
    } else {
        Err(format!("{description} field {field} must equal {expected}"))
    }
}

fn ensure_identifier(value: &str, description: &str) -> Result<(), String> {
    if value
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !value.starts_with('-')
        && !value.ends_with('-')
    {
        Ok(())
    } else {
        Err(format!(
            "{description} must be a lowercase kebab-case identifier"
        ))
    }
}

fn validate_semver(value: &str) -> Result<(), String> {
    let parts = value.split('.').collect::<Vec<_>>();
    if parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        Ok(())
    } else {
        Err("provider profile bundle version must use MAJOR.MINOR.PATCH".to_owned())
    }
}

pub fn local_test_context_is_safe(context: &str) -> bool {
    ["kind-steward-", "k3d-steward-"]
        .into_iter()
        .find_map(|prefix| context.strip_prefix(prefix))
        .is_some_and(|run_id| {
            !run_id.is_empty()
                && run_id
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
}

pub fn neutrality_violations(content: &str) -> Vec<String> {
    let mut violations = Vec::new();

    for region in text_regions(content) {
        for token in region.split(|character: char| {
            !character.is_ascii_alphanumeric() && !matches!(character, '@' | '.' | ':' | '-' | '_')
        }) {
            if token.is_empty() {
                continue;
            }

            if token.contains('@') {
                if !is_reserved_email(token) && !is_technical_package_scope(token) {
                    violations.push(format!("non-reserved email: {token}"));
                }
                continue;
            }

            if let Ok(address) = token.parse::<IpAddr>() {
                if is_globally_routable(address) {
                    match address {
                        IpAddr::V4(_) => {
                            violations.push(format!("non-reserved IPv4 address: {token}"));
                        }
                        IpAddr::V6(_) => {
                            violations.push(format!("non-reserved IPv6 address: {token}"));
                        }
                    }
                }
                continue;
            }

            if looks_like_hostname(token)
                && !is_allowed_filename(token)
                && !is_reserved_hostname(token)
            {
                violations.push(format!("non-reserved hostname: {token}"));
            }
        }
    }

    violations
}

pub fn secret_violations(path: &Path, content: &[u8]) -> Vec<usize> {
    if is_sensitive_path(path) {
        return vec![1];
    }

    if content.contains(&0) {
        return Vec::new();
    }

    let Ok(text) = std::str::from_utf8(content) else {
        return Vec::new();
    };
    let private_key_marker = ["BEGIN", "PRIVATE", "KEY"].join(" ");
    let rsa_key_marker = ["BEGIN", "RSA", "PRIVATE", "KEY"].join(" ");
    let openssh_key_marker = ["BEGIN", "OPENSSH", "PRIVATE", "KEY"].join(" ");
    let github_prefixes = [
        ["gh", "p_"].concat(),
        ["gh", "o_"].concat(),
        ["gh", "u_"].concat(),
        ["gh", "s_"].concat(),
        ["gh", "r_"].concat(),
    ];
    let provider_prefix = ["s", "k-"].concat();

    text.lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let has_key = line.contains(&private_key_marker)
                || line.contains(&rsa_key_marker)
                || line.contains(&openssh_key_marker);
            let has_github_token = github_prefixes
                .iter()
                .any(|prefix| contains_prefixed_secret(line, prefix, 20));
            let has_provider_key = contains_provider_key(line, &provider_prefix);
            let has_aws_key = contains_aws_access_key(line);
            let has_credential_assignment = contains_password_assignment(line);
            let has_connection_credential = contains_password_in_url(line);

            (has_key
                || has_github_token
                || has_provider_key
                || has_aws_key
                || has_credential_assignment
                || has_connection_credential)
                .then_some(index + 1)
        })
        .collect()
}

pub fn validate_register_content(content: &str) -> Result<(), String> {
    let root = toml::from_str::<toml::Table>(content)
        .map_err(|error| format!("conformance register is not valid TOML: {error}"))?;

    if root.get("schema").and_then(toml::Value::as_integer) != Some(1) {
        return Err("conformance register must declare schema = 1".to_owned());
    }
    let meta = root
        .get("meta")
        .and_then(toml::Value::as_table)
        .ok_or_else(|| "conformance register must declare environment provenance".to_owned())?;
    for (field, dependency) in [
        ("pinned_openshell", "OpenShell"),
        ("pinned_spire", "SPIRE"),
        ("pinned_litellm", "LiteLLM"),
        ("pinned_agent_sandbox", "Agent Sandbox"),
        ("pinned_mcp_gw", "mcp-gw"),
    ] {
        if meta
            .get(field)
            .and_then(toml::Value::as_str)
            .is_none_or(str::is_empty)
        {
            return Err(format!(
                "conformance register must pin {dependency} in one place"
            ));
        }
    }
    if root.contains_key("status")
        || root
            .values()
            .any(|value| contains_toml_key(value, "status"))
    {
        return Err("conformance register must not contain a hand-authored status".to_owned());
    }

    let guarantees = root
        .get("guarantee")
        .and_then(toml::Value::as_array)
        .ok_or_else(|| "conformance register must contain guarantee entries".to_owned())?;
    let ids = guarantees
        .iter()
        .filter_map(|guarantee| guarantee.get("id").and_then(toml::Value::as_str))
        .collect::<Vec<_>>();
    let unique = ids.iter().copied().collect::<BTreeSet<_>>();
    if ids.len() != 6 || unique.len() != ids.len() {
        return Err("conformance register must contain unique entries G-1 through G-6".to_owned());
    }
    for expected in 1..=6 {
        let id = format!("G-{expected}");
        if !unique.contains(id.as_str()) {
            return Err(format!("conformance register is missing {id}"));
        }
    }
    Ok(())
}

pub fn migration_history_violations(changes: &str) -> Vec<String> {
    changes
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let status = fields.next()?;
            if status == "A" {
                return None;
            }
            let paths = fields.collect::<Vec<_>>().join(" -> ");
            Some(format!("{status} {paths}"))
        })
        .collect()
}

pub fn migration_base_candidates(configured: Option<&str>) -> Vec<String> {
    let configured = configured
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .filter(|value| !value.bytes().all(|byte| byte == b'0'));

    configured.map_or_else(
        || vec!["main".to_owned(), "origin/main".to_owned()],
        |value| vec![value.to_owned()],
    )
}

pub fn select_migration_base(
    candidates: &[String],
    resolved: &[(String, String)],
) -> Result<String, String> {
    if resolved.is_empty() {
        return Err(if candidates.len() == 1 {
            format!(
                "migration comparison base {} does not resolve to a commit",
                candidates[0]
            )
        } else {
            "no migration comparison base is available; set STEWARD_MIGRATION_BASE or sync local main"
                .to_owned()
        });
    }

    resolved
        .first()
        .map(|(reference, _commit)| reference.clone())
        .ok_or_else(|| "no migration comparison base is available".to_owned())
}

fn contains_toml_key(value: &toml::Value, key: &str) -> bool {
    match value {
        toml::Value::Table(table) => {
            table.contains_key(key) || table.values().any(|value| contains_toml_key(value, key))
        }
        toml::Value::Array(values) => values.iter().any(|value| contains_toml_key(value, key)),
        _ => false,
    }
}

fn text_regions(content: &str) -> Vec<String> {
    let characters = content.chars().collect::<Vec<_>>();
    let mut regions = Vec::new();
    let mut index = 0;

    while index < characters.len() {
        if characters[index] == '\'' && is_rust_lifetime(&characters, index) {
            index += 1;
            continue;
        }

        if matches!(characters[index], '"' | '\'') {
            let opening = index;
            let delimiter = characters[index];
            index += 1;
            let mut region = String::new();
            let mut escaped = false;
            let mut closed = false;
            while index < characters.len() {
                let character = characters[index];
                index += 1;
                if escaped {
                    region.push(character);
                    escaped = false;
                } else if character == '\\' {
                    escaped = true;
                } else if character == delimiter {
                    closed = true;
                    break;
                } else {
                    region.push(character);
                }
            }
            if closed {
                regions.push(region);
            } else {
                index = opening + 1;
            }
            continue;
        }

        if characters[index] == '/' && characters.get(index + 1) == Some(&'/') {
            index += 2;
            let mut region = String::new();
            while index < characters.len() && characters[index] != '\n' {
                region.push(characters[index]);
                index += 1;
            }
            regions.push(region);
            continue;
        }

        if characters[index] == '/' && characters.get(index + 1) == Some(&'*') {
            index += 2;
            let mut depth = 1;
            let mut region = String::new();
            while index < characters.len() && depth > 0 {
                if characters[index] == '/' && characters.get(index + 1) == Some(&'*') {
                    depth += 1;
                    index += 2;
                } else if characters[index] == '*' && characters.get(index + 1) == Some(&'/') {
                    depth -= 1;
                    index += 2;
                } else {
                    region.push(characters[index]);
                    index += 1;
                }
            }
            regions.push(region);
            continue;
        }

        index += 1;
    }

    regions
}

fn is_rust_lifetime(characters: &[char], quote: usize) -> bool {
    let Some(first) = characters.get(quote + 1) else {
        return false;
    };
    if !first.is_ascii_alphabetic() && *first != '_' {
        return false;
    }

    let mut after = quote + 2;
    while characters
        .get(after)
        .is_some_and(|character| character.is_ascii_alphanumeric() || *character == '_')
    {
        after += 1;
    }
    if characters.get(after) == Some(&'\'') {
        return false;
    }

    matches!(
        characters.get(after),
        Some('>' | ',' | ':' | '+' | ')' | ']')
    ) || characters
        .get(after)
        .is_some_and(|character| character.is_whitespace())
        && characters[..quote]
            .iter()
            .rev()
            .find(|character| !character.is_whitespace())
            .is_some_and(|character| matches!(character, '<' | '&' | ':' | '+' | ','))
}

fn is_reserved_email(token: &str) -> bool {
    let Some((local, domain)) = token.rsplit_once('@') else {
        return false;
    };
    !local.is_empty() && !local.contains('@') && is_example_domain(domain)
}

/// Package scopes are source-code dependencies, not fixture identities. Keep
/// this list exact so the neutral identity boundary remains fail-closed.
fn is_technical_package_scope(token: &str) -> bool {
    token == "@playwright"
}

fn is_globally_routable(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_globally_routable_ipv4(address),
        IpAddr::V6(address) => is_globally_routable_ipv6(address),
    }
}

fn is_globally_routable_ipv4(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    let is_shared = octets[0] == 100 && octets[1] & 0b1100_0000 == 0b0100_0000;
    let is_protocol_assignment = matches!(octets, [192, 0, 0, last] if last != 9 && last != 10);
    let is_benchmarking = octets[0] == 198 && octets[1] & 0xfe == 18;
    let is_reserved = octets[0] & 0xf0 == 0xf0 && address != Ipv4Addr::BROADCAST;

    !(octets[0] == 0
        || address.is_private()
        || is_shared
        || address.is_loopback()
        || address.is_link_local()
        || is_protocol_assignment
        || address.is_documentation()
        || is_benchmarking
        || is_reserved
        || address == Ipv4Addr::BROADCAST)
}

fn is_globally_routable_ipv6(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    let value = u128::from_be_bytes(address.octets());
    let is_protocol_assignment = segments[0] == 0x2001 && segments[1] < 0x200;
    let is_protocol_assignment_exception = matches!(
        value,
        0x2001_0001_0000_0000_0000_0000_0000_0001 | 0x2001_0001_0000_0000_0000_0000_0000_0002
    ) || segments[0] == 0x2001
        && (segments[1] == 3
            || segments[1] == 4 && segments[2] == 0x112
            || (0x20..=0x3f).contains(&segments[1]));
    let is_documentation = (segments[0] == 0x2001 && segments[1] == 0x0db8)
        || (segments[0] == 0x3fff && segments[1] & 0xf000 == 0);

    !(address.is_unspecified()
        || address.is_loopback()
        || matches!(segments, [0, 0, 0, 0, 0, 0xffff, _, _])
        || matches!(segments, [0x64, 0xff9b, 1, _, _, _, _, _])
        || matches!(segments, [0x100, 0, 0, 0, _, _, _, _])
        || is_protocol_assignment && !is_protocol_assignment_exception
        || matches!(segments, [0x2002, _, _, _, _, _, _, _])
        || is_documentation
        || matches!(segments, [0x5f00, ..])
        || address.is_unique_local()
        || address.is_unicast_link_local())
}

fn looks_like_hostname(token: &str) -> bool {
    let labels = token.split('.').collect::<Vec<_>>();
    let Some(top_level) = labels.last() else {
        return false;
    };
    labels.len() >= 2
        && top_level.len() >= 2
        && top_level.bytes().all(|byte| byte.is_ascii_alphabetic())
        && labels.iter().all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

// Dotted tokens are ambiguous even when they appear inside paths because the
// neutrality tokenizer deliberately discards path context. Keep this list exact
// and security-biased; add a narrowly reviewed entry when a fixture must name
// another file so unknown hostname-shaped tokens continue to fail closed.
fn is_allowed_filename(token: &str) -> bool {
    matches!(
        token.to_ascii_lowercase().as_str(),
        "build.rs"
            | "changelog.md"
            | "config.toml"
            | "config.yaml"
            | "config.yml"
            | "contributing.md"
            | "fixture.txt"
            | "jwks.json"
            | "lib.rs"
            | "license.md"
            | "main.rs"
            | "main.txt"
            | "mod.rs"
            | "readme.md"
    )
}

fn is_reserved_hostname(token: &str) -> bool {
    token == "test"
        || token.ends_with(".test")
        || is_example_domain(token)
        || is_recognized_upstream_hostname(token)
}

fn is_example_domain(token: &str) -> bool {
    token == "example.com"
        || token.ends_with(".example.com")
        || token == "example.org"
        || token.ends_with(".example.org")
}

fn is_recognized_upstream_hostname(token: &str) -> bool {
    ["crates.io", "docs.rs", "github.com", "openpolicyagent.org"]
        .into_iter()
        .any(|domain| token == domain || token.ends_with(&format!(".{domain}")))
}

fn is_sensitive_path(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let extension = path.extension().and_then(|extension| extension.to_str());

    name == ".env"
        || name.starts_with(".env.")
        || name == "kubeconfig"
        || name.starts_with("kubeconfig.")
        || matches!(extension, Some("pem" | "key" | "p12" | "pfx"))
        || ((name == "jwks.json" || name.ends_with(".jwks.json"))
            && !name.ends_with(".pub.jwks.json"))
}

fn contains_prefixed_secret(line: &str, prefix: &str, minimum_suffix: usize) -> bool {
    line.match_indices(prefix).any(|(index, _)| {
        line[index + prefix.len()..]
            .chars()
            .take_while(|character| character.is_ascii_alphanumeric() || *character == '_')
            .count()
            >= minimum_suffix
    })
}

fn contains_provider_key(line: &str, prefix: &str) -> bool {
    line.match_indices(prefix).any(|(index, _)| {
        let has_token_boundary = line[..index].chars().next_back().is_none_or(|character| {
            !character.is_ascii_alphanumeric() && !matches!(character, '_' | '-')
        });
        if !has_token_boundary {
            return false;
        }

        let suffix = &line[index + prefix.len()..];
        // Legacy keys have no class tag, so a same-shaped identifier cannot be
        // distinguished safely. Keep this branch conservative; structured keys
        // are handled separately below without broadening the legacy shape.
        let legacy_payload_length = suffix
            .chars()
            .take_while(|character| character.is_ascii_alphanumeric() || *character == '_')
            .count();
        if legacy_payload_length >= 20 {
            return true;
        }

        provider_payload(suffix).is_some_and(|payload| {
            payload
                .chars()
                .take_while(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
                })
                .count()
                >= 20
        })
    })
}

fn provider_payload(suffix: &str) -> Option<&str> {
    if let Some(payload) = suffix
        .strip_prefix("proj-")
        .or_else(|| suffix.strip_prefix("svcacct-"))
    {
        return Some(payload);
    }

    let versioned = suffix.strip_prefix("ant-api")?;
    let (version, payload) = versioned.split_once('-')?;
    (version.len() == 2 && version.bytes().all(|byte| byte.is_ascii_digit())).then_some(payload)
}

fn contains_aws_access_key(line: &str) -> bool {
    let prefix = ["AK", "IA"].concat();
    line.match_indices(&prefix).any(|(index, _)| {
        let suffix = &line[index + prefix.len()..];
        suffix
            .chars()
            .take_while(|character| character.is_ascii_uppercase() || character.is_ascii_digit())
            .count()
            >= 16
    })
}

fn contains_password_assignment(line: &str) -> bool {
    let compact = line
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    let marker = ["pass", "word="].concat();
    let Some(index) = compact.find(&marker) else {
        return false;
    };
    let value = &compact[index + marker.len()..];
    is_literal_secret(value)
}

fn contains_password_in_url(line: &str) -> bool {
    line.split_whitespace().any(|raw| {
        let token = raw.trim_matches(|character: char| {
            matches!(character, '"' | '\'' | '(' | ')' | ',' | ';')
        });
        let Some((_scheme, remainder)) = token.split_once("://") else {
            return false;
        };
        let authority = remainder.split(['/', '?', '#']).next().unwrap_or_default();
        let Some((user_info, _host)) = authority.rsplit_once('@') else {
            return false;
        };
        let Some((_user, password)) = user_info.split_once(':') else {
            return false;
        };
        is_literal_secret(password)
    })
}

fn is_literal_secret(value: &str) -> bool {
    let value = value.trim_start_matches(['"', '\'']);
    !value.is_empty()
        && !value.starts_with('<')
        && !value.starts_with("${")
        && !value.starts_with("env:")
}

#[cfg(test)]
mod tests {
    use super::{
        RenderedProviderProfileBundle, install_rendered_provider_profile_bundle,
        local_test_context_is_safe, migration_base_candidates, migration_history_violations,
        neutrality_violations, reconcile_rendered_provider_profile_bundle,
        render_provider_profile_bundle, secret_violations, select_migration_base,
        validate_provider_profile_bundle, validate_register_content,
    };
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_RENDER_INSTALL_ID: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn neutrality_rejects_non_reserved_identifiers() {
        let email = ["alice", "corp.invalid"].join("@");
        let host = ["service", "corp.invalid"].join(".");
        let ip = ["203", "0", "114", "9"].join(".");
        let content = format!("\"{email}\" \"{host}\" \"{ip}\"");

        let violations = neutrality_violations(&content);

        assert_eq!(
            violations.len(),
            3,
            "neutrality gate must reject every non-reserved identifier"
        );
    }

    #[test]
    fn neutrality_allows_the_browser_test_framework_package_scope() {
        let package = ["@playwright", "test"].join("/");

        assert!(
            neutrality_violations(&format!("\"{package}\"")).is_empty(),
            "a technical test-framework package is not a person, hostname, or customer identifier"
        );
    }

    #[test]
    fn secret_scan_rejects_private_key_material_without_echoing_it() {
        let marker = ["BEGIN", "PRIVATE", "KEY"].join(" ");
        let content = format!("prefix {marker} suffix");

        let violations = secret_violations(Path::new("fixture.txt"), content.as_bytes());

        assert_eq!(
            violations,
            vec![1],
            "secret gate must report only the line number containing key material"
        );
    }

    #[test]
    fn neutrality_ignores_dotted_non_hostnames() {
        let violations = neutrality_violations("\"v1.2.3\" \"schema.v2\"");

        assert!(
            violations.is_empty(),
            "version strings and dotted identifiers are not hostnames: {violations:?}"
        );
    }

    #[test]
    fn neutrality_ignores_dotted_code_selectors() {
        let violations = neutrality_violations("let names = PORTS.iter().map(|port| port.name);");

        assert!(
            violations.is_empty(),
            "code selectors outside strings and comments are not hostnames: {violations:?}"
        );
    }

    #[test]
    fn neutrality_allows_reserved_email_subdomains() {
        let violations = neutrality_violations("\"alice@team-a.example.com\"");

        assert!(
            violations.is_empty(),
            "email domains beneath reserved example domains must be allowed: {violations:?}"
        );
    }

    #[test]
    fn neutrality_rejects_globally_routable_ipv6() {
        let address = ["2001", "db9", "", "1"].join(":");
        let violations = neutrality_violations(&format!("\"{address}\""));

        assert_eq!(
            violations.len(),
            1,
            "neutrality gate must reject a globally routable IPv6 address"
        );
    }

    #[test]
    fn neutrality_allows_documentation_ipv6() {
        let violations = neutrality_violations("\"2001:db8::1\"");

        assert!(
            violations.is_empty(),
            "the RFC 3849 documentation prefix must remain allowed: {violations:?}"
        );
    }

    #[test]
    fn neutrality_allows_non_global_ip_addresses() {
        let violations = neutrality_violations(
            "\"127.0.0.1\" \"0.0.0.0\" \"10.0.0.1\" \"192.168.1.1\" \
             \"169.254.1.1\" \"::1\" \"::\" \"fc00::1\" \"fe80::1\"",
        );

        assert!(
            violations.is_empty(),
            "every non-globally-routable IP address must be allowed: {violations:?}"
        );
    }

    #[test]
    fn neutrality_scans_comments() {
        let email = ["ops", "corp.invalid"].join("@");
        let ip = ["203", "0", "114", "5"].join(".");
        let violations = neutrality_violations(&format!("// reach {email} at {ip}"));

        assert_eq!(
            violations.len(),
            2,
            "neutrality gate must inspect identifiers in comments"
        );
    }

    #[test]
    fn neutrality_scans_single_quoted_text() {
        let host = ["service", "corp.invalid"].join(".");
        let violations = neutrality_violations(&format!("'{host}'"));

        assert_eq!(
            violations.len(),
            1,
            "neutrality gate must inspect identifiers in single-quoted fixture text"
        );
    }

    #[test]
    fn neutrality_scans_after_rust_lifetimes() {
        let host = ["secret", "corp.invalid"].join(".");
        let content = format!("fn borrow<'a>() {{}} let host = \"{host}\";");

        let violations = neutrality_violations(&content);

        assert_eq!(
            violations.len(),
            1,
            "a Rust lifetime must not blind the neutrality gate to later identifiers"
        );
    }

    #[test]
    fn neutrality_ignores_common_filenames() {
        let violations = neutrality_violations("\"src/main.rs\" \"config.yaml\" \"README.md\"");

        assert!(
            violations.is_empty(),
            "routine filename literals are not hostnames: {violations:?}"
        );
    }

    #[test]
    fn neutrality_still_rejects_hostname_shaped_like_a_filename() {
        let hostname = ["service", "rs"].join(".");
        let violations = neutrality_violations(&format!("\"{hostname}\""));

        assert_eq!(
            violations.len(),
            1,
            "an ambiguous bare name under a real top-level domain must remain a hostname"
        );
    }

    #[test]
    fn provider_profile_bundle_rejects_cluster_bound_or_secret_bearing_inputs() {
        let bundle = r#"{
          "schema": "steward.provider-profile-bundle/v1",
          "bundle": {"id": "steward-runtime-providers", "version": "1.0.0"},
          "transitions": {"install": ["absent"], "reconcile": ["same-bundle"]},
          "profiles": [{
            "id": "steward-mcp-gw",
            "template": "profiles/steward-mcp-gw.json",
            "inputs": [{"name": "gateway", "kind": "https-origin"}]
          }]
        }"#;
        let template = r#"{
          "schema": "steward.provider-profile-template/v1",
          "id": "steward-mcp-gw",
          "capabilities": ["tool.read"],
          "network": {"endpointInput": "gateway", "protocol": "https"},
          "forbiddenExample": "mcp-gw.namespace.svc.cluster.local"
        }"#;

        let result =
            validate_provider_profile_bundle(bundle, [("profiles/steward-mcp-gw.json", template)]);

        assert!(
            matches!(&result, Err(error) if error.contains("cluster DNS")),
            "a portable bundle must reject cluster DNS and name the portability boundary: {result:?}"
        );
    }

    #[test]
    fn provider_profile_bundle_accepts_only_named_portable_adapter_inputs() {
        let bundle = r#"{
          "schema": "steward.provider-profile-bundle/v1",
          "bundle": {"id": "steward-runtime-providers", "version": "1.0.0"},
          "transitions": {"install": ["absent"], "reconcile": ["same-bundle"]},
          "profiles": [{
            "id": "steward-mcp-gw",
            "template": "profiles/steward-mcp-gw.json",
            "inputs": [
              {"name": "gateway-origin", "kind": "https-origin"},
              {"name": "runtime-grant-origin", "kind": "https-origin"},
              {"name": "service-cidrs", "kind": "cidr-list"}
            ]
          }]
        }"#;
        let template = r#"{
          "schema": "steward.provider-profile-template/v1",
          "id": "steward-mcp-gw",
          "metadata": {
            "displayName": "Steward MCP gateway",
            "description": "Runtime-scoped MCP gateway access",
            "category": "source_control"
          },
          "capabilities": ["tool.read"],
          "network": {
            "endpointInput": "gateway-origin",
            "allowedCidrsInput": "service-cidrs",
            "protocol": "https"
          },
          "authorization": {
            "authName": "access_token",
            "authDescription": "Runtime-bound MCP access",
            "tokenGrantOriginInput": "runtime-grant-origin",
            "tokenPath": "/token",
            "audience": "steward-mcp",
            "jwtSvidAudience": "steward-mint",
            "scopes": ["mcp"],
            "cacheTtlSeconds": 2
          },
          "runtime": {"requiredBinaries": ["/usr/bin/curl"]}
        }"#;

        let result =
            validate_provider_profile_bundle(bundle, [("profiles/steward-mcp-gw.json", template)]);
        assert!(
            result.is_ok(),
            "named endpoint and CIDR inputs keep the bundle portable: {result:?}"
        );
    }

    #[test]
    fn provider_profile_bundle_rejects_oauth_or_secret_contract_fields_before_release() {
        let bundle = r#"{
          "schema": "steward.provider-profile-bundle/v1",
          "oauthClient": "must-not-be-here"
        }"#;

        let result = validate_provider_profile_bundle(bundle, []);

        assert!(
            matches!(&result, Err(error) if error.contains("OAuth")),
            "the rejection must identify the forbidden contract class without echoing a value: {result:?}"
        );
    }

    #[test]
    fn provider_profile_render_requires_exact_bundle_bound_https_and_network_inputs()
    -> Result<(), String> {
        let bundle = r#"{
          "schema": "steward.provider-profile-bundle/v1",
          "bundle": {"id": "steward-runtime-providers", "version": "1.0.0"},
          "transitions": {"install": ["absent"], "reconcile": ["same-bundle"]},
          "profiles": [{
            "id": "steward-mcp-gw",
            "template": "profiles/steward-mcp-gw.json",
            "inputs": [
              {"name": "gateway-origin", "kind": "https-origin"},
              {"name": "runtime-grant-origin", "kind": "https-origin"},
              {"name": "service-cidrs", "kind": "cidr-list"}
            ]
          }]
        }"#;
        let template = r#"{
          "schema": "steward.provider-profile-template/v1",
          "id": "steward-mcp-gw",
          "metadata": {
            "displayName": "Steward MCP gateway",
            "description": "Runtime-scoped MCP gateway access",
            "category": "source_control"
          },
          "capabilities": ["tool.read"],
          "network": {
            "endpointInput": "gateway-origin",
            "allowedCidrsInput": "service-cidrs",
            "protocol": "https"
          },
          "authorization": {
            "authName": "access_token",
            "authDescription": "Runtime-bound MCP access",
            "tokenGrantOriginInput": "runtime-grant-origin",
            "tokenPath": "/token",
            "audience": "steward-mcp",
            "jwtSvidAudience": "steward-mint",
            "scopes": ["mcp"],
            "cacheTtlSeconds": 2
          },
          "runtime": {"requiredBinaries": ["/usr/bin/curl"]}
        }"#;
        let inputs = r#"{
          "schema": "steward.provider-profile-inputs/v1",
          "bundle": {"id": "steward-runtime-providers", "version": "1.0.0"},
          "profiles": [{
            "id": "steward-mcp-gw",
            "inputs": {
              "gateway-origin": "https://mcp.gateway.test:8443",
              "runtime-grant-origin": "https://mint.gateway.test",
              "service-cidrs": ["10.42.0.0/16", "fd00:42::/64"]
            }
          }]
        }"#;

        let rendered = render_provider_profile_bundle(
            bundle,
            [("profiles/steward-mcp-gw.json", template)],
            inputs,
        )?;

        let profile = rendered
            .profiles
            .get("steward-mcp-gw")
            .ok_or_else(|| "rendered profile must retain its manifest id".to_owned())?;
        assert_eq!(
            profile
                .pointer("/endpoints/0/host")
                .and_then(serde_json::Value::as_str),
            Some("mcp.gateway.test")
        );
        assert_eq!(
            profile
                .pointer("/endpoints/0/port")
                .and_then(serde_json::Value::as_u64),
            Some(8443)
        );
        assert_eq!(
            profile
                .pointer("/endpoints/0/access")
                .and_then(serde_json::Value::as_str),
            Some("read-only"),
            "the rendered tool profile must not silently widen tool.read"
        );
        assert_eq!(
            profile
                .pointer("/credentials/0/token_grant/token_endpoint")
                .and_then(serde_json::Value::as_str),
            Some("https://mint.gateway.test/token")
        );
        assert_eq!(
            rendered
                .state
                .pointer("/schema")
                .and_then(serde_json::Value::as_str),
            Some("steward.provider-profile-install-state/v1")
        );
        Ok(())
    }

    #[test]
    fn provider_profile_render_rejects_extra_or_noncanonical_environment_inputs() {
        let bundle = r#"{
          "schema": "steward.provider-profile-bundle/v1",
          "bundle": {"id": "steward-runtime-providers", "version": "1.0.0"},
          "transitions": {"install": ["absent"], "reconcile": ["same-bundle"]},
          "profiles": [{
            "id": "steward-mcp-gw",
            "template": "profiles/steward-mcp-gw.json",
            "inputs": [{"name": "gateway-origin", "kind": "https-origin"}]
          }]
        }"#;
        let template = r#"{
          "schema": "steward.provider-profile-template/v1",
          "id": "steward-mcp-gw",
          "metadata": {"displayName": "Steward MCP gateway", "description": "Runtime MCP access", "category": "source_control"},
          "capabilities": ["tool.read"],
          "network": {"endpointInput": "gateway-origin", "allowedCidrsInput": "gateway-origin", "protocol": "https"},
          "authorization": {
            "authName": "access_token",
            "authDescription": "Runtime access",
            "tokenGrantOriginInput": "gateway-origin",
            "tokenPath": "/token",
            "audience": "steward-mcp",
            "jwtSvidAudience": "steward-mint",
            "scopes": ["mcp"],
            "cacheTtlSeconds": 2
          },
          "runtime": {"requiredBinaries": ["/usr/bin/curl"]}
        }"#;
        let inputs = r#"{
          "schema": "steward.provider-profile-inputs/v1",
          "bundle": {"id": "steward-runtime-providers", "version": "1.0.0"},
          "profiles": [{
            "id": "steward-mcp-gw",
            "inputs": {
              "gateway-origin": "https://gateway.test/path",
              "unexpected": "value"
            }
          }]
        }"#;

        let result = render_provider_profile_bundle(
            bundle,
            [("profiles/steward-mcp-gw.json", template)],
            inputs,
        );
        assert!(
            matches!(result, Err(ref error) if error.contains("exactly match")),
            "extra inputs must be rejected before any profile is rendered: {result:?}"
        );
    }

    #[test]
    fn provider_profile_install_is_absent_only_and_reconcile_rejects_drift() -> Result<(), String> {
        let profile = serde_json::json!({"id": "steward-mcp-gw", "policy": "read-only"});
        let mut profiles = BTreeMap::new();
        profiles.insert("steward-mcp-gw".to_owned(), profile.clone());
        let rendered = RenderedProviderProfileBundle {
            profiles,
            state: serde_json::json!({
                "schema": "steward.provider-profile-install-state/v1",
                "bundle": {"id": "steward-runtime-providers", "version": "1.0.0"},
                "profiles": {"steward-mcp-gw": profile}
            }),
        };
        let directory = std::env::temp_dir().join(format!(
            "steward-provider-profile-install-test-{}-{}",
            std::process::id(),
            NEXT_RENDER_INSTALL_ID.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir(&directory).map_err(|error| format!("create fixture: {error}"))?;
        let output = directory.join("installed");

        let result = (|| {
            install_rendered_provider_profile_bundle(&output, &rendered)?;
            reconcile_rendered_provider_profile_bundle(&output, &rendered)?;
            let repeat = install_rendered_provider_profile_bundle(&output, &rendered);
            if !matches!(repeat, Err(ref error) if error.contains("absent destination")) {
                return Err(format!(
                    "install must not overwrite an existing state: {repeat:?}"
                ));
            }
            fs::write(
                output.join("profiles/steward-mcp-gw.json"),
                "{\"id\":\"steward-mcp-gw\",\"policy\":\"read-write\"}\n",
            )
            .map_err(|error| format!("write drift fixture: {error}"))?;
            let drift = reconcile_rendered_provider_profile_bundle(&output, &rendered);
            if !matches!(drift, Err(ref error) if error.contains("drift")) {
                return Err(format!("reconcile must reject profile drift: {drift:?}"));
            }
            Ok(())
        })();
        fs::remove_dir_all(&directory).map_err(|error| format!("remove fixture: {error}"))?;
        result
    }

    #[test]
    fn neutrality_rejects_extension_shaped_hostnames() {
        let host = ["svc", "go"].join(".");
        let basename = ["deploy", "sh"].join(".");
        let path = ["scripts", &basename].join("/");
        let violations = neutrality_violations(&format!("\"{host}\" \"{path}\""));

        assert_eq!(
            violations.len(),
            2,
            "filename extensions must not exempt hostname-shaped identifiers"
        );
    }

    #[test]
    fn neutrality_allows_recognized_upstream_hosts() {
        let violations = neutrality_violations(
            "// see github.com/org/repo, crates.io, docs.rs, and openpolicyagent.org",
        );

        assert!(
            violations.is_empty(),
            "recognized public upstream references must remain allowed in comments: {violations:?}"
        );
    }

    #[test]
    fn secret_scan_allows_quoted_environment_reference() {
        let content = br#"password = "${DB_PASSWORD}""#;

        let violations = secret_violations(Path::new("config.toml"), content);

        assert!(
            violations.is_empty(),
            "quoted environment references must not be reported as credentials"
        );
    }

    #[test]
    fn secret_scan_rejects_password_in_connection_url() {
        let scheme = ["post", "gres"].concat();
        let separator = [":", "//"].concat();
        let user_info = ["alice", "not-a-secret"].join(":");
        let content = format!("{scheme}{separator}{user_info}@db.example.com/example");

        let violations = secret_violations(Path::new("config.toml"), content.as_bytes());

        assert_eq!(
            violations,
            vec![1],
            "credential-bearing connection URLs must be rejected"
        );
    }

    #[test]
    fn secret_scan_allows_environment_password_in_connection_url() {
        let scheme = ["post", "gres"].concat();
        let separator = [":", "//"].concat();
        let user_info = ["alice", "${DB_PASSWORD}"].join(":");
        let content = format!("{scheme}{separator}{user_info}@db.example.com/example");

        let violations = secret_violations(Path::new("config.toml"), content.as_bytes());

        assert!(
            violations.is_empty(),
            "environment-backed URL passwords must not be reported as credentials"
        );
    }

    #[test]
    fn secret_scan_rejects_modern_dashed_provider_keys() {
        let prefix = ["s", "k"].concat();
        let payloads = [
            "abcde-fghijklmnopqrstuvwxyz012345",
            "abc_defghijklmnopqrstuvwxyz012345",
            "ab-cd_efghijklmnopqrstuvwxyz012345",
        ];
        let content = ["proj", "ant-api03", "svcacct"]
            .into_iter()
            .zip(payloads)
            .map(|(kind, payload)| [prefix.as_str(), kind, payload].join("-"))
            .collect::<Vec<_>>()
            .join("\n");

        let violations = secret_violations(Path::new("fixture.txt"), content.as_bytes());

        assert_eq!(
            violations,
            vec![1, 2, 3],
            "modern dashed provider-key formats must be rejected"
        );
    }

    #[test]
    fn secret_scan_rejects_future_dashed_provider_classes() {
        let prefix = ["s", "k"].concat();
        let payload = "abc-de_fghijklmnopqrstuvwxyz012345";
        let content = [prefix.as_str(), "ant-api04", payload].join("-");

        let violations = secret_violations(Path::new("fixture.txt"), content.as_bytes());

        assert_eq!(
            violations,
            vec![1],
            "future provider classes with base64url payloads must be rejected"
        );
    }

    #[test]
    fn secret_scan_allows_ordinary_kebab_case_text() {
        let content = [
            "disk-usage-monitoring-service",
            "task-management-workflow-configuration",
            "risk-assessment-framework-module",
        ]
        .join("\n");

        let violations = secret_violations(Path::new("README.md"), content.as_bytes());

        assert!(
            violations.is_empty(),
            "ordinary kebab-case prose must not look like a provider key: {violations:?}"
        );
    }

    #[test]
    fn secret_scan_allows_non_key_tokens_starting_with_sk() {
        let prefix = ["s", "k"].concat();
        let content = [
            [prefix.as_str(), "learn", "model-training-pipeline-v2"].join("-"),
            [prefix.as_str(), "icon", "set-large-collection"].join("-"),
            [prefix.as_str(), "session", "key-rotation-interval-seconds"].join("-"),
        ]
        .join("\n");

        let violations = secret_violations(Path::new("README.md"), content.as_bytes());

        assert!(
            violations.is_empty(),
            "non-key sk-prefixed tokens must not be reported: {violations:?}"
        );
    }

    #[test]
    fn secret_scan_treats_bare_jwks_filename_as_sensitive() {
        let violations = secret_violations(Path::new("jwks.json"), b"{}");

        assert_eq!(
            violations,
            vec![1],
            "a bare private JWKS filename must fail regardless of content"
        );
    }

    #[test]
    fn register_ignores_assignment_shaped_prose() {
        let content = r#"
schema = 1

[meta]
pinned_openshell = "v0.0.82"
pinned_spire = "1.15.2"
pinned_litellm = "1.93.0"
pinned_agent_sandbox = "v0.5.0"
pinned_mcp_gw = "v0.2.0"

[[guarantee]]
id = "G-1"
watch = """
status = holds only after S5
id = "G-3"
"""

[[guarantee]]
id = "G-2"

[[guarantee]]
id = "G-3"

[[guarantee]]
id = "G-4"

[[guarantee]]
id = "G-5"

[[guarantee]]
id = "G-6"
"#;

        let validation = validate_register_content(content);

        assert!(
            validation.is_ok(),
            "assignment-shaped prose inside a TOML string must not affect register structure: {validation:?}"
        );
    }

    #[test]
    fn register_rejects_structural_status() {
        let mut guarantees = (1..=6)
            .map(|id| format!("[[guarantee]]\nid = \"G-{id}\"\n"))
            .collect::<String>();
        guarantees.push_str("status = \"holds\"\n");
        let content = format!(
            "schema = 1\n[meta]\npinned_openshell = \"v0.0.82\"\npinned_spire = \"1.15.2\"\npinned_litellm = \"1.93.0\"\npinned_agent_sandbox = \"v0.5.0\"\npinned_mcp_gw = \"v0.2.0\"\n{guarantees}"
        );

        let validation = validate_register_content(&content);

        assert_eq!(
            validation,
            Err("conformance register must not contain a hand-authored status".to_owned()),
            "a structural status key must remain forbidden"
        );
    }

    #[test]
    fn register_requires_complete_conformance_environment_provenance() {
        let guarantees = (1..=6)
            .map(|id| format!("[[guarantee]]\nid = \"G-{id}\"\n"))
            .collect::<String>();
        let content = format!("schema = 1\n[meta]\npinned_openshell = \"v0.0.90\"\n{guarantees}");

        let validation = validate_register_content(&content);

        assert_eq!(
            validation,
            Err("conformance register must pin SPIRE in one place".to_owned()),
            "a foundation claim without its SPIRE version has ambiguous provenance"
        );
    }

    #[test]
    fn migration_history_rejects_modified_files() {
        let changes = "M\tmigrations/0001_initial.sql\n";

        let violations = migration_history_violations(changes);

        assert_eq!(
            violations,
            vec!["M migrations/0001_initial.sql"],
            "an existing migration modification must fail the append-only check"
        );
    }

    #[test]
    fn migration_history_allows_new_files() {
        let changes = "A\tmigrations/0001_initial.sql\n";

        let violations = migration_history_violations(changes);

        assert!(
            violations.is_empty(),
            "new migration files must remain allowed: {violations:?}"
        );
    }

    #[test]
    fn migration_base_falls_back_to_local_then_remote_main() {
        assert_eq!(
            migration_base_candidates(None),
            vec!["main", "origin/main"],
            "local checks must work without an origin remote while detecting stale refs"
        );
    }

    #[test]
    fn migration_base_ignores_an_all_zero_event_sha() {
        let zero_sha = "0000000000000000000000000000000000000000";

        assert_eq!(
            migration_base_candidates(Some(zero_sha)),
            vec!["main", "origin/main"],
            "an all-zero push event SHA must fall back to real repository refs"
        );
    }

    #[test]
    fn migration_base_honors_an_explicit_commit() {
        let commit = "1234567890abcdef1234567890abcdef12345678";

        assert_eq!(
            migration_base_candidates(Some(commit)),
            vec![commit],
            "CI must compare against its explicit event commit"
        );
    }

    #[test]
    fn migration_base_prefers_local_main_when_remote_main_differs() {
        let candidates = vec!["main".to_owned(), "origin/main".to_owned()];
        let resolved = vec![
            ("main".to_owned(), "1111111".to_owned()),
            ("origin/main".to_owned(), "2222222".to_owned()),
        ];

        let selection = select_migration_base(&candidates, &resolved);

        assert_eq!(
            selection,
            Ok("main".to_owned()),
            "an advancing remote must not invalidate the branch's synced local base"
        );
    }

    #[test]
    fn local_context_requires_a_run_id_after_the_reserved_prefix() {
        assert!(
            !local_test_context_is_safe("kind-steward-"),
            "a local context without a run ID must be rejected"
        );
    }

    #[test]
    fn every_deployed_agentruntime_webhook_intercepts_delete() {
        let stack_scripts = [
            (
                "scripts/s2-inference-inside.sh",
                include_str!("../../scripts/s2-inference-inside.sh"),
            ),
            (
                "scripts/s3-envelope-e2e.sh",
                include_str!("../../scripts/s3-envelope-e2e.sh"),
            ),
        ];

        for (path, script) in stack_scripts {
            assert!(
                script.contains("operations: [\"CREATE\", \"DELETE\", \"UPDATE\"]"),
                "{path} must route AgentRuntime DELETE requests through validating admission"
            );
        }
    }
}
