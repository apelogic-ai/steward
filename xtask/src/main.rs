use std::collections::BTreeSet;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use steward_adapter_fake::IMPLEMENTED_PORTS as FAKE_PORTS;
use steward_adapter_github_source::IMPLEMENTED_PORTS as GITHUB_SOURCE_PORTS;
use steward_ports::{Maturity, PORTS};
use steward_types::agent_runtime_crd;
use xtask::{
    install_rendered_provider_profile_bundle, local_test_context_is_safe,
    migration_base_candidates, migration_history_violations, neutrality_violations,
    reconcile_rendered_provider_profile_bundle, render_provider_profile_bundle_directory,
    secret_violations, select_migration_base, upgrade_rendered_provider_profile_bundle,
    validate_m1_contract_directory, validate_provider_profile_bundle_directory,
    validate_register_content,
};

mod storage;

type TaskResult = Result<(), String>;

const PROVIDER_PROFILE_BUNDLE_CATALOG: [(&str, &str); 3] = [
    ("1.0.0", "config/provider-profile-bundle/v1"),
    ("1.1.0", "config/provider-profile-bundle/v1.1.0"),
    ("1.2.0", "config/provider-profile-bundle/v1.2.0"),
];

fn main() -> ExitCode {
    match dispatch(env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch(arguments: Vec<String>) -> TaskResult {
    let Some(command) = arguments.first().map(String::as_str) else {
        return Err(usage());
    };
    let rest = &arguments[1..];

    match command {
        "ci" if rest.is_empty() => ci(),
        "quality" if rest.is_empty() => quality(),
        "storage" if rest == ["check"] => storage::check(&root()),
        "storage" if rest == ["audit"] => storage::audit(&root()),
        "e2e-openshell-adapter" if rest.is_empty() => e2e_openshell_adapter(),
        "e2e-governed-connections" if rest.is_empty() => e2e_governed_connections(),
        "e2e-postgres-tls" if rest.is_empty() => e2e_postgres_tls(),
        "browser-e2e" if rest.is_empty() => browser_e2e(false),
        "browser-e2e" if rest == ["--browser-ready"] => browser_e2e(true),
        "policy-test" if rest.is_empty() => policy_test(),
        "migrate-check" if rest.is_empty() => migrate_check(),
        "generate-manifests" if rest.is_empty() => generate_manifests(),
        "verify-manifests" if rest.is_empty() => verify_manifests(),
        "check-neutrality" if rest.is_empty() => check_neutrality(),
        "check-secrets" if rest.is_empty() => check_secrets(),
        "m1-contracts" if rest == ["--check"] => m1_contracts_check(),
        "conformance" => conformance(rest),
        "register" => register(rest),
        "ports" if rest == ["--check"] => ports_check(),
        "provider-profile-bundle" => provider_profile_bundle(rest),
        "layering-test" if rest.is_empty() => layering_test(),
        "dev" => dev(rest),
        _ => Err(usage()),
    }
}

fn usage() -> String {
    [
        "usage: cargo xtask <command>",
        "commands:",
        "  ci",
        "  quality",
        "  storage check|audit",
        "  e2e-openshell-adapter",
        "  e2e-governed-connections",
        "  e2e-postgres-tls",
        "  browser-e2e [--browser-ready]",
        "  policy-test",
        "  migrate-check",
        "  generate-manifests",
        "  verify-manifests",
        "  check-neutrality",
        "  check-secrets",
        "  m1-contracts --check",
        "  conformance --pinned|--latest",
        "  register --check",
        "  ports --check",
        "  provider-profile-bundle validate",
        "  provider-profile-bundle render --inputs <file>",
        "  provider-profile-bundle install --inputs <file> --output <directory>",
        "  provider-profile-bundle reconcile --inputs <file> --output <directory>",
        "  provider-profile-bundle upgrade --from-inputs <file> --inputs <file> --output <directory>",
        "  layering-test",
        "  dev doctor",
    ]
    .join("\n")
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

fn ci() -> TaskResult {
    quality()?;
    conformance(&["--pinned".to_owned()])
}

fn quality() -> TaskResult {
    storage::check(&root())?;
    run("cargo", &["fmt", "--all", "--", "--check"])?;
    run(
        "cargo",
        &[
            "fmt",
            "--manifest-path",
            "conformance/Cargo.toml",
            "--",
            "--check",
        ],
    )?;
    run(
        "cargo",
        &["fmt", "--manifest-path", "e2e/Cargo.toml", "--", "--check"],
    )?;
    run(
        "cargo",
        &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
    )?;
    run("cargo", &["test", "--workspace"])?;
    // Vendor SDKs are introduced in later slices, so unused wrapper declarations
    // are expected here. `layering_test` below exercises the wrapper rule itself.
    run("cargo", &["deny", "check", "-A", "unused-wrapper"])?;
    policy_test()?;
    migrate_check()?;
    verify_manifests()?;
    m1_contracts_check()?;
    check_neutrality()?;
    check_secrets()?;
    provider_profile_bundle_validate()?;
    register(&["--check".to_owned()])?;
    ports_check()?;
    layering_test()
}

fn m1_contracts_check() -> TaskResult {
    let directory = root().join("docs/contracts/m1/v1");
    let summary = validate_m1_contract_directory(&directory).map_err(|error| {
        format!(
            "M1 contract validation failed for {}: {error}",
            directory.display()
        )
    })?;
    println!(
        "m1-contracts --check: {} definitions; {} positive, {} negative, {} compatibility fixtures",
        summary.definitions,
        summary.positive_fixtures,
        summary.negative_fixtures,
        summary.compatibility_fixtures
    );
    Ok(())
}

fn provider_profile_bundle_validate() -> TaskResult {
    for (version, relative_directory) in PROVIDER_PROFILE_BUNDLE_CATALOG {
        let directory = root().join(relative_directory);
        validate_provider_profile_bundle_directory(&directory).map_err(|error| {
            format!(
                "provider profile bundle {version} validation failed for {}: {error}",
                directory.display()
            )
        })?;
        println!(
            "provider-profile-bundle: {} validated as immutable product-owned bundle {version}",
            directory.display()
        );
    }
    Ok(())
}

fn provider_profile_bundle(arguments: &[String]) -> TaskResult {
    match arguments {
        [command] if command == "validate" => provider_profile_bundle_validate(),
        [command, inputs_flag, input_path] if command == "render" && inputs_flag == "--inputs" => {
            let rendered = render_provider_profile_bundle_from_input(input_path)?;
            let output = serde_json::to_string_pretty(&rendered.state)
                .map_err(|error| format!("failed to serialize rendered provider profiles: {error}"))?;
            println!("{output}");
            Ok(())
        }
        [command, inputs_flag, input_path, output_flag, output_directory]
            if command == "install" && inputs_flag == "--inputs" && output_flag == "--output" =>
        {
            let rendered = render_provider_profile_bundle_from_input(input_path)?;
            install_rendered_provider_profile_bundle(Path::new(output_directory), &rendered)?;
            println!(
                "provider-profile-bundle: installed {} {} into {}",
                rendered
                    .state
                    .pointer("/bundle/id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown"),
                rendered
                    .state
                    .pointer("/bundle/version")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown"),
                output_directory
            );
            Ok(())
        }
        [command, inputs_flag, input_path, output_flag, output_directory]
            if command == "reconcile" && inputs_flag == "--inputs" && output_flag == "--output" =>
        {
            let rendered = render_provider_profile_bundle_from_input(input_path)?;
            reconcile_rendered_provider_profile_bundle(Path::new(output_directory), &rendered)?;
            println!(
                "provider-profile-bundle: verified same-bundle installation in {output_directory}"
            );
            Ok(())
        }
        [
            command,
            from_inputs_flag,
            from_input_path,
            inputs_flag,
            input_path,
            output_flag,
            output_directory,
        ] if command == "upgrade"
            && from_inputs_flag == "--from-inputs"
            && inputs_flag == "--inputs"
            && output_flag == "--output" =>
        {
            let current = render_provider_profile_bundle_from_input(from_input_path)?;
            let replacement = render_provider_profile_bundle_from_input(input_path)?;
            upgrade_rendered_provider_profile_bundle(
                Path::new(output_directory),
                &current,
                &replacement,
            )?;
            let current_identity = current
                .state
                .pointer("/bundle/version")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            let replacement_identity = replacement
                .state
                .pointer("/bundle/version")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            println!(
                "provider-profile-bundle: upgraded exact steward-runtime-providers@{current_identity} installation to {replacement_identity} in {output_directory}"
            );
            Ok(())
        }
        _ => Err(
            "usage: cargo xtask provider-profile-bundle validate|render --inputs <file>|install --inputs <file> --output <directory>|reconcile --inputs <file> --output <directory>|upgrade --from-inputs <file> --inputs <file> --output <directory>"
                .to_owned(),
        ),
    }
}

fn render_provider_profile_bundle_from_input(
    input_path: &str,
) -> Result<xtask::RenderedProviderProfileBundle, String> {
    let input_content = fs::read_to_string(input_path).map_err(|error| {
        format!("provider profile environment input file {input_path} is required: {error}")
    })?;
    let directory = provider_profile_bundle_directory_for_inputs(&input_content)?;
    render_provider_profile_bundle_directory(&directory, &input_content)
}

fn provider_profile_bundle_directory_for_inputs(input_content: &str) -> Result<PathBuf, String> {
    let inputs: serde_json::Value = serde_json::from_str(input_content)
        .map_err(|error| format!("provider profile environment inputs must be JSON: {error}"))?;
    let bundle_id = inputs
        .pointer("/bundle/id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "provider profile environment inputs require bundle.id".to_owned())?;
    let bundle_version = inputs
        .pointer("/bundle/version")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "provider profile environment inputs require bundle.version".to_owned())?;
    if bundle_id != "steward-runtime-providers" {
        return Err(format!(
            "provider profile environment inputs select unsupported bundle id {bundle_id}"
        ));
    }
    let relative_directory = PROVIDER_PROFILE_BUNDLE_CATALOG
        .iter()
        .find_map(|(version, directory)| (*version == bundle_version).then_some(*directory))
        .ok_or_else(|| {
            format!(
                "provider profile environment inputs select unsupported steward-runtime-providers version {bundle_version}"
            )
        })?;
    Ok(root().join(relative_directory))
}

fn e2e_openshell_adapter() -> TaskResult {
    run("bash", &["scripts/openshell-adapter-e2e.sh"])
}

fn e2e_governed_connections() -> TaskResult {
    run("bash", &["scripts/governed-connections-e2e.sh"])
}

fn e2e_postgres_tls() -> TaskResult {
    run("bash", &["scripts/postgres-tls-e2e.sh"])
}

fn browser_e2e(browser_ready: bool) -> TaskResult {
    let browser_e2e_directory = root().join("target/browser-e2e");
    let cache = browser_e2e_directory.join("bun-cache");
    let browsers = browser_e2e_directory.join("browsers");
    fs::create_dir_all(&cache)
        .map_err(|error| format!("failed to create browser E2E Bun cache: {error}"))?;
    fs::create_dir_all(&browsers)
        .map_err(|error| format!("failed to create browser E2E browser directory: {error}"))?;
    let bun_environment = [
        ("BUN_INSTALL_CACHE_DIR", cache.as_os_str()),
        ("PLAYWRIGHT_BROWSERS_PATH", browsers.as_os_str()),
    ];
    if !browser_ready {
        run_with_env("bun", &["install", "--frozen-lockfile"], &bun_environment)?;
        run_with_env(
            "bunx",
            &["playwright", "install", "chromium"],
            &bun_environment,
        )?;
    }
    run("bun", &["run", "web:check"])?;
    run("bun", &["run", "--cwd", "web", "build"])?;
    run_with_env("bun", &["run", "test:browser-e2e"], &bun_environment)
}

fn policy_test() -> TaskResult {
    run("opa", &["test", "policy"])
}

fn migrate_check() -> TaskResult {
    let directory = root().join("migrations");
    ensure_directory(&directory)?;
    let names = files_with_extension(&directory, "sql")?
        .into_iter()
        .filter_map(|path| path.file_name().and_then(OsStr::to_str).map(str::to_owned))
        .collect::<Vec<_>>();
    let base = resolve_migration_base()?;
    let changes = migration_changes(&root(), &base)?;
    let violations = migration_history_violations(&changes);
    if !violations.is_empty() {
        return Err(format!(
            "existing migrations are immutable; only additions are allowed:\n{}",
            violations.join("\n")
        ));
    }
    println!(
        "migrate-check: {} migration files; append-only history verified against {base}",
        names.len(),
    );
    Ok(())
}

fn git_command_in_repository(repository: &Path) -> Command {
    let mut command = Command::new("git");
    command.current_dir(repository);
    for variable in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_QUARANTINE_PATH",
        "GIT_NAMESPACE",
        "GIT_PREFIX",
    ] {
        command.env_remove(variable);
    }
    command
}

fn migration_changes(repository: &Path, base: &str) -> Result<String, String> {
    let range = format!("{base}...HEAD");
    let output = git_command_in_repository(repository)
        .args([
            "diff",
            "--name-status",
            "--find-renames",
            &range,
            "--",
            ":(glob)migrations/*.sql",
        ])
        .output()
        .map_err(|error| format!("failed to inspect migration history: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git could not compare migration history against {base}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout)
        .map_err(|_| "git returned non-UTF-8 migration paths".to_owned())
}

fn resolve_migration_base() -> Result<String, String> {
    let configured = env::var("STEWARD_MIGRATION_BASE").ok();
    let candidates = migration_base_candidates(configured.as_deref());
    let mut resolved = Vec::new();

    for candidate in &candidates {
        if let Some(commit) = resolve_git_commit(candidate)? {
            resolved.push((candidate.clone(), commit));
        }
    }

    select_migration_base(&candidates, &resolved)
}

fn resolve_git_commit(reference: &str) -> Result<Option<String>, String> {
    let commitish = format!("{reference}^{{commit}}");
    let output = Command::new("git")
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &commitish,
        ])
        .current_dir(root())
        .output()
        .map_err(|error| format!("failed to resolve migration base {reference}: {error}"))?;
    if output.status.success() {
        return String::from_utf8(output.stdout)
            .map(|commit| Some(commit.trim().to_owned()))
            .map_err(|_| format!("git returned a non-UTF-8 commit for {reference}"));
    }
    if output.stderr.is_empty() {
        return Ok(None);
    }

    Err(format!(
        "git could not resolve migration base {reference}: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

fn verify_manifests() -> TaskResult {
    let directory = root().join("manifests");
    ensure_directory(&directory)?;
    let expected_path = directory.join("agents.apelogic.ai_agentruntimes.yaml");
    let actual = fs::read_to_string(&expected_path)
        .map_err(|error| format!("failed to read {}: {error}", expected_path.display()))?;
    let expected = render_agent_runtime_crd()?;
    if actual != expected {
        return Err(format!(
            "{} is stale; run `cargo xtask generate-manifests`",
            expected_path.display()
        ));
    }
    let generated = files_with_extension(&directory, "yaml")?;
    if generated != [expected_path] {
        return Err("manifests contains an unrecognized generated YAML file".to_owned());
    }
    println!("verify-manifests: AgentRuntime CRD matches steward-types");
    Ok(())
}

fn generate_manifests() -> TaskResult {
    let directory = root().join("manifests");
    ensure_directory(&directory)?;
    let path = directory.join("agents.apelogic.ai_agentruntimes.yaml");
    write_file(&path, &render_agent_runtime_crd()?)?;
    println!("generate-manifests: wrote {}", path.display());
    Ok(())
}

fn render_agent_runtime_crd() -> Result<String, String> {
    let yaml = serde_saphyr::to_string(&agent_runtime_crd())
        .map_err(|error| format!("failed to serialize AgentRuntime CRD: {error}"))?;
    Ok(format!(
        "# Generated by `cargo xtask generate-manifests`; do not edit.\n{yaml}"
    ))
}

fn check_neutrality() -> TaskResult {
    let repository = root();
    let files = collect_files(&repository)?;
    let mut failures = Vec::new();

    for path in files.into_iter().filter(|path| is_test_path(path)) {
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let violations = neutrality_violations(&content);
        if !violations.is_empty() {
            failures.push(format!(
                "{}: {} non-reserved identifiers",
                display_relative(&path, &repository),
                violations.len()
            ));
        }
    }

    if failures.is_empty() {
        println!("check-neutrality: all test identifiers use reserved ranges");
        Ok(())
    } else {
        Err(format!(
            "neutrality violations found:\n{}",
            failures.join("\n")
        ))
    }
}

fn check_secrets() -> TaskResult {
    let repository = root();
    let mut failures = Vec::new();

    for path in collect_files(&repository)? {
        let content = fs::read(&path)
            .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
        let lines = secret_violations(&path, &content);
        if !lines.is_empty() {
            failures.push(format!(
                "{}: suspicious material at line(s) {}",
                display_relative(&path, &repository),
                lines
                    .iter()
                    .map(usize::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }

    if failures.is_empty() {
        println!("check-secrets: working tree contains no recognized secret material");
        Ok(())
    } else {
        Err(format!(
            "secret scan rejected the working tree:\n{}",
            failures.join("\n")
        ))
    }
}

fn conformance(arguments: &[String]) -> TaskResult {
    if arguments != ["--pinned"] && arguments != ["--latest"] {
        return Err("conformance requires exactly --pinned or --latest".to_owned());
    }
    validate_register()?;
    let target = arguments[0].trim_start_matches("--");
    run_conformance_module(target, "G-1", "g1_egress")?;
    run_conformance_module(target, "G-2", "g2_credential_isolation")?;
    run_conformance_module(target, "G-4", "g4_revocation")?;
    run_conformance_module(target, "G-5", "g5_model_allowlist")?;
    println!(
        "conformance --{target}: G-1, G-2, G-4, and G-5 each executed exactly one negative test"
    );
    Ok(())
}

fn run_conformance_module(target: &str, guarantee: &str, module: &str) -> TaskResult {
    let output = Command::new("cargo")
        .args([
            "test",
            "--manifest-path",
            "conformance/Cargo.toml",
            "--test",
            module,
            "--",
            "--nocapture",
        ])
        .env("STEWARD_CONFORMANCE_TARGET", target)
        .current_dir(root())
        .output()
        .map_err(|error| format!("failed to run {guarantee} {target} conformance: {error}"))?;
    io::stdout()
        .write_all(&output.stdout)
        .map_err(|error| format!("failed to relay {guarantee} conformance output: {error}"))?;
    io::stderr()
        .write_all(&output.stderr)
        .map_err(|error| format!("failed to relay {guarantee} conformance diagnostics: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "{guarantee} {target} conformance exited with {}",
            output.status
        ));
    }
    validate_conformance_test_result_for(&String::from_utf8_lossy(&output.stdout), guarantee)?;
    Ok(())
}

#[cfg(test)]
fn validate_conformance_test_result(output: &str) -> TaskResult {
    validate_conformance_test_result_for(output, "G-2")
}

fn validate_conformance_test_result_for(output: &str, guarantee: &str) -> TaskResult {
    const EXPECTED_RUST: &str =
        "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out";
    let expected_upstream = format!("{guarantee} upstream result: 1 passed; 0 failed; 0 skipped");
    let rust_summaries = output
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("test result:"))
        .collect::<Vec<_>>();
    let upstream_summaries = output
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with(&format!("{guarantee} upstream result:")))
        .collect::<Vec<_>>();
    if rust_summaries.len() == 1
        && rust_summaries[0].starts_with(EXPECTED_RUST)
        && upstream_summaries == [expected_upstream]
    {
        Ok(())
    } else {
        Err(format!(
            "{guarantee} evidence must execute exactly one upstream test and one Rust wrapper with none skipped, ignored, or filtered; upstream: {}; Rust: {}",
            upstream_summaries.join(" | "),
            rust_summaries.join(" | ")
        ))
    }
}

fn register(arguments: &[String]) -> TaskResult {
    if arguments != ["--check"] {
        return Err("register accepts exactly `--check`".to_owned());
    }
    validate_register()?;
    println!("register --check: declarative register shape is valid");
    Ok(())
}

fn validate_register() -> TaskResult {
    let path = root().join("conformance/register.toml");
    let content = fs::read_to_string(&path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    validate_register_content(&content)?;
    for (guarantee, module) in [
        ("G-2", "g2_credential_isolation"),
        ("G-5", "g5_model_allowlist"),
    ] {
        let path = root()
            .join("conformance")
            .join("tests")
            .join(format!("{module}.rs"));
        if !path.is_file() {
            return Err(format!(
                "{guarantee} claim has no conformance module {}",
                path.display()
            ));
        }
    }
    Ok(())
}

fn ports_check() -> TaskResult {
    let fake = FAKE_PORTS.iter().copied().collect::<BTreeSet<_>>();
    let declared = PORTS
        .iter()
        .map(|descriptor| descriptor.name)
        .collect::<BTreeSet<_>>();
    if fake != declared {
        return Err("fake adapter must implement every declared port".to_owned());
    }

    let real_implementations = real_implemented_ports();
    for descriptor in PORTS {
        let expected = if real_implementations.contains(descriptor.name) {
            Maturity::Proven
        } else {
            Maturity::Provisional
        };
        if descriptor.maturity != expected {
            return Err(format!(
                "{} maturity is {:?}, but implementors derive {:?}",
                descriptor.name, descriptor.maturity, expected
            ));
        }
    }
    println!(
        "ports --check: {} ports declared; fake complete; maturity derived",
        PORTS.len()
    );
    Ok(())
}

fn real_implemented_ports() -> BTreeSet<&'static str> {
    [
        GITHUB_SOURCE_PORTS.as_slice(),
        steward_adapter_jira::IMPLEMENTED_PORTS.as_slice(),
        steward_adapter_litellm::IMPLEMENTED_PORTS.as_slice(),
        steward_adapter_mcp_gw::IMPLEMENTED_PORTS.as_slice(),
        steward_adapter_opa::IMPLEMENTED_PORTS.as_slice(),
        steward_adapter_openshell::IMPLEMENTED_PORTS.as_slice(),
        steward_adapter_spire::IMPLEMENTED_PORTS.as_slice(),
    ]
    .into_iter()
    .flatten()
    .copied()
    .collect()
}

fn layering_test() -> TaskResult {
    let fixture = root()
        .join("target")
        .join("xtask")
        .join(format!("layering-{}", std::process::id()));
    if fixture.exists() {
        return Err(format!(
            "refusing to overwrite existing layering fixture {}",
            fixture.display()
        ));
    }
    let guard = TemporaryTree::create(fixture)?;
    write_layering_fixture(guard.path(), false)?;
    run_in(
        guard.path(),
        "cargo",
        &[
            "deny",
            "--manifest-path",
            "Cargo.toml",
            "--config",
            root().join("deny.toml").to_string_lossy().as_ref(),
            "check",
            "-A",
            "unused-wrapper",
            "bans",
        ],
    )?;

    write_layering_fixture(guard.path(), true)?;
    let output = Command::new("cargo")
        .args([
            "deny",
            "--manifest-path",
            "Cargo.toml",
            "--config",
            root().join("deny.toml").to_string_lossy().as_ref(),
            "check",
            "-A",
            "unused-wrapper",
            "bans",
        ])
        .current_dir(guard.path())
        .output()
        .map_err(|error| format!("failed to run planted layering violation: {error}"))?;
    if output.status.success() {
        return Err("cargo-deny accepted a planted vendor dependency in core".to_owned());
    }
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    if !diagnostic.contains("banned") || !diagnostic.contains("steward-controller") {
        return Err(
            "cargo-deny rejected the fixture for the wrong reason; expected the planted core dependency"
                .to_owned(),
        );
    }
    println!("layering-test: allowed wrapper passed and planted core violation was rejected");
    Ok(())
}

fn write_layering_fixture(directory: &Path, include_violation: bool) -> TaskResult {
    let members = if include_violation {
        "\"vendor\", \"adapter\", \"core\""
    } else {
        "\"vendor\", \"adapter\""
    };
    write_file(
        &directory.join("Cargo.toml"),
        &format!("[workspace]\nresolver = \"2\"\nmembers = [{members}]\n"),
    )?;
    write_crate(directory, "vendor", "openshell-sdk", "")?;
    write_crate(
        directory,
        "adapter",
        "steward-adapter-openshell",
        "openshell-sdk = { path = \"../vendor\", version = \"=0.0.0\" }",
    )?;
    if include_violation {
        write_crate(
            directory,
            "core",
            "steward-controller",
            "openshell-sdk = { path = \"../vendor\", version = \"=0.0.0\" }",
        )?;
    }
    Ok(())
}

fn write_crate(directory: &Path, folder: &str, name: &str, dependencies: &str) -> TaskResult {
    let crate_directory = directory.join(folder);
    fs::create_dir_all(crate_directory.join("src"))
        .map_err(|error| format!("failed to create fixture crate {folder}: {error}"))?;
    write_file(
        &crate_directory.join("Cargo.toml"),
        &format!(
            "[package]\nname = \"{name}\"\nversion = \"0.0.0\"\nedition = \"2024\"\nlicense = \"Apache-2.0\"\n\n[dependencies]\n{dependencies}\n"
        ),
    )?;
    write_file(&crate_directory.join("src/lib.rs"), "")?;
    Ok(())
}

fn write_file(path: &Path, content: &str) -> TaskResult {
    fs::write(path, content).map_err(|error| format!("failed to write {}: {error}", path.display()))
}

fn dev(arguments: &[String]) -> TaskResult {
    let Some(operation) = arguments.first().map(String::as_str) else {
        return Err("dev requires doctor".to_owned());
    };
    if arguments.len() != 1 {
        return Err("dev accepts exactly one operation".to_owned());
    }
    match operation {
        "doctor" => dev_doctor(),
        _ => Err("dev requires doctor".to_owned()),
    }
}

fn dev_doctor() -> TaskResult {
    if let Ok(context) = env::var("STEWARD_TEST_KUBE_CONTEXT") {
        validate_local_test_context(&context)?;
    }
    let run_directory = root().join(".steward-run");
    if run_directory.exists() {
        let mut entries = fs::read_dir(&run_directory)
            .map_err(|error| format!("failed to inspect {}: {error}", run_directory.display()))?;
        if entries.next().is_some() {
            return Err(format!(
                "{} contains run artifacts; clean them by their recorded run ID",
                run_directory.display()
            ));
        }
    }
    println!("dev doctor: no Steward run artifacts found; ambient kube context was not used");
    Ok(())
}

fn validate_local_test_context(context: &str) -> TaskResult {
    if local_test_context_is_safe(context) {
        Ok(())
    } else {
        Err(format!(
            "refusing kube context `{context}`; expected kind-steward-* or k3d-steward-*"
        ))
    }
}

fn run(program: &str, arguments: &[&str]) -> TaskResult {
    run_in(&root(), program, arguments)
}

fn run_with_env(program: &str, arguments: &[&str], environment: &[(&str, &OsStr)]) -> TaskResult {
    println!("+ {program} {}", arguments.join(" "));
    let status = Command::new(program)
        .args(arguments)
        .envs(environment.iter().copied())
        .current_dir(root())
        .status()
        .map_err(|error| format!("failed to run {program}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} exited with {status}"))
    }
}

fn run_in(directory: &Path, program: &str, arguments: &[&str]) -> TaskResult {
    println!("+ {program} {}", arguments.join(" "));
    let status = Command::new(program)
        .args(arguments)
        .current_dir(directory)
        .status()
        .map_err(|error| format!("failed to run {program}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} exited with {status}"))
    }
}

fn ensure_directory(path: &Path) -> TaskResult {
    if path.is_dir() {
        Ok(())
    } else {
        Err(format!("required directory is missing: {}", path.display()))
    }
}

fn files_with_extension(directory: &Path, extension: &str) -> Result<Vec<PathBuf>, String> {
    let mut paths = fs::read_dir(directory)
        .map_err(|error| format!("failed to read {}: {error}", directory.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension() == Some(OsStr::new(extension)))
        .collect::<Vec<_>>();
    paths.sort();
    Ok(paths)
}

fn collect_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    collect_files_inner(root, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_files_inner(directory: &Path, files: &mut Vec<PathBuf>) -> TaskResult {
    let entries = fs::read_dir(directory)
        .map_err(|error| format!("failed to read {}: {error}", directory.display()))?;
    for entry in entries {
        let entry =
            entry.map_err(|error| format!("failed to inspect {}: {error}", directory.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| format!("failed to inspect {}: {error}", path.display()))?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            if should_skip_directory(&path) {
                continue;
            }
            collect_files_inner(&path, files)?;
        } else if file_type.is_file() {
            files.push(path);
        }
    }
    Ok(())
}

fn should_skip_directory(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(OsStr::to_str),
        Some(".git" | ".next" | "target" | "node_modules" | ".steward-run" | ".worktrees")
    )
}

fn is_test_path(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(
            component.as_os_str().to_str(),
            Some("tests" | "testdata" | "fixtures")
        )
    }) || path
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| name.contains("_test."))
}

fn display_relative(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

struct TemporaryTree {
    path: PathBuf,
}

impl TemporaryTree {
    fn create(path: PathBuf) -> Result<Self, String> {
        fs::create_dir_all(&path)
            .map_err(|error| format!("failed to create {}: {error}", path.display()))?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TemporaryTree {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.path) {
            eprintln!(
                "warning: failed to remove owned fixture {}: {error}",
                self.path.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        git_command_in_repository, migration_changes, provider_profile_bundle_directory_for_inputs,
        root, should_skip_directory, validate_conformance_test_result,
    };
    use std::fs;
    use std::io::ErrorKind;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_REPOSITORY_ID: AtomicU64 = AtomicU64::new(0);

    fn uses_user_envelope_only_contract() -> Result<bool, String> {
        let chart = fs::read_to_string(root().join("charts/steward/Chart.yaml"))
            .map_err(|error| format!("Steward chart metadata is required: {error}"))?;
        let version = chart
            .lines()
            .find_map(|line| line.strip_prefix("version: "))
            .ok_or_else(|| "Steward chart version is required".to_owned())?;
        match version {
            "0.1.23" => Ok(false),
            "0.2.0" => Ok(true),
            other => Err(format!(
                "release enforcement has not reviewed Steward chart version {other}"
            )),
        }
    }

    fn text_files_below(path: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
        for entry in fs::read_dir(path)
            .map_err(|error| format!("failed to inspect {}: {error}", path.display()))?
        {
            let entry = entry.map_err(|error| {
                format!(
                    "failed to inspect an entry below {}: {error}",
                    path.display()
                )
            })?;
            let entry_path = entry.path();
            if entry_path.is_dir() {
                text_files_below(&entry_path, files)?;
                continue;
            }
            let is_text = entry_path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| {
                    matches!(
                        extension,
                        "rs" | "ts"
                            | "tsx"
                            | "js"
                            | "mjs"
                            | "json"
                            | "yaml"
                            | "yml"
                            | "toml"
                            | "sh"
                            | "md"
                            | "html"
                            | "css"
                    )
                });
            if is_text {
                files.push(entry_path);
            }
        }
        Ok(())
    }

    #[test]
    fn browser_e2e_ci_uses_the_pinned_nextjs_gate() -> Result<(), String> {
        let repository = root();
        let workflow = fs::read_to_string(repository.join(".github/workflows/ci.yml"))
            .map_err(|error| format!("Steward CI workflow is required: {error}"))?;
        let package = fs::read_to_string(repository.join("package.json"))
            .map_err(|error| format!("pinned browser test package is required: {error}"))?;
        let bun_version = fs::read_to_string(repository.join(".bun-version"))
            .map_err(|error| format!("pinned Bun version is required: {error}"))?;
        let journey = fs::read_to_string(repository.join("tests/browser/steward-next.spec.mjs"))
            .map_err(|error| format!("Next browser journey is required: {error}"))?;
        let xtask_source = include_str!("main.rs");

        let browser_job = workflow
            .split("  browser-e2e:")
            .nth(1)
            .and_then(|jobs| jobs.split("\n  pinned:").next())
            .ok_or_else(|| "browser E2E CI job is required".to_owned())?;

        assert!(
            browser_job.contains("bun-version-file: .bun-version"),
            "browser E2E CI must use the repository-pinned Bun runtime"
        );
        assert!(
            browser_job.contains("bunx playwright install --with-deps chromium"),
            "browser E2E CI must install Playwright's pinned Chromium image"
        );
        assert!(
            browser_job.contains("PLAYWRIGHT_BROWSERS_PATH"),
            "browser E2E CI must use its ephemeral pinned browser image directory"
        );
        assert!(
            browser_job.contains("cargo xtask browser-e2e --browser-ready"),
            "browser E2E CI must use the cargo xtask gate"
        );
        assert!(
            browser_job.contains("bun run web:check")
                && browser_job.contains("bun run --cwd web build"),
            "browser E2E CI must reject stale generated clients before building the Next application"
        );
        assert!(
            xtask_source.contains("run(\"bun\", &[\"run\", \"web:check\"])")
                && xtask_source.contains("run(\"bun\", &[\"run\", \"--cwd\", \"web\", \"build\"])")
                && xtask_source.contains("run_with_env(\"bun\", &[\"run\", \"test:browser-e2e\"]"),
            "the local browser gate must verify, build, and exercise the Next.js application"
        );
        assert!(
            package.contains("\"@playwright/test\": \"1.62.1\""),
            "the browser runner must be exact-version pinned in the source manifest"
        );
        assert_eq!(bun_version.trim(), "1.2.21");
        for required in [
            "127.0.0.1",
            "Storage.prototype",
            "consoleErrors",
            "Connect GitHub",
            "Disconnect GitHub",
            "viewport",
        ] {
            assert!(
                journey.contains(required),
                "the browser journey must cover {required}"
            );
        }
        for required in [
            "content-security-policy",
            "strict-dynamic",
            "crossOriginRequests",
            "Sign in required",
            "Forbidden",
            "Workspace view",
            "scrollWidth",
        ] {
            assert!(
                journey.contains(required),
                "the Next browser journey must cover {required}"
            );
        }

        Ok(())
    }

    #[test]
    fn next_presentation_foundation_is_bun_pinned_nonce_safe_and_route_complete()
    -> Result<(), String> {
        let repository = root();
        let package = fs::read_to_string(repository.join("package.json"))
            .map_err(|error| format!("Bun workspace manifest is required: {error}"))?;
        let web_package = fs::read_to_string(repository.join("web/package.json"))
            .map_err(|error| format!("steward-web package manifest is required: {error}"))?;
        let proxy = fs::read_to_string(repository.join("web/src/proxy.ts"))
            .map_err(|error| format!("nonce-compatible Next proxy is required: {error}"))?;
        let layout = fs::read_to_string(repository.join("web/src/app/layout.tsx"))
            .map_err(|error| format!("dynamic Next root layout is required: {error}"))?;
        let api_config =
            fs::read_to_string(repository.join("web/openapi-ts.config.ts")).map_err(|error| {
                format!("typed API client generator configuration is required: {error}")
            })?;
        let session_provider = fs::read_to_string(
            repository.join("web/src/session/session-context.tsx"),
        )
        .map_err(|error| format!("generated-client session bootstrap is required: {error}"))?;

        for required in [
            "\"packageManager\": \"bun@1.2.21\"",
            "\"web:build\"",
            "\"web:check\"",
            "\"web:generate-api\"",
        ] {
            assert!(
                package.contains(required),
                "Bun workspace is missing {required}"
            );
        }
        for required in [
            "\"next\": \"16.3.3\"",
            "\"react\": \"19.2.8\"",
            "\"tailwindcss\": \"4.3.3\"",
            "\"@hey-api/openapi-ts\": \"0.99.0\"",
        ] {
            assert!(
                web_package.contains(required),
                "steward-web dependencies must stay exact-version pinned: {required}"
            );
        }
        for required in [
            "crypto.randomUUID()",
            "script-src 'self' 'nonce-${nonce}' 'strict-dynamic'",
            "style-src 'self' 'nonce-${nonce}'",
            "connect-src 'self'",
            "frame-ancestors 'none'",
            "requestHeaders.set(\"x-nonce\", nonce)",
            "response.headers.set(\"Content-Security-Policy\"",
        ] {
            assert!(
                proxy.contains(required),
                "strict Next CSP is missing {required}"
            );
        }
        assert!(
            layout.contains("await headers()"),
            "the root layout must opt into request rendering so Next applies the CSP nonce to framework scripts"
        );
        assert!(
            api_config.contains("src/api-client") && api_config.contains("@hey-api/client-fetch"),
            "the generated typed client must target web/src/api-client and use same-origin fetch"
        );
        for required in [
            "@/api-client",
            "credentials: \"same-origin\"",
            "cache: \"no-store\"",
            "status: \"loading\"",
            "status: \"authenticated\"",
            "status: \"unauthorized\"",
            "status: \"unavailable\"",
            "status: \"error\"",
        ] {
            assert!(
                session_provider.contains(required),
                "session bootstrap must preserve the explicit state {required}"
            );
        }
        for forbidden in ["localStorage", "sessionStorage", "NEXT_PUBLIC_"] {
            assert!(
                !session_provider.contains(forbidden),
                "session bootstrap must not use browser or public environment storage: {forbidden}"
            );
        }

        for route in [
            "web/src/app/envelopes/page.tsx",
            "web/src/app/envelopes/new/page.tsx",
            "web/src/app/envelopes/[id]/page.tsx",
            "web/src/app/envelopes/[id]/runs/page.tsx",
            "web/src/app/runs/page.tsx",
            "web/src/app/runs/[id]/page.tsx",
            "web/src/app/connections/page.tsx",
            "web/src/app/settings/page.tsx",
            "web/src/app/admin/envelopes/templates/page.tsx",
            "web/src/app/admin/runs/page.tsx",
            "web/src/app/admin/approvals/page.tsx",
            "web/src/app/admin/settings/page.tsx",
            "web/src/app/health/ready/route.ts",
        ] {
            assert!(
                repository.join(route).is_file(),
                "Next presentation route is required: {route}"
            );
        }

        Ok(())
    }

    #[test]
    fn durable_task_orchestration_migration_declares_the_recovery_boundary() -> Result<(), String> {
        let migration_path = root().join("migrations/0028_durable_task_runtime_orchestration.sql");
        let migration = fs::read_to_string(&migration_path).map_err(|error| {
            format!(
                "the durable Task orchestration migration must exist at {}: {error}",
                migration_path.display()
            )
        })?;

        for required in [
            "CREATE TABLE task_runtime_operations",
            "CREATE TABLE task_execution_attempts",
            "CREATE TABLE external_effect_outbox",
            "CREATE TABLE task_orchestration_journal",
            "generation bigint",
            "runtime_create_pending",
            "runtime_observed",
            "approval_pending",
            "activation_pending",
            "cleanup_pending",
            "outcome_unknown",
            "cancel_requested",
            "runtime_absent_observed_at",
            "projections_absent_observed_at",
            "runtime_create_authorized_at",
            "task_commands_are_monotonic",
            "external_effect_outbox_transition_is_monotonic",
        ] {
            assert!(
                migration.contains(required),
                "the durable Task orchestration migration is missing {required}"
            );
        }

        Ok(())
    }

    #[test]
    fn task_orchestration_rollout_is_staged_before_the_new_owner_starts() -> Result<(), String> {
        let chart = fs::read_to_string(root().join("charts/steward/templates/all.yaml"))
            .map_err(|error| format!("Steward chart is required: {error}"))?;
        let controller = fs::read_to_string(root().join("bins/steward-controller/src/main.rs"))
            .map_err(|error| format!("controller binary is required: {error}"))?;
        let apiserver = fs::read_to_string(root().join("bins/steward-apiserver/src/main.rs"))
            .map_err(|error| format!("apiserver binary is required: {error}"))?;

        assert!(
            chart.matches("STEWARD_TASK_ORCHESTRATION_MODE").count() >= 2
                && controller.contains("task_orchestration_mode")
                && apiserver.contains("task_orchestration_mode"),
            "the initial rollout must put both Task writers behind one explicit staged mode"
        );
        Ok(())
    }

    #[test]
    fn active_task_operations_revalidate_authority_before_runtime_observation() -> Result<(), String>
    {
        let controller = fs::read_to_string(root().join("crates/steward-controller/src/lib.rs"))
            .map_err(|error| format!("controller source is required: {error}"))?;
        assert!(
            controller.contains("revalidate_active_task_authority"),
            "an active Task must revalidate authority even when execution has not been requested"
        );
        Ok(())
    }

    #[test]
    fn retryable_attempt_observation_errors_are_not_terminalized() -> Result<(), String> {
        let controller = fs::read_to_string(root().join("crates/steward-controller/src/lib.rs"))
            .map_err(|error| format!("controller source is required: {error}"))?;
        assert!(
            controller.contains(".map_err(TaskControllerError::Sandbox)?"),
            "a retryable sandbox observation failure must preserve the current attempt state"
        );
        Ok(())
    }

    #[test]
    fn openshell_attempt_markers_prove_running_process_liveness() -> Result<(), String> {
        let adapter = fs::read_to_string(root().join("adapters/openshell/src/lib.rs"))
            .map_err(|error| format!("OpenShell adapter source is required: {error}"))?;
        assert!(
            adapter.contains("pid-start")
                && adapter.contains("kill -0")
                && adapter.contains("heartbeat")
                && adapter.contains("now - heartbeat"),
            "claimed/running attempt markers must carry a cross-exec heartbeat lease and expire without liveness proof"
        );
        Ok(())
    }

    #[test]
    fn absent_cancelled_attempts_expire_into_a_terminal_observation() -> Result<(), String> {
        let controller = fs::read_to_string(root().join("crates/steward-controller/src/lib.rs"))
            .map_err(|error| format!("controller source is required: {error}"))?;
        assert!(
            controller.contains("terminalize_expired_attempt_observation"),
            "cancellation must resolve an absent post-start attempt after its observation deadline"
        );
        Ok(())
    }

    #[test]
    fn cleanup_retires_task_approval_authority_before_finalization() -> Result<(), String> {
        let store = fs::read_to_string(root().join("crates/steward-store/src/lib.rs"))
            .map_err(|error| format!("store source is required: {error}"))?;
        let controller = fs::read_to_string(root().join("crates/steward-controller/src/lib.rs"))
            .map_err(|error| format!("controller source is required: {error}"))?;
        assert!(
            store.contains("retire_task_authority_for_cleanup")
                && !controller.contains("owned_projections_absent: true"),
            "cleanup must revoke grants and retire pending approval delivery before recording projection absence"
        );
        Ok(())
    }

    #[test]
    fn task_codex_provider_profiles_cover_supported_linux_architectures() -> Result<(), String> {
        let arm64_binary = "/usr/lib/node_modules/@openai/codex/node_modules/@openai/codex-linux-arm64/vendor/aarch64-unknown-linux-musl/bin/codex";
        let amd64_binary = "/usr/lib/node_modules/@openai/codex/node_modules/@openai/codex-linux-x64/vendor/x86_64-unknown-linux-musl/bin/codex";
        for profile_path in [
            "config/task/tool-provider-profile.yaml",
            "config/task/inference-provider-profile.yaml",
        ] {
            let profile = fs::read_to_string(root().join(profile_path)).map_err(|error| {
                format!("Codex provider profile {profile_path} is required: {error}")
            })?;
            for required_binary in [arm64_binary, amd64_binary] {
                assert!(
                    profile.contains(required_binary),
                    "Codex provider profile {profile_path} must authorize {required_binary}"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn production_execution_binding_code_contains_no_deployment_agent_values() -> Result<(), String>
    {
        let production_sources = [
            "crates/steward-types/src/lib.rs",
            "crates/steward-apiserver/src/execution_bindings.rs",
            "crates/steward-apiserver/src/tasks.rs",
            "crates/steward-apiserver/src/workflows.rs",
            "adapters/openshell/src/lib.rs",
            "bins/steward-apiserver/src/main.rs",
        ];
        let forbidden = [
            "SUPPORTED_WORKFLOW_AGENT",
            "codex@0.140.0",
            "codex-cli 0.140.0",
            "steward-runtime-providers@1.3.0",
            "steward-mcp-gw-v1-3-0",
            "steward-litellm-v1-3-0",
            "@openai/codex-linux-",
        ];
        for path in production_sources {
            let source = fs::read_to_string(root().join(path))
                .map_err(|error| format!("read production source {path}: {error}"))?;
            for value in forbidden {
                assert!(
                    !source.contains(value),
                    "production source {path} embeds deployment-owned value {value}"
                );
            }
        }

        let core_task_sources = [
            "crates/steward-apiserver/src/execution_bindings.rs",
            "crates/steward-apiserver/src/tasks.rs",
        ];
        let adapter_owned_values = [
            "codex",
            "CODEX_HOME",
            "litellm-litellm",
            "STEWARD_MCP_GW_BEARER_TOKEN",
            "openshell-token-grant-placeholder",
        ];
        for path in core_task_sources {
            let source = fs::read_to_string(root().join(path))
                .map_err(|error| format!("read core Task source {path}: {error}"))?;
            for value in adapter_owned_values {
                assert!(
                    !source.contains(value),
                    "core Task source {path} embeds adapter-owned value {value}"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn released_provider_profile_bundle_1_0_0_remains_immutable() -> Result<(), String> {
        for (released_path, expected_sha256) in [
            (
                "config/provider-profile-bundle/v1/README.md",
                "a583d69ae143cc1499dc9d25e6addc29a8a95643cd305c3b7b267520e2213849",
            ),
            (
                "config/provider-profile-bundle/v1/bundle.json",
                "5b1abc27b1ce69da6ad3dc406973ab95c1ea0fe861e00f85af5b3df8ed95401d",
            ),
            (
                "config/provider-profile-bundle/v1/profiles/steward-litellm.json",
                "f9061708ee8b0f9f7bf2e821f7739444e93405a0759293e308198e64198d9b11",
            ),
            (
                "config/provider-profile-bundle/v1/profiles/steward-mcp-gw.json",
                "6b56705eecf7bb06fce0f779bd571334fa9aa0c06d812eecad28f4db4223449f",
            ),
        ] {
            let output = Command::new("sha256sum")
                .arg(root().join(released_path))
                .output()
                .map_err(|error| {
                    format!("sha256sum is required to verify {released_path}: {error}")
                })?;
            if !output.status.success() {
                return Err(format!(
                    "sha256sum failed for released provider-profile file {released_path}: {}",
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
            let actual_sha256 = String::from_utf8(output.stdout)
                .map_err(|error| format!("sha256sum output for {released_path} is UTF-8: {error}"))?
                .split_whitespace()
                .next()
                .ok_or_else(|| format!("sha256sum returned no digest for {released_path}"))?
                .to_owned();
            assert_eq!(
                actual_sha256, expected_sha256,
                "released steward-runtime-providers@1.0.0 file {released_path} must remain byte-for-byte immutable"
            );
        }
        Ok(())
    }

    #[test]
    fn released_provider_profile_bundle_1_1_0_remains_immutable() -> Result<(), String> {
        for (released_path, expected_sha256) in [
            (
                "config/provider-profile-bundle/v1.1.0/README.md",
                "0fad547a4d77bfaca6e2497dc990dad00e4465da4aef0b8e16b0b4891b2def04",
            ),
            (
                "config/provider-profile-bundle/v1.1.0/bundle.json",
                "ced8f21b81737c7ed411fdc5f985d11e30b5513effd58a5ae7187d0d32268dfb",
            ),
            (
                "config/provider-profile-bundle/v1.1.0/profiles/steward-litellm.json",
                "575c361f126601e1882f3634a1193ca731810f9b512ad0e305990cf7f91d577b",
            ),
            (
                "config/provider-profile-bundle/v1.1.0/profiles/steward-mcp-gw.json",
                "b2f5883be97fa4ede9e32260c487c1eb4cf0ea3ad94d6f9a6b9450da4f6eb6c1",
            ),
        ] {
            let output = Command::new("sha256sum")
                .arg(root().join(released_path))
                .output()
                .map_err(|error| {
                    format!("sha256sum is required to verify {released_path}: {error}")
                })?;
            if !output.status.success() {
                return Err(format!(
                    "sha256sum failed for released provider-profile file {released_path}: {}",
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
            let actual_sha256 = String::from_utf8(output.stdout)
                .map_err(|error| format!("sha256sum output for {released_path} is UTF-8: {error}"))?
                .split_whitespace()
                .next()
                .ok_or_else(|| format!("sha256sum returned no digest for {released_path}"))?
                .to_owned();
            assert_eq!(
                actual_sha256, expected_sha256,
                "released steward-runtime-providers@1.1.0 file {released_path} must remain byte-for-byte immutable"
            );
        }
        Ok(())
    }

    #[test]
    fn provider_profile_inputs_select_their_exact_immutable_bundle() -> Result<(), String> {
        for (version, expected_suffix) in
            [("1.0.0", "/v1"), ("1.1.0", "/v1.1.0"), ("1.2.0", "/v1.2.0")]
        {
            let inputs = serde_json::json!({
                "bundle": {"id": "steward-runtime-providers", "version": version}
            });
            let directory = provider_profile_bundle_directory_for_inputs(&inputs.to_string())?;
            assert!(
                directory.to_string_lossy().ends_with(expected_suffix),
                "provider profile inputs for {version} must select {expected_suffix}, got {}",
                directory.display()
            );
        }
        let unsupported = serde_json::json!({
            "bundle": {"id": "steward-runtime-providers", "version": "1.3.0"}
        });
        let result = provider_profile_bundle_directory_for_inputs(&unsupported.to_string());
        assert!(
            matches!(result, Err(ref error) if error.contains("unsupported") && error.contains("1.3.0")),
            "unknown bundle identities must fail closed: {result:?}"
        );
        Ok(())
    }

    #[test]
    fn release_candidate_fails_closed_on_critical_component_images() -> Result<(), String> {
        let workflow = fs::read_to_string(root().join(".github/workflows/ci.yml"))
            .map_err(|error| format!("Steward CI workflow is required: {error}"))?;
        let release_candidate = workflow
            .split("  release-candidate:")
            .nth(1)
            .and_then(|jobs| jobs.split("\n  pinned:").next())
            .ok_or_else(|| "release-candidate CI job is required".to_owned())?;

        for component in ["apiserver", "controller", "mint", "bridge"] {
            assert!(
                release_candidate.contains(&format!(
                    "image-ref: steward-{component}:release-validation"
                )),
                "release-candidate CI must scan the {component} production image"
            );
        }
        assert_eq!(
            release_candidate
                .matches("aquasecurity/trivy-action@a9c7b0f06e461e9d4b4d1711f154ee024b8d7ab8")
                .count(),
            4,
            "release-candidate CI must use the pinned Trivy action for every component image"
        );
        assert_eq!(
            release_candidate.matches("exit-code: \"1\"").count(),
            4,
            "every release-candidate image scan must fail closed"
        );
        assert_eq!(
            release_candidate.matches("severity: CRITICAL").count(),
            4,
            "every release-candidate image scan must enforce CRITICAL findings"
        );

        Ok(())
    }

    #[test]
    fn openshell_adapter_does_not_select_a_public_sandbox_image() -> Result<(), String> {
        let source = fs::read_to_string(root().join("adapters/openshell/src/lib.rs"))
            .map_err(|error| format!("OpenShell adapter source is required: {error}"))?;
        let contract =
            fs::read_to_string(root().join("adapters/openshell/examples/workspace_contract.rs"))
                .map_err(|error| {
                    format!("OpenShell adapter contract example is required: {error}")
                })?;

        for adapter in [source, contract] {
            assert!(
                !adapter.contains("ghcr.io/nvidia/openshell-community/sandboxes/"),
                "the Steward adapter must leave sandbox image selection to the configured OpenShell gateway"
            );
        }

        Ok(())
    }

    #[test]
    fn openshell_chart_requires_verified_authenticated_gateway_transport() -> Result<(), String> {
        let chart = root().join("charts/steward");
        let values = fs::read_to_string(chart.join("values.yaml"))
            .map_err(|error| format!("published Steward chart values are required: {error}"))?;
        let schema = fs::read_to_string(chart.join("values.schema.json"))
            .map_err(|error| format!("published Steward values schema is required: {error}"))?;
        let templates = fs::read_to_string(chart.join("templates/all.yaml")).map_err(|error| {
            format!("published Steward Kubernetes templates are required: {error}")
        })?;

        for required in [
            "openshellEndpoint",
            "openshellServerName",
            "openshellRuntimeClassName",
            "openshellTaskLogMode",
            "workloadExchangeEndpoint",
            "workloadExchangeServerName",
            "workloadExchangeTrust",
            "openshellClient",
            "caCertificate",
            "clientCertificate",
            "clientPrivateKey",
        ] {
            assert!(
                values.contains(required),
                "chart values must expose the required OpenShell setting or Secret reference {required}"
            );
            assert!(
                schema.contains(required),
                "the values schema must require the OpenShell setting or Secret reference {required}"
            );
        }
        for forbidden in ["bearerToken", "clientBearerToken"] {
            assert!(
                !values.contains(forbidden) && !schema.contains(forbidden),
                "the OpenShell client Secret contract must not contain workload token setting {forbidden}"
            );
        }
        assert!(
            !schema.contains("\"const\": \"sandbox-vm\"")
                && schema.contains("openshellRuntimeClassName"),
            "the chart schema must accept valid Kubernetes RuntimeClass names without encoding a Kata-only contract"
        );
        for environment_variable in [
            "STEWARD_OPENSHELL_CA_CERTIFICATE_FILE",
            "STEWARD_OPENSHELL_CLIENT_CERTIFICATE_FILE",
            "STEWARD_OPENSHELL_CLIENT_PRIVATE_KEY_FILE",
            "STEWARD_WORKLOAD_EXCHANGE_ENDPOINT",
            "STEWARD_WORKLOAD_EXCHANGE_SERVER_NAME",
            "STEWARD_WORKLOAD_EXCHANGE_CA_CERTIFICATE_FILE",
            "STEWARD_WORKLOAD_SOURCE_CREDENTIAL_FILE",
            "STEWARD_OPENSHELL_SERVER_NAME",
            "STEWARD_OPENSHELL_RUNTIME_CLASS_NAME",
            "STEWARD_OPENSHELL_TASK_LOG_MODE",
        ] {
            assert!(
                templates.contains(environment_variable),
                "the controller deployment must receive {environment_variable}"
            );
        }
        assert!(
            !values.contains("openshellEndpoint: http://"),
            "the published chart must not default OpenShell transport to plaintext gRPC"
        );
        assert!(
            values.contains("openshell: 8080"),
            "the default NetworkPolicy must permit the OpenShell v0.0.98 gateway TLS service port"
        );
        for projected_token_contract in [
            "serviceAccountToken:",
            "audience: apelogic-workload-exchange",
            "expirationSeconds: 600",
            "mountPath: /var/run/secrets/steward/workload",
            "value: /var/run/secrets/steward/workload/source-token",
        ] {
            assert!(
                templates.contains(projected_token_contract),
                "the controller must consume the rotating OpenShell workload token contract {projected_token_contract}"
            );
        }
        assert_eq!(
            templates.matches("path: source-token").count(),
            1,
            "only the controller workload projection may provide a source-token credential"
        );
        for governed_connection_contract in [
            "STEWARD_CONNECTIONS_BRIDGE_ARTIFACT_TRUST_MODE",
            "STEWARD_CONNECTIONS_BRIDGE_IMAGE",
            "STEWARD_CONNECTIONS_MCP_GW_ORIGIN",
            "STEWARD_CONNECTIONS_MCP_GW_VERSION",
            "STEWARD_CONNECTIONS_RUNTIME_NAMESPACE",
            "connections-bridge-attestation",
        ] {
            assert!(
                templates.contains(governed_connection_contract),
                "the governed Connections path must render {governed_connection_contract}"
            );
        }
        assert!(
            !templates.contains("STEWARD_OPENSHELL_BEARER_TOKEN_FILE")
                && !templates.contains("audience: openshell-api"),
            "the chart must never send a raw Kubernetes service-account token to OpenShell"
        );
        assert!(
            !values.contains("workloadExchangeRoles")
                && !values.contains("workloadExchangeAlgorithm"),
            "the caller must not select exchange roles or a signing algorithm"
        );

        Ok(())
    }

    #[test]
    fn openshell_task_process_logging_contract_is_explicit() -> Result<(), String> {
        let controller = fs::read_to_string(root().join("bins/steward-controller/src/main.rs"))
            .map_err(|error| format!("Steward controller source is required: {error}"))?;
        let adapter = fs::read_to_string(root().join("adapters/openshell/src/lib.rs"))
            .map_err(|error| format!("OpenShell adapter source is required: {error}"))?;
        let values = fs::read_to_string(root().join("charts/steward/values.yaml"))
            .map_err(|error| format!("Steward chart values are required: {error}"))?;
        let schema = fs::read_to_string(root().join("charts/steward/values.schema.json"))
            .map_err(|error| format!("Steward values schema is required: {error}"))?;
        let templates = fs::read_to_string(root().join("charts/steward/templates/all.yaml"))
            .map_err(|error| format!("Steward chart templates are required: {error}"))?;
        let chart_readme = fs::read_to_string(root().join("charts/steward/README.md"))
            .map_err(|error| format!("Steward chart README is required: {error}"))?;

        assert!(
            controller.contains("STEWARD_OPENSHELL_TASK_LOG_MODE"),
            "the controller must parse STEWARD_OPENSHELL_TASK_LOG_MODE at startup"
        );
        assert!(
            adapter.contains("exec_sandbox"),
            "the OpenShell task path must consume the live sandbox execution stream"
        );
        for required_field in ["runtime_uid", "workspace", "sandbox", "stream", "message"] {
            assert!(
                adapter.contains(required_field),
                "each task-process log record must carry {required_field}"
            );
        }
        assert!(
            adapter.contains("escaped_task_log_value(message)") && !adapter.contains("bytes={}"),
            "full task logging must copy escaped stdout and stderr into controller logs"
        );
        assert!(
            values.contains("openshellTaskLogMode: \"off\""),
            "the chart must default OpenShell task-process logging to off"
        );
        assert!(
            schema.contains("openshellTaskLogMode")
                && schema.contains("\"enum\": [\"off\", \"full\"]"),
            "the values schema must restrict openshellTaskLogMode to off or full"
        );
        assert!(
            templates.contains("STEWARD_OPENSHELL_TASK_LOG_MODE"),
            "the controller Deployment must receive STEWARD_OPENSHELL_TASK_LOG_MODE"
        );
        assert!(
            chart_readme.contains("**Warning:** `full` logging copies task-controlled output")
                && chart_readme.contains("without redaction")
                && chart_readme.contains("credentials, or other")
                && chart_readme.contains("sensitive information"),
            "the chart README must warn that full task logging may expose sensitive output"
        );

        Ok(())
    }

    #[test]
    fn task_copy_smoke_contract_is_authority_bounded_and_idempotently_bootstrapped()
    -> Result<(), String> {
        if uses_user_envelope_only_contract()? {
            for removed in [
                "config/task/workflows.example.json",
                "config/task/steward-run-service-envelope.example.json",
                "scripts/bootstrap-task-copy-smoke.sh",
            ] {
                assert!(
                    !root().join(removed).exists(),
                    "v0.2 must remove legacy Task bootstrap artifact {removed}"
                );
            }
            let contract = fs::read_to_string(root().join("config/task/README.md"))
                .map_err(|error| format!("Task production configuration is required: {error}"))?;
            for required in [
                "exactly one active, provisioned User Envelope",
                "steward.capability-catalog/v1",
                "grants no Task authority",
            ] {
                assert!(
                    contract.contains(required),
                    "v0.2 Task contract must state `{required}`"
                );
            }
            return Ok(());
        }

        let workflows_path = root().join("config/task/workflows.example.json");
        let workflows = serde_json::from_str::<serde_json::Value>(
            &fs::read_to_string(&workflows_path)
                .map_err(|error| format!("Task workflow catalog is required: {error}"))?,
        )
        .map_err(|error| format!("Task workflow catalog must be valid JSON: {error}"))?;
        let copy_smoke = workflows
            .as_array()
            .and_then(|workflows| {
                workflows.iter().find(|workflow| {
                    workflow.get("name").and_then(serde_json::Value::as_str) == Some("copy-smoke")
                })
            })
            .ok_or_else(|| "production Task catalog must include copy-smoke".to_owned())?;
        for authority in ["llms", "tools"] {
            assert!(
                copy_smoke
                    .get(authority)
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(Vec::is_empty),
                "copy-smoke must not request {authority} authority"
            );
        }
        assert_eq!(
            copy_smoke
                .pointer("/budget/monthlyLimit")
                .and_then(serde_json::Value::as_str),
            Some("0.00"),
            "copy-smoke must not reserve inference spend"
        );
        let command = copy_smoke
            .get("command")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| "copy-smoke command must be an argv array".to_owned())?
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect::<Vec<_>>();
        assert_eq!(
            command,
            vec![
                "/bin/sh",
                "-c",
                "set -eu; mkdir -p \"$STEWARD_OUTPUT_DIR/out\"; cp in/payload.bin \"$STEWARD_OUTPUT_DIR/out/payload.bin\"",
            ],
            "copy-smoke must only copy the declared input to the declared output root"
        );

        let envelope_path = root().join("config/task/steward-run-service-envelope.example.json");
        let envelope = serde_json::from_str::<serde_json::Value>(
            &fs::read_to_string(&envelope_path)
                .map_err(|error| format!("copy-smoke service envelope is required: {error}"))?,
        )
        .map_err(|error| format!("copy-smoke service envelope must be valid JSON: {error}"))?;
        for authority in ["llms", "tools"] {
            assert!(
                envelope
                    .pointer(&format!("/spec/{authority}"))
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(Vec::is_empty),
                "copy-smoke service envelope must grant no {authority} authority"
            );
        }
        assert_eq!(
            envelope
                .pointer("/spec/budget/monthlyLimit")
                .and_then(serde_json::Value::as_str),
            Some("0.00"),
            "copy-smoke service envelope must grant no inference budget"
        );

        let bootstrap = fs::read_to_string(root().join("scripts/bootstrap-task-copy-smoke.sh"))
            .map_err(|error| format!("copy-smoke bootstrap procedure is required: {error}"))?;
        for required in [
            "STEWARD_APISERVER_URL",
            "STEWARD_APISERVER_CA_CERTIFICATE_FILE",
            "STEWARD_SERVICE_ENVELOPE_BOOTSTRAP_TOKEN_FILE",
            "steward-run-service-envelope.example.json",
            "/admin/service-envelopes/steward-run",
            "--cacert",
            "--config",
        ] {
            assert!(
                bootstrap.contains(required),
                "copy-smoke bootstrap procedure must use {required}"
            );
        }
        assert!(
            bootstrap.contains("200|201"),
            "copy-smoke bootstrap must treat a matching existing envelope as success"
        );
        assert!(
            bootstrap.contains("STEWARD_APISERVER_URL must use HTTPS"),
            "copy-smoke bootstrap must reject plaintext transport"
        );
        assert!(
            !bootstrap.contains("--header \"Authorization:"),
            "copy-smoke bootstrap must not expose its bearer token in process arguments"
        );

        Ok(())
    }

    #[test]
    fn task_bootstrap_publishes_its_route_scoped_identity_contract() -> Result<(), String> {
        let contract = fs::read_to_string(root().join("config/task/README.md"))
            .map_err(|error| format!("Task production configuration is required: {error}"))?;
        if uses_user_envelope_only_contract()? {
            for removed in [
                "agents.apelogic.ai/service-envelope-bootstrap:steward-run",
                "STEWARD_RUN_SERVICE_ENVELOPE_BOOTSTRAP_GROUP",
                "/admin/service-envelopes",
            ] {
                assert!(
                    !contract.contains(removed),
                    "v0.2 Task contract must not retain `{removed}`"
                );
            }
            return Ok(());
        }
        for required in [
            "agents.apelogic.ai/service-envelope-bootstrap:steward-run",
            "steward-task-api",
            "STEWARD_KUBERNETES_TOKEN_REVIEW_AUDIENCE",
            "https://kubernetes.default.svc",
            "DEV EKS OIDC identity-provider",
            "must not be stored in a Kubernetes Secret",
            "route-scoped authorization contract",
            "Infra's short-lived token exchange",
        ] {
            assert!(
                contract.contains(required),
                "Task bootstrap authority contract must state `{required}`"
            );
        }
        Ok(())
    }

    #[test]
    fn v020_live_surfaces_reject_service_envelope_drift() -> Result<(), String> {
        if !uses_user_envelope_only_contract()? {
            return Ok(());
        }

        let repository = root();
        let mut files = vec![repository.join("README.md")];
        for relative in ["bins", "crates", "web", "charts", "config", "scripts"] {
            text_files_below(&repository.join(relative), &mut files)?;
        }
        for relative in [
            "docs/README.md",
            "docs/admin-agent-runs-api-v1.md",
            "docs/contracts/task/v2/README.md",
            "docs/github-actions-generator.md",
            "docs/installation/installation-guide.md",
            "docs/task-runtime-orchestration.md",
            "docs/task-submission-api.md",
        ] {
            files.push(repository.join(relative));
        }

        let forbidden = [
            "/admin/service-envelopes",
            "service-envelope-bootstrap",
            "latest_service_envelope",
            "insert_service_envelope",
        ];
        for file in files {
            let contents = fs::read_to_string(&file)
                .map_err(|error| format!("failed to read {}: {error}", file.display()))?;
            for value in forbidden {
                assert!(
                    !contents.contains(value),
                    "live v0.2 surface {} contains forbidden Service Envelope reference `{value}`",
                    file.strip_prefix(&repository).unwrap_or(&file).display()
                );
            }
        }
        Ok(())
    }

    #[test]
    fn delegated_task_authentication_uses_one_kubernetes_token_review_audience()
    -> Result<(), String> {
        let chart = root().join("charts/steward");
        let values = fs::read_to_string(chart.join("values.yaml"))
            .map_err(|error| format!("published Steward chart values are required: {error}"))?;
        let schema = fs::read_to_string(chart.join("values.schema.json"))
            .map_err(|error| format!("published Steward values schema is required: {error}"))?;
        let templates = fs::read_to_string(chart.join("templates/all.yaml")).map_err(|error| {
            format!("published Steward Kubernetes templates are required: {error}")
        })?;
        let apiserver_source =
            fs::read_to_string(root().join("crates/steward-apiserver/src/lib.rs"))
                .map_err(|error| format!("Steward apiserver source is required: {error}"))?;
        let tasks = fs::read_to_string(root().join("crates/steward-apiserver/src/tasks.rs"))
            .map_err(|error| format!("Steward Task source is required: {error}"))?;
        let values = serde_saphyr::from_str::<serde_json::Value>(&values).map_err(|error| {
            format!("published Steward chart values must be valid YAML: {error}")
        })?;
        let apiserver = values
            .pointer("/config/apiserver")
            .ok_or_else(|| "chart apiserver configuration is required".to_owned())?;
        let token_review_audience = apiserver
            .get("kubernetesTokenReviewAudience")
            .and_then(serde_json::Value::as_str);
        assert_eq!(
            token_review_audience,
            Some("https://kubernetes.default.svc"),
            "all delegated TokenReviews must use the configured Kubernetes API server audience"
        );
        for legacy in ["tokenAudience", "taskTokenAudience"] {
            assert!(
                apiserver.get(legacy).is_none(),
                "ambiguous legacy audience setting {legacy} must not remain in rendered configuration"
            );
            assert!(
                !schema.contains(&format!("\"{legacy}\"")),
                "ambiguous legacy audience setting {legacy} must not remain in the schema"
            );
        }
        assert!(
            schema.contains("kubernetesTokenReviewAudience"),
            "values schema must require the delegated TokenReview audience"
        );
        for required in [
            "STEWARD_KUBERNETES_TOKEN_REVIEW_AUDIENCE",
            ".Values.config.apiserver.kubernetesTokenReviewAudience",
        ] {
            assert!(
                templates.contains(required),
                "rendered apiserver configuration is missing {required}"
            );
        }
        for legacy_environment in ["STEWARD_TOKEN_AUDIENCE", "STEWARD_TASK_TOKEN_AUDIENCE"] {
            assert!(
                !templates.contains(legacy_environment),
                "rendered apiserver configuration must not retain {legacy_environment}"
            );
        }
        for source in [&apiserver_source, &tasks] {
            assert!(
                source.contains("token_review_request("),
                "every delegated authentication path must use the shared TokenReview request builder"
            );
            assert!(
                source.contains("authenticated_token_review_user("),
                "every delegated authentication path must use the shared fail-closed response validator"
            );
        }
        assert!(
            !tasks.contains("TokenReviewSpec"),
            "Task authentication must not grow an independent TokenReview request path"
        );
        Ok(())
    }

    #[test]
    fn openshell_v0098_adapter_integration_is_a_required_ci_lane() -> Result<(), String> {
        let ci = fs::read_to_string(root().join(".github/workflows/ci.yml"))
            .map_err(|error| format!("Steward CI workflow is required: {error}"))?;
        let adapter_manifest = fs::read_to_string(root().join("adapters/openshell/Cargo.toml"))
            .map_err(|error| format!("OpenShell adapter manifest is required: {error}"))?;
        let harness = fs::read_to_string(root().join("scripts/openshell-adapter-e2e.sh")).map_err(
            |error| format!("OpenShell adapter integration harness is required: {error}"),
        )?;
        let e2e_source = fs::read_to_string(root().join("e2e/openshell_adapter_v0098.rs"))
            .map_err(|error| format!("OpenShell adapter integration test is required: {error}"))?;
        let chart_readme = fs::read_to_string(root().join("charts/steward/README.md"))
            .map_err(|error| format!("Steward chart README is required: {error}"))?;

        assert!(
            ci.contains("cargo xtask e2e-openshell-adapter"),
            "CI must execute the real OpenShell adapter integration lane"
        );
        assert!(
            adapter_manifest.contains("832841295992f0112f43f27de5d68213376ff3cb"),
            "the runtime adapter must pin the exact OpenShell v0.0.98 source revision"
        );
        for required in [
            "OPEN_SHELL_RELEASE=\"v0.0.98\"",
            "KIND_NODE_IMAGE=\"kindest/node:v1.32.1@sha256:6afef2b7f69d627ea7bf27ee6696b6868d18e03bf98167c420df486da4662db6\"",
            "--image \"${KIND_NODE_IMAGE}\"",
            "server.oidc.issuer=",
            "--test openshell_adapter_v0098",
        ] {
            assert!(
                harness.contains(required),
                "OpenShell adapter integration harness is missing {required}"
            );
        }
        let explicit_runtime_lane = harness
            .contains("server.defaultRuntimeClassName=openshell-runc")
            && harness.contains("STEWARD_OPENSHELL_RUNTIME_CLASS_NAME=openshell-runc")
            && harness.contains("handler: runc")
            && e2e_source.contains("assert_runtime_class_propagation");
        let default_runtime_lane = harness
            .contains("adapter_round_trip_is_authenticated_with_default_runtime_and_cleanup")
            && !harness.contains("server.defaultRuntimeClassName=")
            && !harness.contains("STEWARD_OPENSHELL_RUNTIME_CLASS_NAME=")
            && !harness.contains("kind: RuntimeClass")
            && e2e_source.contains("assert_default_runtime_class");
        assert!(
            (explicit_runtime_lane || default_runtime_lane) && !e2e_source.contains("kata_bound"),
            "the kind lane must enforce either the existing explicit RuntimeClass contract or the cluster default runtime, never Kata isolation"
        );
        assert!(
            chart_readme.contains("does not prove a VM isolation boundary"),
            "the chart documentation must not overstate runtime-class propagation as VM isolation"
        );

        let adapter_source = fs::read_to_string(root().join("adapters/openshell/src/lib.rs"))
            .map_err(|error| format!("OpenShell adapter source is required: {error}"))?;
        assert!(
            !adapter_source.contains("driver_config"),
            "the adapter must not expose per-create OpenShell driver or scheduler overrides"
        );

        Ok(())
    }

    #[test]
    fn customer_install_guide_verifies_and_installs_the_oci_chart_by_digest() -> Result<(), String>
    {
        let guide = fs::read_to_string(root().join("docs/installation/installation-guide.md"))
            .map_err(|error| format!("customer installation guide is required: {error}"))?;

        for required in [
            "Helm 3.17.0 or newer",
            "STEWARD_CHART_REF=\"oci://${STEWARD_CHART_REPOSITORY}@${STEWARD_CHART_DIGEST}\"",
            "helm pull \"${STEWARD_CHART_REF}\" --destination \"${STEWARD_CHART_DIRECTORY}\"",
            "awk '$1 == \"Digest:\" { print $2 }'",
            "test \"${resolved_chart_digest}\" = \"${STEWARD_CHART_DIGEST}\"",
            "upgrade --install steward \"${STEWARD_CHART_PACKAGE}\"",
        ] {
            assert!(
                guide.contains(required),
                "customer installation guide is missing the copyable OCI chart command: {required}"
            );
        }
        assert!(
            guide.contains("OCI chart digest pull commands were exercised with Helm v3.17.1")
                || guide.contains("| Helm | 3.17+; tested with 3.17.1 |"),
            "customer installation guide must record the tested Helm version"
        );
        assert!(
            guide.contains("STEWARD_CHART_PACKAGE=\"${STEWARD_CHART_DIRECTORY}/steward@sha256-${STEWARD_CHART_DIGEST#sha256:}.tgz\"")
                || guide.contains("STEWARD_CHART_PACKAGE=\"$(find \"${STEWARD_CHART_DIRECTORY}\" -maxdepth 1 -type f -name 'steward*.tgz' -print -quit)\""),
            "customer installation guide must select the pulled chart package"
        );

        Ok(())
    }

    #[test]
    fn customer_install_guide_separates_ordered_administration_from_helm() -> Result<(), String> {
        let guide = fs::read_to_string(root().join("docs/installation/installation-guide.md"))
            .map_err(|error| format!("customer installation guide is required: {error}"))?;

        let administration_start = guide
            .find("## Post-install administration (not Helm installation)")
            .ok_or_else(|| {
                "customer installation guide is missing the administration boundary".to_owned()
            })?;
        let administration_tail = &guide[administration_start..];
        let delivery_start = administration_tail
            .find("## Post-install and delivery tests")
            .ok_or_else(|| {
                "customer installation guide is missing post-install delivery tests".to_owned()
            })?;
        let administration = &administration_tail[..delivery_start];

        for required in [
            "None of these administration records is a Helm installation outcome",
            "Google/browser authentication is conditional",
        ] {
            assert!(
                administration.contains(required),
                "customer installation guide is missing the administration boundary: {required}"
            );
        }

        if uses_user_envelope_only_contract()? {
            for forbidden in [
                "### Operator/service post-install administration",
                "POST /admin/service-envelopes",
                "scripts/bootstrap-task-copy-smoke.sh",
                "service-envelope-bootstrap",
            ] {
                assert!(
                    !administration.contains(forbidden),
                    "v0.2 administration instructions must not retain `{forbidden}`"
                );
            }
            let browser_heading = "### Conditional human browser administration";
            let browser_path = administration
                .split_once(browser_heading)
                .map(|(_, path)| path)
                .ok_or_else(|| {
                    format!("customer installation guide is missing {browser_heading}")
                })?;
            let ordered_steps = [
                "1. **Enable optional human browser administration.**",
                "2. **First login and canonical ID.**",
                "3. **Authorized local RBAC grant.**",
                "4. **Verify capabilities and publish browser governance data.**",
                "5. **User Envelope operation.**",
            ];
            let positions = ordered_steps
                .map(|step| {
                    browser_path
                        .find(step)
                        .ok_or_else(|| format!("customer installation guide is missing {step}"))
                })
                .into_iter()
                .collect::<Result<Vec<_>, _>>()?;
            assert!(
                positions.windows(2).all(|pair| pair[0] < pair[1]),
                "v0.2 browser administration steps must remain in operator order"
            );
            for required in [
                "deployment capability catalog",
                "catalog is availability data",
                "exactly one active provisioned User Envelope",
            ] {
                assert!(
                    browser_path.contains(required),
                    "v0.2 browser administration is missing `{required}`"
                );
            }
            return Ok(());
        }

        let service_heading = "### Operator/service post-install administration";
        let browser_heading = "### Conditional human browser administration";
        let service_start = administration
            .find(service_heading)
            .ok_or_else(|| format!("customer installation guide is missing {service_heading}"))?;
        let browser_start = administration
            .find(browser_heading)
            .ok_or_else(|| format!("customer installation guide is missing {browser_heading}"))?;
        assert!(
            service_start < browser_start,
            "service-only administration must precede conditional human browser administration"
        );

        let service_path = &administration[service_start..browser_start];
        for required in [
            "POST /admin/service-envelopes/{service}",
            "separately authenticated operator/service identity",
            "scripts/bootstrap-task-copy-smoke.sh",
            "short-lived route-scoped identity",
        ] {
            assert!(
                service_path.contains(required),
                "service-only administration is missing {required}"
            );
        }

        let browser_path = &administration[browser_start..];
        for required in [
            "When `browserAuth.enabled=false`, skip this entire subsection",
            "There is no documented non-browser substitute",
            "steward-apiserver-bin bootstrap-rbac",
            "reads and verifies the existing Service Envelope",
            "does not provision or modify it",
        ] {
            assert!(
                browser_path.contains(required),
                "conditional browser administration is missing {required}"
            );
        }

        let ordered_steps = [
            "1. **Enable optional human browser administration.**",
            "2. **First login and canonical ID.**",
            "3. **Authorized local RBAC grant.**",
            "4. **Verify service authority and publish the browser catalog.**",
            "5. **User Envelope operation.**",
        ];
        let positions = ordered_steps
            .map(|step| {
                browser_path
                    .find(step)
                    .ok_or_else(|| format!("customer installation guide is missing {step}"))
            })
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?;
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "conditional browser administration steps must remain in operator order"
        );

        for browser_only in [
            "First login and canonical ID",
            "Verify service authority and publish the browser catalog",
            "User Envelope operation",
        ] {
            assert!(
                !service_path.contains(browser_only),
                "browser-only operation escaped its conditional group: {browser_only}"
            );
        }

        for forbidden in [
            "provision the required Service Envelope",
            "POST /admin/service-envelopes",
        ] {
            assert!(
                !browser_path.contains(forbidden),
                "browser administration must not claim Service Envelope authoring: {forbidden}"
            );
        }

        Ok(())
    }

    #[test]
    fn governed_connections_real_stack_is_a_required_pinned_ci_lane() -> Result<(), String> {
        let ci = fs::read_to_string(root().join(".github/workflows/ci.yml"))
            .map_err(|error| format!("Steward CI workflow is required: {error}"))?;
        let xtask_source = fs::read_to_string(root().join("xtask/src/main.rs"))
            .map_err(|error| format!("xtask source is required: {error}"))?;
        let outer = fs::read_to_string(root().join("scripts/governed-connections-e2e.sh"))
            .map_err(|error| format!("governed Connections outer harness is required: {error}"))?;
        let inner = fs::read_to_string(root().join("scripts/governed-connections-inside.sh"))
            .map_err(|error| format!("governed Connections inner harness is required: {error}"))?;
        let test = fs::read_to_string(root().join("e2e/governed_connections.rs"))
            .map_err(|error| format!("governed Connections E2E is required: {error}"))?;
        let stack = fs::read_to_string(root().join("config/connections-e2e/stack.yaml"))
            .map_err(|error| format!("governed Connections stack is required: {error}"))?;
        let sandbox = fs::read_to_string(root().join("e2e/Dockerfile.workflow-sandbox"))
            .map_err(|error| format!("pinned workflow sandbox is required: {error}"))?;

        for required in [
            "governed-connections:",
            "cargo xtask e2e-governed-connections",
            "- governed-connections",
            "GOVERNED_CONNECTIONS: ${{ needs.governed-connections.result }}",
            "supervisor-tools: \"true\"",
        ] {
            assert!(ci.contains(required), "pinned CI is missing {required}");
        }
        assert!(
            xtask_source.contains("\"e2e-governed-connections\" if rest.is_empty()")
                && xtask_source.contains("scripts/governed-connections-e2e.sh"),
            "xtask must expose the governed Connections real-stack harness"
        );
        for required in [
            "STEWARD_OPEN_SHELL_RELEASE=v0.0.98",
            "sha256:80bef7bee93482c8091335ae27c3c3e968e5c78c2bb4a40b401e6af36f70f993",
            "e2e/Dockerfile.workflow-sandbox",
            "scripts/build-patched-openshell-supervisor.sh",
            "STEWARD_OPENSHELL_SUPERVISOR_IMAGE",
            "STEWARD_OPENSHELL_SANDBOX_IMAGE",
            "scripts/openshell-testbed.sh",
        ] {
            assert!(
                outer.contains(required),
                "outer harness is missing {required}"
            );
        }
        for required in [
            "--test governed_connections",
            "STEWARD_CONNECTIONS_TEST_BRIDGE_DIGEST_IMAGE",
            "STEWARD_TEST_KUBE_CONTEXT",
            "ctr -n k8s.io images list",
            "$1 == image { print $3 }",
            "ctr -n k8s.io images tag \"${bridge_containerd_name}\" \"${bridge_digest_image}\"",
        ] {
            assert!(
                inner.contains(required),
                "inner harness is missing {required}"
            );
        }
        for required in [
            "real MCP-GW 0.4.9 OAuth state must have its pinned 600-second lifetime",
            "connection operations must remain structurally absent from generic run history",
            "harness.wait_runtime_phase(",
            "ALICE_RUNTIME,\n        \"Running\"",
        ] {
            assert!(
                test.contains(required),
                "real-stack test is missing {required}"
            );
        }
        assert!(
            stack.contains("steward-connections-e2e-mint-runtime")
                && stack.contains("steward-connections-e2e-mint-credentials"),
            "Mint runtime reads and namespace-scoped Secret reads must be separate"
        );
        assert!(
            sandbox.contains("ghcr.io/nvidia/openshell-community/sandboxes/base@sha256:")
                && sandbox.contains("@openai/codex@0.140.0")
                && sandbox.contains("codex-cli 0.140.0"),
            "the real-stack lane must install and verify the exact approved Workflow agent"
        );
        Ok(())
    }

    #[test]
    fn tls_required_postgres_is_a_ci_and_release_gate() -> Result<(), String> {
        let workspace = fs::read_to_string(root().join("Cargo.toml"))
            .map_err(|error| format!("workspace manifest is required: {error}"))?;
        let ci = fs::read_to_string(root().join(".github/workflows/ci.yml"))
            .map_err(|error| format!("Steward CI workflow is required: {error}"))?;
        let release = fs::read_to_string(root().join(".github/workflows/release.yml"))
            .map_err(|error| format!("Steward release workflow is required: {error}"))?;
        let harness = fs::read_to_string(root().join("scripts/postgres-tls-e2e.sh"))
            .map_err(|error| format!("PostgreSQL TLS integration harness is required: {error}"))?;

        assert!(
            workspace.contains("\"tls-rustls-ring-native-roots\""),
            "production SQLx must include a Rustls TLS provider"
        );
        for workflow in [&ci, &release] {
            assert!(
                workflow.contains("cargo xtask e2e-postgres-tls"),
                "CI and release validation must execute the TLS-required PostgreSQL lane"
            );
        }
        for required in [
            "sslmode=disable",
            "sslmode=require",
            "--test postgres_tls",
            "--test task_orchestration",
            "STEWARD_TEST_DATABASE_URL",
            "steward.test/run-id",
            "docker volume create",
            "docker volume rm",
            "chmod 600 /tls-output/server.key",
            "${TLS_VOLUME}:/tls-input:ro",
        ] {
            assert!(
                harness.contains(required),
                "PostgreSQL TLS integration harness is missing {required}"
            );
        }

        Ok(())
    }

    #[test]
    fn provider_profile_bundle_release_is_archived_attested_and_publicly_verified()
    -> Result<(), String> {
        let workflow = fs::read_to_string(root().join(".github/workflows/release.yml"))
            .map_err(|error| format!("published Steward release workflow is required: {error}"))?;
        let packaging = fs::read_to_string(
            root().join("scripts/package-provider-profile-bundle.sh"),
        )
        .map_err(|error| format!("provider-profile bundle packager is required: {error}"))?;
        let packaging_test =
            fs::read_to_string(root().join("scripts/test-package-provider-profile-bundle.sh"))
                .map_err(|error| {
                    format!("provider-profile bundle packaging test is required: {error}")
                })?;

        let publisher = workflow
            .split("  publish-provider-profile-bundle:")
            .nth(1)
            .and_then(|job| job.split("\n  openshell-x86-conformance:").next())
            .ok_or_else(|| "provider-profile bundle release job is required".to_owned())?;
        for required in [
            "needs: validate",
            "attestations: write",
            "scripts/package-provider-profile-bundle.sh",
            "actions/attest-build-provenance@",
            "subject-path: dist/steward-runtime-providers-${{ steps.version.outputs.version }}.tar.gz",
            "release-provider-profile-bundle",
            "provider-profile-bundle.digest",
        ] {
            assert!(
                publisher.contains(required),
                "provider-profile bundle release job must include {required}"
            );
        }
        for required in [
            "publish-provider-profile-bundle",
            "openshell-x86-conformance",
            "release / linux-amd64 OpenShell adapter",
            "Verify authenticated OpenShell adapter on linux/amd64",
            "cargo xtask e2e-openshell-adapter",
            "Provider profile bundle asset:",
            "Provider profile bundle identity: steward-runtime-providers@1.2.0",
            "Provider profile bundle SHA-256:",
            "Provider profile bundle signer identity:",
            "Provider profile bundle source repository:",
            "Provider profile bundle source commit:",
            "Provider profile bundle verification:",
            "Verify published provider-profile bundle availability",
            "gh release download",
            "gh attestation verify \"$destination/$asset\"",
            "--cert-identity \"$signer_identity\"",
            "provider-profile-bundle.digest",
        ] {
            assert!(
                workflow.contains(required),
                "public release handoff must include {required}"
            );
        }
        let create_release = workflow
            .find("gh release create \"$GITHUB_REF_NAME\"")
            .ok_or_else(|| "GitHub release creation must remain explicit".to_owned())?;
        let verify_release = workflow
            .find("Verify published provider-profile bundle availability")
            .ok_or_else(|| {
                "published provider-profile bundle verification is required".to_owned()
            })?;
        assert!(
            verify_release > create_release,
            "bundle availability must be verified after the GitHub Release publishes its asset"
        );
        let release = workflow
            .split("  release:")
            .nth(1)
            .ok_or_else(|| "GitHub Release job is required".to_owned())?;
        for required in [
            "openshell-x86-conformance",
            "needs.openshell-x86-conformance.result == 'success'",
        ] {
            assert!(
                release.contains(required),
                "GitHub Release must fail closed without {required}"
            );
        }
        for required in [
            "--sort=name",
            "--mtime=@0",
            "--owner=0",
            "--group=0",
            "--numeric-owner",
            "--format=ustar",
            "gzip -n",
            "provider-profile-bundle/v1.2.0/bundle.json",
            "provider-profile-bundle/v1.2.0/profiles/steward-litellm.json",
            "provider-profile-bundle/v1.2.0/profiles/steward-mcp-gw.json",
            "sha256sum",
        ] {
            assert!(
                packaging.contains(required),
                "provider-profile bundle packager must retain {required}"
            );
        }
        for required in [
            "deterministic provider-profile bundle archive",
            "archive bytes must be reproducible",
            "unexpected archive entry",
            "digest must bind the archive bytes",
        ] {
            assert!(
                packaging_test.contains(required),
                "provider-profile bundle packaging test must prove {required}"
            );
        }
        let bundle_readme =
            fs::read_to_string(root().join("config/provider-profile-bundle/v1.2.0/README.md"))
                .map_err(|error| format!("provider-profile bundle README is required: {error}"))?;
        for required in [
            "Verify a released bundle before rendering or installation",
            "gh attestation verify",
            "--cert-identity",
            "source repository and",
            "exact source commit",
            "test \"$actual_digest\" = \"$expected_digest\"",
            "source-only validation",
            "eligible for release",
        ] {
            assert!(
                bundle_readme.contains(required),
                "provider-profile bundle consumer verification instructions must include {required}"
            );
        }
        Ok(())
    }

    #[test]
    fn v020_release_handoff_is_machine_readable_attested_and_verified() -> Result<(), String> {
        let workflow = fs::read_to_string(root().join(".github/workflows/release.yml"))
            .map_err(|error| format!("published Steward release workflow is required: {error}"))?;
        let release = workflow
            .split("  release:")
            .nth(1)
            .ok_or_else(|| "GitHub Release job is required".to_owned())?;
        for required in [
            "id-token: write",
            "attestations: write",
            "steward.release-handoff/v1",
            "authorityContract: \"user-envelope-only\"",
            "identityPolicyContract: \"github-oidc-exchange.apelogic.io/v5\"",
            "0039_user_envelope_only_task_authority.sql",
            "serviceEnvelopeSupported: false",
            "subject-path: dist/release-handoff.json",
            "release-handoff-attestation.jsonl",
            "gh attestation verify dist/release-handoff.json",
            "Verify published release handoff",
            "cmp dist/release-handoff.json",
        ] {
            assert!(
                release.contains(required),
                "v0.2 release handoff is missing `{required}`"
            );
        }
        for component in ["apiserver", "controller", "mint", "bridge", "web"] {
            assert!(
                release.contains(&format!("{component}: {{reference:")),
                "machine-readable handoff must include {component}"
            );
        }
        let manifest = release
            .find("> dist/release-handoff.json")
            .ok_or_else(|| "machine-readable handoff generation is required".to_owned())?;
        let attestation = release
            .find("subject-path: dist/release-handoff.json")
            .ok_or_else(|| "machine-readable handoff attestation is required".to_owned())?;
        let publication = release
            .find("gh release create \"$GITHUB_REF_NAME\"")
            .ok_or_else(|| "GitHub release creation must remain explicit".to_owned())?;
        let public_verification = release
            .find("Verify published release handoff")
            .ok_or_else(|| "published handoff verification is required".to_owned())?;
        assert!(
            manifest < attestation
                && attestation < publication
                && publication < public_verification,
            "release handoff must be generated, attested, published, and then publicly verified"
        );
        Ok(())
    }

    #[test]
    fn next_web_release_is_immutable_same_origin_and_least_privileged() -> Result<(), String> {
        let chart = root().join("charts/steward");
        let values = fs::read_to_string(chart.join("values.yaml"))
            .map_err(|error| format!("published Steward chart values are required: {error}"))?;
        let schema = fs::read_to_string(chart.join("values.schema.json"))
            .map_err(|error| format!("published Steward values schema is required: {error}"))?;
        let templates = fs::read_to_string(chart.join("templates/all.yaml")).map_err(|error| {
            format!("published Steward Kubernetes templates are required: {error}")
        })?;
        let workflow = fs::read_to_string(root().join(".github/workflows/release.yml"))
            .map_err(|error| format!("published release workflow is required: {error}"))?;
        let container = fs::read_to_string(root().join("build/web.Dockerfile"))
            .map_err(|error| format!("production web container is required: {error}"))?;
        let release_validation = fs::read_to_string(
            root().join("scripts/validate-release-artifacts.sh"),
        )
        .map_err(|error| format!("release artifact validation script is required: {error}"))?;

        for required in ["web:", "ingress:", "host:", "tlsSecretName:"] {
            assert!(
                values.contains(required),
                "web chart values are missing {required}"
            );
            assert!(
                schema.contains(required.trim_end_matches(':')),
                "web values schema is missing {required}"
            );
        }
        for required in [
            "app.kubernetes.io/component: web",
            "automountServiceAccountToken: false",
            "readOnlyRootFilesystem: true",
            "runAsNonRoot: true",
            "path: /health/ready",
            "kind: Ingress",
            "path: /admin/api",
            "path: /admin/auth",
            "path: /admin/connections/github/callback",
            "path: /app/api",
            "name: steward-apiserver",
            "name: steward-web",
            "metadata: { name: steward-web-egress }",
            "egress: []",
        ] {
            assert!(
                templates.contains(required),
                "web deployment contract is missing {required}"
            );
        }
        for forbidden in [
            ".Values.secrets.database",
            ".Values.secrets.mint",
            ".Values.secrets.jira",
            ".Values.secrets.litellm",
            "serviceAccountToken:",
        ] {
            let web_deployment = templates
                .split("metadata: { name: steward-web }")
                .nth(1)
                .and_then(|tail| tail.split("---").next())
                .ok_or_else(|| "web Deployment template is missing".to_owned())?;
            assert!(
                !web_deployment.contains(forbidden),
                "web deployment must not contain {forbidden}"
            );
        }
        for required in [
            "bun install --frozen-lockfile",
            "bun run --cwd web build",
            "NEXT_TELEMETRY_DISABLED=1",
            "/workspace/web/.next/standalone",
            "/workspace/web/.next/static",
            "USER 65532:65532",
        ] {
            assert!(
                container.contains(required),
                "web image is missing {required}"
            );
        }
        assert_eq!(
            container.matches("FROM ").count(),
            3,
            "web image needs pinned Bun, build, and runtime stages"
        );
        assert_eq!(
            container.matches("@sha256:").count(),
            3,
            "web base images must be digest pinned"
        );
        assert!(
            workflow.contains("component: web"),
            "release matrix must publish the web image"
        );
        assert!(
            workflow.contains("dockerfile: build/web.Dockerfile"),
            "release matrix must select the web Dockerfile"
        );
        for required in [
            "docker run --rm --detach",
            "--read-only",
            "docker inspect --format",
            "65532:65532 true",
            "--retry-all-errors",
            "/health/ready",
            "web release image did not become ready",
        ] {
            assert!(
                release_validation.contains(required),
                "release validation must run the built web image and prove {required}"
            );
        }
        Ok(())
    }

    #[test]
    fn repository_scans_do_not_descend_into_generated_or_dependency_trees() {
        assert!(should_skip_directory(Path::new("/workspace/node_modules")));
        assert!(should_skip_directory(Path::new("/workspace/web/.next")));
    }

    #[test]
    fn next_browser_journey_uses_only_neutral_test_identifiers() -> Result<(), String> {
        let content = fs::read_to_string(root().join("tests/browser/steward-next.spec.mjs"))
            .map_err(|error| format!("Next browser journey is required: {error}"))?;
        let violations = xtask::neutrality_violations(&content);
        assert!(violations.is_empty(), "{violations:?}");
        Ok(())
    }

    #[test]
    fn production_release_contract_is_complete_and_fail_closed() -> Result<(), String> {
        let chart = root().join("charts/steward");
        let values = fs::read_to_string(chart.join("values.yaml"))
            .map_err(|error| format!("published Steward chart values are required: {error}"))?;
        let schema = fs::read_to_string(chart.join("values.schema.json"))
            .map_err(|error| format!("published Steward values schema is required: {error}"))?;
        serde_json::from_str::<serde_json::Value>(&schema)
            .map_err(|error| format!("published Steward values schema is invalid JSON: {error}"))?;
        let templates = fs::read_to_string(chart.join("templates/all.yaml")).map_err(|error| {
            format!("published Steward Kubernetes templates are required: {error}")
        })?;
        let crd = fs::read_to_string(chart.join("crds/agentruntimes.yaml"))
            .map_err(|error| format!("published Steward CRD is required: {error}"))?;
        let generated_crd =
            fs::read_to_string(root().join("manifests/agents.apelogic.ai_agentruntimes.yaml"))
                .map_err(|error| format!("failed to read generated AgentRuntime CRD: {error}"))?;
        let workflow = fs::read_to_string(root().join(".github/workflows/release.yml"))
            .map_err(|error| format!("published Steward release workflow is required: {error}"))?;
        let ci = fs::read_to_string(root().join(".github/workflows/ci.yml"))
            .map_err(|error| format!("Steward CI workflow is required: {error}"))?;
        let release_validation = fs::read_to_string(
            root().join("scripts/validate-release-artifacts.sh"),
        )
        .map_err(|error| format!("release artifact validation script is required: {error}"))?;
        let setup_tools = fs::read_to_string(root().join(".github/actions/setup-tools/action.yml"))
            .map_err(|error| format!("Steward CI tool installer is required: {error}"))?;
        let provider_profiles = [
            "config/s1/provider-profile.yaml",
            "config/s5/tool-provider-profile.yaml",
        ]
        .map(|path| {
            fs::read_to_string(root().join(path))
                .map_err(|error| format!("pinned provider profile {path} is required: {error}"))
        })
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;
        let container = fs::read_to_string(root().join("build/package.Dockerfile"))
            .map_err(|error| format!("production container build is required: {error}"))?;
        let bridge_container = fs::read_to_string(
            root().join("build/connections-bridge.Dockerfile"),
        )
        .map_err(|error| format!("Connections bridge sandbox image build is required: {error}"))?;
        let controller_binary =
            fs::read_to_string(root().join("bins/steward-controller/src/main.rs"))
                .map_err(|error| format!("Steward controller source is required: {error}"))?;

        for required in [
            "apiserver:",
            "controller:",
            "mint:",
            "browserAuth:",
            "digest:",
            "pullPolicy:",
        ] {
            assert!(
                values.contains(required),
                "chart values are missing {required}"
            );
            assert!(
                schema.contains(required.trim_end_matches(':')),
                "values schema is missing {required}"
            );
        }
        for required in [
            "kind: ServiceAccount",
            "name: {{ include \"steward.apiserverName\" . }}",
            "name: {{ include \"steward.controllerName\" . }}",
            "name: {{ include \"steward.mintName\" . }}",
            "failurePolicy: Fail",
            "kind: Certificate",
            ".Values.spire.csiDriver",
            "kind: NetworkPolicy",
            "kind: ClusterSPIFFEID",
            ".Values.runtimeNamespaces",
            "kind: RoleBinding",
            "STEWARD_GOOGLE_OIDC_CLIENT_ID",
            "STEWARD_GOOGLE_OIDC_CLIENT_SECRET",
            ".Values.browserAuth.enabled",
        ] {
            assert!(
                templates.contains(required),
                "chart templates are missing {required}"
            );
        }
        assert!(
            values.contains("csiDriver: csi.spiffe.io"),
            "the SPIRE CSI driver must be enabled by default"
        );
        assert!(
            crd.contains("kind: CustomResourceDefinition"),
            "the published chart must install the AgentRuntime CRD"
        );
        assert_eq!(
            crd, generated_crd,
            "the chart CRD must be byte-identical to the generated manifest"
        );
        for binary in [
            "bins/steward-apiserver/src/main.rs",
            "bins/steward-controller/src/main.rs",
        ] {
            let source = fs::read_to_string(root().join(binary))
                .map_err(|error| format!("failed to inspect {binary}: {error}"))?;
            assert!(
                source.contains("PemObject"),
                "{binary} must accept cert-manager's PEM certificate and key files"
            );
        }
        assert!(
            templates.contains("{{- if .Values.web.enabled }}")
                && templates.contains("kind: Ingress"),
            "the same-origin Ingress must remain opt-in with the web workload"
        );
        let global_roles = templates
            .split("kind: ClusterRoleBinding")
            .next()
            .ok_or_else(|| "global ClusterRoles are missing".to_owned())?;
        assert!(
            !global_roles.contains("resources: [\"secrets\"]"),
            "globally bound ClusterRoles must never grant Secret access"
        );
        assert!(
            values.contains("runtimeNamespaces: []"),
            "runtime Secret access must default to no authorized namespaces"
        );
        assert!(
            templates.contains("spiffeIDTemplate: spiffe://{{ .Values.config.mint.spiffeTrustDomain }}{{ .Values.spire.identityPath }}"),
            "Mint ClusterSPIFFEID must bind the configured trust domain and identity path"
        );
        assert!(
            values.contains("identityPath: /steward/mint"),
            "Mint must use the stable /steward/mint SPIFFE identity by default"
        );
        for provider_value in ["audience: steward-mcp", "allowedScopes: mcp inference"] {
            assert!(
                values.contains(provider_value),
                "chart Mint defaults must match the tested provider contract: missing {provider_value}"
            );
        }
        for provider_profile in provider_profiles {
            for provider_value in ["audience: steward-mcp", "scopes: [mcp]"] {
                assert!(
                    provider_profile.contains(provider_value),
                    "pinned provider profile is missing {provider_value}"
                );
            }
        }
        let apiserver = templates
            .split("kind: Deployment")
            .nth(1)
            .ok_or_else(|| "apiserver Deployment template is missing".to_owned())?;
        let controller = templates
            .split("kind: Deployment")
            .nth(2)
            .ok_or_else(|| "controller Deployment template is missing".to_owned())?;
        let mint = templates
            .split("kind: Deployment")
            .nth(3)
            .ok_or_else(|| "mint Deployment template is missing".to_owned())?;
        assert!(!apiserver.contains(".Values.secrets.mint"));
        assert!(!apiserver.contains(".Values.secrets.litellm"));
        assert!(!controller.contains(".Values.secrets.mint"));
        assert!(
            controller.contains("STEWARD_JIRA_TOKEN")
                && controller.contains("STEWARD_JIRA_BASE_URL")
                && controller.contains("STEWARD_JIRA_PROJECT_KEY")
                && controller.contains("STEWARD_JIRA_ACCOUNT_EMAIL")
                && controller.contains(".Values.secrets.jira"),
            "the Task approval outbox dispatcher must receive only the Jira decision-channel configuration"
        );
        assert!(
            apiserver.contains("STEWARD_TASK_INFERENCE_ENDPOINT"),
            "the Codex execution adapter must receive its deployment-owned inference endpoint"
        );
        assert!(!mint.contains(".Values.secrets.database"));
        assert!(!mint.contains(".Values.secrets.jira"));
        assert!(!mint.contains(".Values.secrets.litellm"));
        for artifact in [
            "apiserver.digest",
            "controller.digest",
            "mint.digest",
            "bridge.digest",
            "bridge-attestation-bundle.jsonl",
            "gh attestation download",
            "--predicate-type https://slsa.dev/provenance/v1",
            "Bridge signer identity:",
            "Bridge source repository:",
            "Bridge source commit:",
            "helm-chart.digest",
        ] {
            assert!(
                workflow.contains(artifact),
                "release workflow must record {artifact}"
            );
        }
        assert!(
            values.contains("connectionsBridge:")
                && values.contains("artifactTrust:")
                && values.contains("mode: github-attestation")
                && values.contains("mcpGatewayOrigin")
                && values.contains("mcpGatewayVersion")
                && schema.contains("connectionsBridge"),
            "governed Connections chart values must require the controller-owned MCP-GW origin"
        );
        assert!(
            controller.contains("connections-bridge-attestation"),
            "Connections bridge controller must mount the immutable provenance bundle"
        );
        for trust_contract in [
            "github-attestation",
            "operator-pinned",
            "STEWARD_CONNECTIONS_BRIDGE_ARTIFACT_TRUST_MODE",
        ] {
            assert!(
                controller.contains(trust_contract)
                    || schema.contains(trust_contract)
                    || values.contains(trust_contract),
                "Connections bridge artifact-trust contract is missing {trust_contract}"
            );
        }
        assert!(
            controller.contains("STEWARD_CONNECTIONS_MCP_GW_ORIGIN")
                && controller.contains("STEWARD_CONNECTIONS_MCP_GW_VERSION"),
            "Connections bridge controller must receive the server-owned MCP-GW origin and pinned version"
        );
        for isolated_bridge_configuration in [
            "STEWARD_STABLE_BRIDGE_IMAGE",
            "STEWARD_STABLE_BRIDGE_MCP_GW_ORIGIN",
            "STEWARD_CONNECTIONS_BRIDGE_IMAGE",
            "STEWARD_CONNECTIONS_MCP_GW_ORIGIN",
        ] {
            assert!(
                controller_binary.contains(isolated_bridge_configuration),
                "controller must preserve the independent stable and governed Connections bridge configuration: missing {isolated_bridge_configuration}"
            );
        }
        assert!(
            workflow.matches("exit-code: \"1\"").count() >= 2,
            "image and chart vulnerability scans must fail releases on critical findings"
        );
        assert_eq!(
            container.matches("FROM ").count(),
            2,
            "production images must use a build stage and a minimal runtime stage"
        );
        assert_eq!(
            container.matches("@sha256:").count(),
            2,
            "every production image base must be pinned by digest"
        );
        assert!(
            container.contains("USER 65532:65532"),
            "production images must run as a numeric non-root user"
        );
        for required in [
            "FROM ubuntu:24.04@sha256:",
            "apt-get install --yes --no-install-recommends iproute2",
            "rm -rf /var/lib/apt/lists/*",
            "test -x /bin/cat",
            "test -x /bin/find",
            "test -x /bin/id",
            "test -x /bin/ip",
            "test -x /bin/mkdir",
            "test -x /bin/mktemp",
            "test -x /bin/rm",
            "test -x /bin/sh",
            "test -x /bin/sleep",
            "test -x /bin/tar",
            "test -x /bin/touch",
            "mkdir -p /sandbox",
            "chown 65532:65532 /sandbox",
            "USER 65532:65532",
            "/usr/local/bin/steward-connections-bridge",
        ] {
            assert!(
                bridge_container.contains(required),
                "Connections bridge sandbox image is missing required OpenShell runtime prerequisite: {required}"
            );
        }
        assert!(
            workflow.contains("${{ steps.version.outputs.version }}-${{ matrix.component }}"),
            "component tags must match the published chart contract"
        );
        assert!(
            workflow.contains("platforms: linux/amd64"),
            "release images must publish the supported linux/amd64 runtime platform explicitly"
        );
        assert!(
            workflow.contains("push:\n    tags:"),
            "release must run only from a version tag"
        );
        for required in [
            "release-candidate:",
            "scripts/validate-release-artifacts.sh --build-images",
            "actionlint",
            "shellcheck",
        ] {
            assert!(
                ci.contains(required),
                "pull-request CI must validate release artifacts: missing {required}"
            );
        }
        for required in [
            "workflow-tools:",
            "actionlint-version:",
            "shellcheck-version:",
            "sha256sum --check",
        ] {
            assert!(
                setup_tools.contains(required),
                "pinned CI workflow tools are missing {required}"
            );
        }
        for required in [
            "helm template steward",
            "docker build",
            "docker run --rm --entrypoint /bin/sh",
            "command -v tar",
            "command -v ip",
            "test -w /sandbox",
            "test \"$(id -u)\" = \"65532\"",
        ] {
            assert!(
                release_validation.contains(required),
                "release validation must exercise artifact construction: missing {required}"
            );
        }
        for required in [
            "steward-mint:release-validation",
            "--network none",
            "KUBECONFIG=/run/steward-release/kubeconfig",
            "Could not automatically determine the process-level CryptoProvider",
            "OpenShell identity discovery failed",
        ] {
            assert!(
                release_validation.contains(required),
                "release validation must smoke-test the combined mint image startup: missing {required}"
            );
        }
        Ok(())
    }

    struct TestRepository {
        path: PathBuf,
    }

    impl TestRepository {
        fn create() -> Result<Self, String> {
            for _ in 0..1_024 {
                let nonce = NEXT_REPOSITORY_ID.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir()
                    .join(format!("steward-migration-{}-{nonce}", std::process::id()));
                match fs::create_dir(&path) {
                    Ok(()) => return Ok(Self { path }),
                    Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                    Err(error) => {
                        return Err(format!("failed to create {}: {error}", path.display()));
                    }
                }
            }

            Err("failed to allocate a unique migration test repository".to_owned())
        }
    }

    impl Drop for TestRepository {
        fn drop(&mut self) {
            if let Err(error) = fs::remove_dir_all(&self.path)
                && error.kind() != ErrorKind::NotFound
            {
                eprintln!(
                    "warning: failed to remove test repository {}: {error}",
                    self.path.display()
                );
            }
        }
    }

    fn test_git_command(repository: &Path, arguments: &[&str]) -> Command {
        let mut command = git_command_in_repository(repository);
        command
            .args(["-c", "commit.gpgsign=false"])
            .args(arguments)
            .env("GIT_CONFIG_GLOBAL", repository.join(".gitconfig-disabled"))
            .env("GIT_CONFIG_NOSYSTEM", "1");
        command
    }

    fn git(repository: &Path, arguments: &[&str]) -> Result<(), String> {
        let output = test_git_command(repository, arguments)
            .output()
            .map_err(|error| format!("failed to run git {}: {error}", arguments.join(" ")))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "git {} failed: {}",
                arguments.join(" "),
                String::from_utf8_lossy(&output.stderr)
            ))
        }
    }

    #[test]
    fn migration_test_git_isolates_signing_configuration() -> Result<(), String> {
        let repository = TestRepository::create()?;
        let command = test_git_command(&repository.path, &["commit"]);
        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let expected_global = repository.path.join(".gitconfig-disabled");
        let global_config = command
            .get_envs()
            .find(|(key, _value)| *key == "GIT_CONFIG_GLOBAL")
            .and_then(|(_key, value)| value);
        let no_system_config = command
            .get_envs()
            .find(|(key, _value)| *key == "GIT_CONFIG_NOSYSTEM")
            .and_then(|(_key, value)| value);

        assert_eq!(
            arguments,
            ["-c", "commit.gpgsign=false", "commit"],
            "fixture commits must disable signing explicitly"
        );
        assert_eq!(
            global_config,
            Some(expected_global.as_os_str()),
            "fixture Git commands must not inherit global configuration"
        );
        assert_eq!(
            no_system_config,
            Some(std::ffi::OsStr::new("1")),
            "fixture Git commands must not inherit system configuration"
        );
        for variable in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_QUARANTINE_PATH",
            "GIT_NAMESPACE",
            "GIT_PREFIX",
        ] {
            let value = command
                .get_envs()
                .find(|(key, _value)| *key == variable)
                .map(|(_key, value)| value);
            assert_eq!(
                value,
                Some(None),
                "fixture Git commands must clear inherited {variable}"
            );
        }
        Ok(())
    }

    #[test]
    fn conformance_requires_exactly_one_executed_test() {
        let rust_green =
            "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out";
        let green = format!("G-2 upstream result: 1 passed; 0 failed; 0 skipped\n{rust_green}");
        assert!(
            validate_conformance_test_result(&green).is_ok(),
            "one executed upstream negative test and one Rust wrapper must be accepted as evidence"
        );
        assert!(
            validate_conformance_test_result(rust_green).is_err(),
            "a passing Rust wrapper without an executed Bun test must not count as evidence"
        );
        assert!(
            validate_conformance_test_result("G-2 upstream result: 1 passed; 0 failed; 0 skipped")
                .is_err(),
            "an upstream sentinel without its Rust wrapper must not count as evidence"
        );
        let duplicate = format!(
            "G-2 upstream result: 1 passed; 0 failed; 0 skipped\nG-2 upstream result: 1 passed; 0 failed; 0 skipped\n{rust_green}"
        );
        assert!(
            validate_conformance_test_result(&duplicate).is_err(),
            "duplicate upstream summaries must not count as exact evidence"
        );

        for invalid in [
            "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 1 filtered out",
            "test result: ok. 0 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out",
            "test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out",
        ] {
            let output = format!("G-2 upstream result: 1 passed; 0 failed; 0 skipped\n{invalid}");
            assert!(
                validate_conformance_test_result(&output).is_err(),
                "zero, ignored, filtered, or duplicate tests must not count as evidence: {invalid}"
            );
        }
    }

    #[test]
    fn poc_api_projects_only_its_tls_material() -> Result<(), String> {
        let manifest_path = root().join("config/poc/api-stack.yaml");
        let content = fs::read_to_string(&manifest_path)
            .map_err(|error| format!("failed to read {}: {error}", manifest_path.display()))?;
        let api_deployment = content
            .split("\n---\n")
            .map(serde_saphyr::from_str::<serde_json::Value>)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("failed to parse {}: {error}", manifest_path.display()))?
            .into_iter()
            .find(|document| {
                document
                    .pointer("/kind")
                    .and_then(serde_json::Value::as_str)
                    == Some("Deployment")
                    && document
                        .pointer("/metadata/name")
                        .and_then(serde_json::Value::as_str)
                        == Some("steward-poc-api")
            })
            .ok_or_else(|| "PoC API Deployment is missing".to_owned())?;
        let projected_keys = api_deployment
            .pointer("/spec/template/spec/volumes")
            .and_then(serde_json::Value::as_array)
            .and_then(|volumes| {
                volumes.iter().find(|volume| {
                    volume.get("name").and_then(serde_json::Value::as_str) == Some("secrets")
                })
            })
            .and_then(|volume| volume.pointer("/secret/items"))
            .and_then(serde_json::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        item.get("key")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned)
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        assert_eq!(
            projected_keys,
            ["tls-cert.der", "tls-key.der"],
            "the API pod must not receive the mint signing key, LiteLLM master key, or introspection credential"
        );
        Ok(())
    }

    #[test]
    fn g1_github_api_probe_uses_a_non_secret_user_agent() -> Result<(), String> {
        let script_path = root().join("scripts/g1-upstream-conformance-inside.sh");
        let script = fs::read_to_string(&script_path)
            .map_err(|error| format!("failed to read {}: {error}", script_path.display()))?;
        assert!(
            script.contains("-H 'User-Agent: steward-conformance/1.0' https://api.github.com/zen"),
            "the public G-1 GitHub API probe must identify itself without adding a credential"
        );
        Ok(())
    }

    #[test]
    fn g1_requires_the_explicit_default_deny_response() -> Result<(), String> {
        let script_path = root().join("scripts/g1-upstream-conformance-inside.sh");
        let script = fs::read_to_string(&script_path)
            .map_err(|error| format!("failed to read {}: {error}", script_path.display()))?;
        assert!(
            script.contains(
                "curl -sS --max-time 20 -H 'User-Agent: steward-conformance/1.0' https://api.github.com/zen"
            ),
            "the allowed probe must treat an upstream HTTP response as reachable without curl --fail"
        );
        assert!(
            script.contains(
                "curl -sS --max-time 10 --output /dev/null --write-out '%{http_connect}' https://docs.rs"
            ),
            "the forbidden HTTPS probe must retain the proxy CONNECT status for an explicit denial assertion"
        );
        assert!(
            script.contains(
                "if [[ \"${denied_exit}\" -ne 56 || \"${denied_connect_status}\" != \"403\" ]]; then"
            ),
            "G-1 must pass only on curl's failed CONNECT exit paired with OpenShell's explicit 403 denial"
        );
        Ok(())
    }

    #[test]
    fn migration_diff_accepts_a_divergent_base() -> Result<(), String> {
        let repository = TestRepository::create()?;
        git(&repository.path, &["init", "--initial-branch=main"])?;
        git(
            &repository.path,
            &["config", "user.email", "alice@example.com"],
        )?;
        git(&repository.path, &["config", "user.name", "alice"])?;
        fs::write(repository.path.join("README.md"), "base\n")
            .map_err(|error| format!("failed to write base fixture: {error}"))?;
        git(&repository.path, &["add", "README.md"])?;
        git(&repository.path, &["commit", "-m", "base"])?;

        git(&repository.path, &["switch", "-c", "feature"])?;
        fs::create_dir(repository.path.join("migrations"))
            .map_err(|error| format!("failed to create migration fixture directory: {error}"))?;
        fs::write(
            repository.path.join("migrations/0001_feature.sql"),
            "select 1;\n",
        )
        .map_err(|error| format!("failed to write migration fixture: {error}"))?;
        git(&repository.path, &["add", "migrations/0001_feature.sql"])?;
        git(&repository.path, &["commit", "-m", "feature"])?;

        git(&repository.path, &["switch", "main"])?;
        fs::write(repository.path.join("main.txt"), "advanced\n")
            .map_err(|error| format!("failed to write advanced-main fixture: {error}"))?;
        git(&repository.path, &["add", "main.txt"])?;
        git(&repository.path, &["commit", "-m", "advance main"])?;
        git(&repository.path, &["switch", "feature"])?;

        let changes = migration_changes(&repository.path, "main")?;

        assert!(
            changes.contains("migrations/0001_feature.sql"),
            "three-dot comparison must include the feature migration: {changes}"
        );
        Ok(())
    }
}
