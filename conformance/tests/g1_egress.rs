use std::path::Path;
use std::process::Command;

#[test]
fn holds_openshell_blocks_an_unlisted_egress_destination() -> Result<(), String> {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| "conformance manifest has no repository parent".to_owned())?;
    let status = Command::new("bash")
        .arg(repository.join("scripts/g1-upstream-conformance"))
        .current_dir(repository)
        .status()
        .map_err(|error| format!("failed to start G-1 harness: {error}"))?;
    if !status.success() {
        return Err(format!(
            "G-1 upstream default-deny egress harness failed with {status}"
        ));
    }
    Ok(())
}
