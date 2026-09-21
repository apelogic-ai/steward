use std::process::Command;

#[test]
fn core_mode_needs_no_agent_inference_endpoint() -> Result<(), String> {
    let output = Command::new(env!("CARGO_BIN_EXE_steward-apiserver-bin"))
        .arg("validate-core-config")
        .env("STEWARD_EXECUTION_ENABLED", "false")
        .env("STEWARD_TASK_ORCHESTRATION_MODE", "staged")
        .env_remove("STEWARD_TASK_INFERENCE_ENDPOINT")
        .env_remove("STEWARD_TASK_ANTHROPIC_INFERENCE_ENDPOINT")
        .output()
        .map_err(|error| format!("run core configuration preflight: {error}"))?;
    assert!(
        output.status.success(),
        "core mode must not need model endpoints: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[test]
fn core_mode_rejects_active_execution_bindings() -> Result<(), String> {
    let output = Command::new(env!("CARGO_BIN_EXE_steward-apiserver-bin"))
        .arg("validate-core-config")
        .env("STEWARD_EXECUTION_ENABLED", "false")
        .env("STEWARD_TASK_ORCHESTRATION_MODE", "staged")
        .env("STEWARD_TASK_EXECUTION_BINDINGS_MODE", "active")
        .output()
        .map_err(|error| format!("run core configuration preflight: {error}"))?;
    assert!(
        !output.status.success(),
        "core-only mode must reject active bindings"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("staged execution bindings"),
        "unexpected error: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}
