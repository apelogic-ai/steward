use std::path::Path;
use std::process::Command;

#[test]
fn holds_mcp_gateway_rejects_cross_subject_credential_reuse() -> Result<(), String> {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| "conformance manifest has no repository parent".to_owned())?;
    let harness = repository
        .join("scripts")
        .join("g2-upstream-conformance")
        .with_extension("sh");
    let status = Command::new("bash")
        .arg(harness)
        .current_dir(repository)
        .status()
        .map_err(|error| format!("failed to start pinned G-2 harness: {error}"))?;
    if !status.success() {
        return Err(format!(
            "G-2 upstream credential-isolation harness failed with {status}"
        ));
    }
    Ok(())
}
