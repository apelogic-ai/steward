use std::path::Path;
use std::process::Command;

#[test]
fn holds_litellm_rejects_a_deleted_runtime_key() -> Result<(), String> {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| "conformance manifest has no repository parent".to_owned())?;
    let status = Command::new("bash")
        .arg(repository.join("scripts/g4-upstream-conformance"))
        .current_dir(repository)
        .status()
        .map_err(|error| format!("failed to start G-4 harness: {error}"))?;
    if !status.success() {
        return Err(format!(
            "G-4 upstream key-revocation harness failed with {status}"
        ));
    }
    Ok(())
}
