use std::path::Path;
use std::process::Command;

#[test]
fn holds_runtime_bound_credentials_reject_cross_user_reuse() -> Result<(), String> {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| "conformance manifest has no repository parent".to_owned())?;
    let harness = repository
        .join("scripts")
        .join("s1-identity-e2e")
        .with_extension("sh");
    let status = Command::new("bash")
        .arg(harness)
        .current_dir(repository)
        .status()
        .map_err(|error| format!("failed to start pinned G-2 harness: {error}"))?;
    if !status.success() {
        return Err(format!(
            "pinned G-2 credential-isolation harness failed with {status}"
        ));
    }
    Ok(())
}
