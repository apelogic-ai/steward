use std::path::Path;
use std::process::Command;

#[test]
fn holds_inference_gateway_rejects_a_model_outside_the_runtime_allowlist() -> Result<(), String> {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| "conformance manifest has no repository parent".to_owned())?;
    let harness = repository
        .join("scripts")
        .join("g5-upstream-conformance")
        .with_extension("sh");
    let status = Command::new("bash")
        .arg(harness)
        .current_dir(repository)
        .status()
        .map_err(|error| format!("failed to start G-5 harness: {error}"))?;
    if !status.success() {
        return Err(format!(
            "G-5 upstream model-allowlist harness failed with {status}"
        ));
    }
    Ok(())
}
