use std::process::Command;

pub(crate) fn require_tool_version(
    program: &str,
    arguments: &[&str],
    expected: &str,
    installation_url: &str,
) -> Result<(), String> {
    let output = Command::new(program)
        .args(arguments)
        .output()
        .map_err(|error| {
            format!("{program} {expected} is required; install it from {installation_url}: {error}")
        })?;
    if !output.status.success() {
        return Err(format!(
            "{program} version check failed with {}; install {program} {expected} from {installation_url}",
            output.status
        ));
    }
    let version_output = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if (expected == "*" && !version_output.trim().is_empty())
        || version_output_matches(&version_output, expected)
    {
        Ok(())
    } else {
        Err(format!(
            "{program} {expected} is required, observed `{}`; install the pinned version from {installation_url}",
            version_output.trim()
        ))
    }
}

fn version_output_matches(output: &str, expected: &str) -> bool {
    output
        .split(|character: char| character.is_whitespace())
        .map(|word| word.trim_start_matches('v'))
        .any(|word| {
            word == expected
                || word
                    .strip_prefix(expected)
                    .is_some_and(|suffix| suffix.starts_with('+'))
        })
}

pub(crate) fn checked_command(program: &str) -> Result<Command, String> {
    require_executed_tool_version(program)?;
    Ok(Command::new(program))
}

pub(crate) fn require_executed_tool_version(program: &str) -> Result<(), String> {
    match program {
        "actionlint" => require_tool_version(
            program,
            &["-version"],
            "1.7.7",
            "https://github.com/rhysd/actionlint/releases/tag/v1.7.7",
        ),
        "bash" => require_tool_version(
            program,
            &["--version"],
            "*",
            "https://www.gnu.org/software/bash/",
        ),
        "bun" | "bunx" => require_tool_version(
            program,
            &["--version"],
            "1.2.21",
            "https://github.com/oven-sh/bun/releases/tag/bun-v1.2.21",
        ),
        "cargo" => require_tool_version(program, &["--version"], "1.95.0", "https://rustup.rs/"),
        "git" => require_tool_version(
            program,
            &["--version"],
            "*",
            "https://git-scm.com/downloads",
        ),
        "opa" => require_tool_version(
            program,
            &["version"],
            "1.18.2",
            "https://github.com/open-policy-agent/opa/releases/tag/v1.18.2",
        ),
        "openssl" => require_tool_version(
            program,
            &["version"],
            "*",
            "https://www.openssl.org/source/",
        ),
        "sha256sum" => require_tool_version(
            program,
            &["--version"],
            "*",
            "https://www.gnu.org/software/coreutils/",
        ),
        "shellcheck" => require_tool_version(
            program,
            &["--version"],
            "0.10.0",
            "https://github.com/koalaman/shellcheck/releases/tag/v0.10.0",
        ),
        other => Err(format!(
            "external tool {other} has no require_tool_version contract"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::version_output_matches;

    #[test]
    fn pinned_versions_accept_only_exact_tokens_or_build_metadata() {
        assert!(version_output_matches("v3.17.1+g980d8ac", "3.17.1"));
        assert!(version_output_matches("cargo 1.95.0 (abc)", "1.95.0"));
        assert!(!version_output_matches("v3.17.10", "3.17.1"));
        assert!(!version_output_matches("tool-3.17.1", "3.17.1"));
    }
}
