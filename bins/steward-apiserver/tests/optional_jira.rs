use std::process::Command;

#[test]
fn no_jira_configuration_passes_preflight() -> Result<(), String> {
    let output = Command::new(env!("CARGO_BIN_EXE_steward-apiserver-bin"))
        .arg("validate-jira-config")
        .env_remove("STEWARD_JIRA_BASE_URL")
        .env_remove("STEWARD_JIRA_PROJECT_KEY")
        .env_remove("STEWARD_JIRA_ACCOUNT_EMAIL")
        .env_remove("STEWARD_JIRA_TOKEN")
        .output()
        .map_err(|error| format!("run Jira preflight: {error}"))?;
    assert!(
        output.status.success(),
        "absent optional Jira must not prevent startup: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}
