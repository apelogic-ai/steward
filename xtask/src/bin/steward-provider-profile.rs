use std::env;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use xtask::{
    RenderedProviderProfileBundle, install_rendered_provider_profile_bundle,
    reconcile_rendered_provider_profile_bundle, render_provider_profile_bundle_directory,
    validate_provider_profile_bundle_directory,
};

const RESULT_SCHEMA: &str = "steward.provider-profile-result/v1";

fn main() -> ExitCode {
    match dispatch(&env::args().skip(1).collect::<Vec<_>>()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch(arguments: &[String]) -> Result<(), String> {
    match arguments {
        [operation, bundle_flag, bundle, inputs_flag, inputs]
            if matches!(operation.as_str(), "validate" | "render")
                && bundle_flag == "--bundle"
                && inputs_flag == "--inputs" =>
        {
            let rendered = load_rendered(Path::new(bundle), Path::new(inputs))?;
            let report = result_document(operation, Path::new(bundle), &rendered)?;
            let output = if operation == "render" {
                json!({"result": report, "installation": rendered.state})
            } else {
                report
            };
            print_json(&output)
        }
        [
            operation,
            bundle_flag,
            bundle,
            inputs_flag,
            inputs,
            output_flag,
            output,
        ] if matches!(operation.as_str(), "install" | "reconcile")
            && bundle_flag == "--bundle"
            && inputs_flag == "--inputs"
            && output_flag == "--output" =>
        {
            let bundle = Path::new(bundle);
            let rendered = load_rendered(bundle, Path::new(inputs))?;
            if operation == "install" {
                install_rendered_provider_profile_bundle(Path::new(output), &rendered)?;
            } else {
                reconcile_rendered_provider_profile_bundle(Path::new(output), &rendered)?;
            }
            print_json(&result_document(operation, bundle, &rendered)?)
        }
        _ => Err(usage()),
    }
}

fn usage() -> String {
    [
        "usage:",
        "  steward-provider-profile validate --bundle <directory> --inputs <file>",
        "  steward-provider-profile render --bundle <directory> --inputs <file>",
        "  steward-provider-profile install --bundle <directory> --inputs <file> --output <directory>",
        "  steward-provider-profile reconcile --bundle <directory> --inputs <file> --output <directory>",
    ]
    .join("\n")
}

fn load_rendered(
    bundle_directory: &Path,
    inputs_path: &Path,
) -> Result<RenderedProviderProfileBundle, String> {
    validate_provider_profile_bundle_directory(bundle_directory)?;
    let inputs = fs::read_to_string(inputs_path).map_err(|error| {
        format!(
            "provider profile inputs {} are required: {error}",
            inputs_path.display()
        )
    })?;
    render_provider_profile_bundle_directory(bundle_directory, &inputs)
}

fn result_document(
    operation: &str,
    bundle_directory: &Path,
    rendered: &RenderedProviderProfileBundle,
) -> Result<Value, String> {
    let release = read_release_metadata(bundle_directory)?;
    let closure_bytes = serde_json::to_vec(&rendered.state).map_err(|error| {
        format!("rendered provider profile closure is not serializable: {error}")
    })?;
    let profiles = rendered
        .profiles
        .iter()
        .map(|(id, profile)| {
            let bytes = serde_json::to_vec(profile).map_err(|error| {
                format!("rendered provider profile {id} is not serializable: {error}")
            })?;
            Ok(json!({
                "id": id,
                "digest": format!("sha256:{:x}", Sha256::digest(bytes))
            }))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let bundle_id = rendered
        .state
        .pointer("/bundle/id")
        .and_then(Value::as_str)
        .ok_or_else(|| "rendered provider profile closure requires bundle.id".to_owned())?;
    let bundle_version = rendered
        .state
        .pointer("/bundle/version")
        .and_then(Value::as_str)
        .ok_or_else(|| "rendered provider profile closure requires bundle.version".to_owned())?;

    Ok(json!({
        "schemaVersion": RESULT_SCHEMA,
        "operation": operation,
        "status": "valid",
        "sourceRelease": release.source_release,
        "installer": {"os": release.installer_os, "architecture": release.installer_architecture},
        "bundle": {"id": bundle_id, "version": bundle_version},
        "closureDigest": format!("sha256:{:x}", Sha256::digest(closure_bytes)),
        "profiles": profiles
    }))
}

struct ReleaseMetadata {
    source_release: String,
    installer_os: String,
    installer_architecture: String,
}

fn read_release_metadata(bundle_directory: &Path) -> Result<ReleaseMetadata, String> {
    let path = bundle_directory.join("release.json");
    let value: Value = serde_json::from_str(&fs::read_to_string(&path).map_err(|error| {
        format!(
            "provider profile release metadata {} is required: {error}",
            path.display()
        )
    })?)
    .map_err(|error| {
        format!(
            "provider profile release metadata {} is invalid: {error}",
            path.display()
        )
    })?;
    if value.get("schema").and_then(Value::as_str) != Some("steward.provider-profile-release/v1") {
        return Err("provider profile release metadata has an unsupported schema".to_owned());
    }
    let source_release = value
        .get("sourceRelease")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "provider profile release metadata requires sourceRelease".to_owned())?;
    let installer_os = value
        .pointer("/installer/os")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "provider profile release metadata requires installer.os".to_owned())?;
    let installer_architecture = value
        .pointer("/installer/architecture")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            "provider profile release metadata requires installer.architecture".to_owned()
        })?;
    Ok(ReleaseMetadata {
        source_release: source_release.to_owned(),
        installer_os: installer_os.to_owned(),
        installer_architecture: installer_architecture.to_owned(),
    })
}

fn print_json(value: &Value) -> Result<(), String> {
    println!(
        "{}",
        serde_json::to_string_pretty(value)
            .map_err(|error| format!("provider profile result is not serializable: {error}"))?
    );
    Ok(())
}
