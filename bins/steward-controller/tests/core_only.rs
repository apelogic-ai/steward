use std::process::Command;

#[test]
fn core_mode_needs_no_sandbox_or_inference_configuration() -> Result<(), String> {
    let output = Command::new(env!("CARGO_BIN_EXE_steward-controller-bin"))
        .arg("validate-core-config")
        .env("STEWARD_EXECUTION_ENABLED", "false")
        .env("STEWARD_TASK_ORCHESTRATION_MODE", "staged")
        .env_remove("STEWARD_OPENSHELL_ENDPOINT")
        .env_remove("STEWARD_OPENSHELL_RUNTIME_CLASS_NAME")
        .env_remove("STEWARD_LITELLM_URL")
        .env_remove("STEWARD_LITELLM_MASTER_KEY")
        .output()
        .map_err(|error| format!("run core configuration preflight: {error}"))?;
    assert!(
        output.status.success(),
        "core mode must not read governed-execution configuration: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}
