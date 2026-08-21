use std::collections::BTreeSet;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use steward_adapter_fake::IMPLEMENTED_PORTS as FAKE_PORTS;
use steward_ports::{Maturity, PORTS};
use steward_types::agent_runtime_crd;
use xtask::{
    local_test_context_is_safe, migration_base_candidates, migration_history_violations,
    neutrality_violations, secret_violations, select_migration_base, validate_register_content,
};

type TaskResult = Result<(), String>;

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
        "e2e-s0" if rest.is_empty() => e2e_s0(),
        "e2e-s1" if rest.is_empty() => e2e_s1(),
        "e2e-s2" if rest.is_empty() => e2e_s2(),
        "e2e-s3" if rest.is_empty() => e2e_s3(),
        "e2e-s4" if rest.is_empty() => e2e_s4(),
        "e2e-s5" if rest.is_empty() => e2e_s5(),
        "e2e-task" if rest.is_empty() => e2e_task(),
        "e2e-controller-runtime-lifecycle" if rest.is_empty() => e2e_controller_runtime_lifecycle(),
        "e2e-openshell-adapter" if rest.is_empty() => e2e_openshell_adapter(),
        "e2e-postgres-tls" if rest.is_empty() => e2e_postgres_tls(),
        "browser-e2e" if rest.is_empty() => browser_e2e(false),
        "browser-e2e" if rest == ["--browser-ready"] => browser_e2e(true),
        "policy-test" if rest.is_empty() => policy_test(),
        "migrate-check" if rest.is_empty() => migrate_check(),
        "generate-manifests" if rest.is_empty() => generate_manifests(),
        "verify-manifests" if rest.is_empty() => verify_manifests(),
        "check-neutrality" if rest.is_empty() => check_neutrality(),
        "check-secrets" if rest.is_empty() => check_secrets(),
        "conformance" => conformance(rest),
        "register" => register(rest),
        "ports" if rest == ["--check"] => ports_check(),
        "layering-test" if rest.is_empty() => layering_test(),
        "dev" => dev(rest),
        "reap" if rest.is_empty() => Err(
            "reaping is introduced with the ephemeral S0.0 harness; no resources exist in S-1"
                .to_owned(),
        ),
        _ => Err(usage()),
    }
}

fn usage() -> String {
    [
        "usage: cargo xtask <command>",
        "commands:",
        "  ci",
        "  quality",
        "  e2e-s0",
        "  e2e-s1",
        "  e2e-s2",
        "  e2e-s3",
        "  e2e-s4",
        "  e2e-s5",
        "  e2e-task",
        "  e2e-controller-runtime-lifecycle",
        "  e2e-openshell-adapter",
        "  e2e-postgres-tls",
        "  browser-e2e [--browser-ready]",
        "  policy-test",
        "  migrate-check",
        "  generate-manifests",
        "  verify-manifests",
        "  check-neutrality",
        "  check-secrets",
        "  conformance --pinned|--latest",
        "  register --check",
        "  ports --check",
        "  layering-test",
        "  dev doctor|up|down",
        "  reap",
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
    check_neutrality()?;
    check_secrets()?;
    register(&["--check".to_owned()])?;
    ports_check()?;
    layering_test()
}

fn e2e_s0() -> TaskResult {
    run("bash", &["scripts/s0-0-openshell-spike.sh", "--s0-e2e"])
}

fn e2e_s1() -> TaskResult {
    run("bash", &["scripts/s1-identity-e2e.sh"])
}

fn e2e_s2() -> TaskResult {
    run("bash", &["scripts/s2-inference-e2e.sh"])
}

fn e2e_s3() -> TaskResult {
    run("bash", &["scripts/s3-envelope-e2e.sh"])
}

fn e2e_s4() -> TaskResult {
    run("bash", &["scripts/s4-escalation-e2e.sh"])
}

fn e2e_s5() -> TaskResult {
    run("bash", &["scripts/s5-revocation-e2e.sh"])
}

fn e2e_task() -> TaskResult {
    run("bash", &["scripts/task-submission-e2e.sh"])
}

fn e2e_controller_runtime_lifecycle() -> TaskResult {
    run("bash", &["scripts/task-submission-e2e.sh"])
}

fn e2e_openshell_adapter() -> TaskResult {
    run("bash", &["scripts/openshell-adapter-e2e.sh"])
}

fn e2e_postgres_tls() -> TaskResult {
    run("bash", &["scripts/postgres-tls-e2e.sh"])
}

fn browser_e2e(browser_ready: bool) -> TaskResult {
    let browser_e2e_directory = root().join("target/browser-e2e");
    let cache = browser_e2e_directory.join("npm-cache");
    let browsers = browser_e2e_directory.join("browsers");
    fs::create_dir_all(&cache)
        .map_err(|error| format!("failed to create browser E2E npm cache: {error}"))?;
    fs::create_dir_all(&browsers)
        .map_err(|error| format!("failed to create browser E2E browser directory: {error}"))?;
    let npm_environment = [
        ("npm_config_cache", cache.as_os_str()),
        ("PLAYWRIGHT_BROWSERS_PATH", browsers.as_os_str()),
    ];
    if !browser_ready {
        run_with_env("npm", &["ci"], &npm_environment)?;
        run_with_env(
            "npm",
            &["exec", "playwright", "install", "chromium"],
            &npm_environment,
        )?;
    }
    run(
        "cargo",
        &[
            "build",
            "-p",
            "steward-apiserver",
            "--locked",
            "--features",
            "admin-demo",
            "--examples",
        ],
    )?;
    run_with_env("npm", &["run", "test:browser-e2e"], &npm_environment)
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
        return Err("S-1 supports `register --check`; rendering arrives in S0.0".to_owned());
    }
    validate_register()?;
    println!("register --check: declarative register shape is valid; evidence arrives in S0.0");
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
        return Err("dev requires doctor, up, or down".to_owned());
    };
    if arguments.len() != 1 {
        return Err("dev accepts exactly one operation in S-1".to_owned());
    }
    match operation {
        "doctor" => dev_doctor(),
        "up" | "down" => {
            require_local_test_context()?;
            Err(format!(
                "dev {operation} is introduced with the ephemeral S0.0 harness"
            ))
        }
        _ => Err("dev requires doctor, up, or down".to_owned()),
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

fn require_local_test_context() -> TaskResult {
    let context = env::var("STEWARD_TEST_KUBE_CONTEXT").map_err(|_| {
        "STEWARD_TEST_KUBE_CONTEXT must explicitly select an ephemeral local context".to_owned()
    })?;
    validate_local_test_context(&context)
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
        Some(".git" | "target" | ".steward-run" | ".worktrees")
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
        git_command_in_repository, migration_changes, root, validate_conformance_test_result,
    };
    use std::fs;
    use std::io::ErrorKind;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_REPOSITORY_ID: AtomicU64 = AtomicU64::new(0);

    fn ci_job(workflow: &str, job_name: &str) -> Result<String, String> {
        let header = format!("\n  {job_name}:");
        let (_, job) = workflow
            .split_once(&header)
            .ok_or_else(|| format!("{job_name} CI job is required"))?;

        Ok(job
            .lines()
            .take_while(|line| {
                line.is_empty() || line.starts_with("    ") || !line.starts_with("  ")
            })
            .collect::<Vec<_>>()
            .join("\n"))
    }

    #[test]
    fn release_ci_installs_supervisor_build_tools() -> Result<(), String> {
        let workflow = fs::read_to_string(root().join(".github/workflows/release.yml"))
            .map_err(|error| format!("published Steward release workflow is required: {error}"))?;

        assert!(
            workflow.contains("run: cargo xtask ci"),
            "release validation must run the complete Steward CI gate"
        );
        assert!(
            workflow.contains("supervisor-tools: \"true\""),
            "release validation must install Zig and cargo-zigbuild before pinned conformance"
        );

        Ok(())
    }

    #[test]
    fn deterministic_local_identity_fixture_is_absent_from_release_images() -> Result<(), String> {
        let repository = root();
        let production_container = fs::read_to_string(repository.join("build/package.Dockerfile"))
            .map_err(|error| format!("production container build is required: {error}"))?;
        let release_workflow = fs::read_to_string(repository.join(".github/workflows/release.yml"))
            .map_err(|error| format!("release workflow is required: {error}"))?;
        let binary_manifest =
            fs::read_to_string(repository.join("bins/steward-apiserver/Cargo.toml"))
                .map_err(|error| format!("apiserver manifest is required: {error}"))?;
        let binary_source =
            fs::read_to_string(repository.join("bins/steward-apiserver/src/main.rs"))
                .map_err(|error| format!("apiserver source is required: {error}"))?;

        assert!(
            binary_manifest.contains("local-fixtures = [\"steward-store/local-fixtures\"]"),
            "the local fixture command must require an explicit compile-time feature"
        );
        assert!(
            binary_source.contains("#[cfg(feature = \"local-fixtures\")]"),
            "the local fixture command must be compile-time excluded by default"
        );
        assert!(
            !production_container.contains("local-fixtures")
                && !release_workflow.contains("--features local-fixtures"),
            "published runtime images must never enable the deterministic local identity fixture"
        );
        Ok(())
    }

    #[test]
    fn browser_e2e_ci_uses_the_pinned_loopback_gate() -> Result<(), String> {
        let repository = root();
        let workflow = fs::read_to_string(repository.join(".github/workflows/ci.yml"))
            .map_err(|error| format!("Steward CI workflow is required: {error}"))?;
        let package = fs::read_to_string(repository.join("package.json"))
            .map_err(|error| format!("pinned browser test package is required: {error}"))?;
        let node_version = fs::read_to_string(repository.join(".node-version"))
            .map_err(|error| format!("pinned Node version is required: {error}"))?;
        let journey = fs::read_to_string(repository.join("tests/browser/steward-ui.spec.mjs"))
            .map_err(|error| format!("loopback browser journey is required: {error}"))?;
        let xtask_source = include_str!("main.rs");

        let browser_job = workflow
            .split("  browser-e2e:")
            .nth(1)
            .and_then(|jobs| jobs.split("\n  pinned:").next())
            .ok_or_else(|| "browser E2E CI job is required".to_owned())?;

        assert!(
            browser_job.contains("node-version-file: .node-version"),
            "browser E2E CI must use the repository-pinned Node runtime"
        );
        assert!(
            browser_job.contains("npm exec playwright install --with-deps chromium"),
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
            browser_job.contains(
                "cargo build -p steward-apiserver --locked --features admin-demo --examples"
            ),
            "browser E2E CI must precompile loopback demos before Playwright starts"
        );
        assert!(
            xtask_source.contains("\"steward-apiserver\",")
                && xtask_source.contains("\"--examples\",")
                && journey.contains("exampleBinary(\"user-envelope-demo\")")
                && journey.contains("exampleBinary(\"admin-dashboard-demo\")"),
            "the local browser gate must build and then directly supervise its loopback demos"
        );
        assert!(
            package.contains("\"@playwright/test\": \"1.62.1\""),
            "the browser runner must be exact-version pinned in the source manifest"
        );
        assert_eq!(node_version.trim(), "26.5.0");
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
                "the loopback browser journey must cover {required}"
            );
        }

        Ok(())
    }

    #[test]
    fn controller_owned_task_lifecycle_is_a_named_ci_e2e_gate() -> Result<(), String> {
        let workflow = fs::read_to_string(root().join(".github/workflows/ci.yml"))
            .map_err(|error| format!("Steward CI workflow is required: {error}"))?;
        let xtask_source = include_str!("main.rs");
        let task_wrapper = fs::read_to_string(root().join("scripts/task-submission-e2e.sh"))
            .map_err(|error| format!("task lifecycle wrapper is required: {error}"))?;
        let task_callback = fs::read_to_string(root().join("scripts/task-submission-inside.sh"))
            .map_err(|error| format!("task lifecycle image callback is required: {error}"))?;

        let lifecycle_job = ci_job(&workflow, "e2e-controller-runtime-lifecycle")?;
        let pinned_job = ci_job(&workflow, "pinned")?;

        assert!(
            lifecycle_job.contains("cargo xtask e2e-controller-runtime-lifecycle"),
            "controller-owned lifecycle CI must invoke the named xtask E2E gate"
        );
        assert!(
            pinned_job.contains("- e2e-controller-runtime-lifecycle")
                && pinned_job.contains(
                    "CONTROLLER_RUNTIME_LIFECYCLE: ${{ needs.e2e-controller-runtime-lifecycle.result }}"
                )
                && pinned_job.contains("${CONTROLLER_RUNTIME_LIFECYCLE}"),
            "the pinned aggregate must fail when the controller-owned lifecycle E2E fails"
        );
        assert!(
            xtask_source.contains("\"e2e-controller-runtime-lifecycle\" if rest.is_empty()"),
            "the named controller-owned lifecycle E2E must be dispatchable locally"
        );
        assert!(
            xtask_source.contains("e2e_controller_runtime_lifecycle"),
            "the named controller-owned lifecycle command must have a dedicated implementation"
        );
        assert!(
            task_wrapper.contains("scripts/task-submission-inside.sh"),
            "the task lifecycle wrapper must defer image provision until the post-S0 callback"
        );
        assert!(
            !task_wrapper.contains("build-steward-mint-image.sh"),
            "the task lifecycle wrapper must not build the mint image before the long S0 setup gap"
        );
        assert!(
            !task_wrapper.contains("build-patched-mcp-gw.sh"),
            "the task lifecycle wrapper must not build mcp-gw before the long S0 setup gap"
        );
        for required in [
            "build-steward-mint-image.sh",
            "build-patched-mcp-gw.sh",
            "e2e/Dockerfile.task",
            "docker image inspect",
            "exec bash \"${ROOT}/scripts/s2-inference-inside.sh\"",
        ] {
            assert!(
                task_callback.contains(required),
                "the post-S0 task callback must provision and inspect local images before S2: missing {required}"
            );
        }
        let mint_build = task_callback
            .find("build-steward-mint-image.sh")
            .ok_or_else(|| "task callback must build mint".to_owned())?;
        let mcp_gw_build = task_callback
            .find("build-patched-mcp-gw.sh")
            .ok_or_else(|| "task callback must build mcp-gw".to_owned())?;
        let task_build = task_callback
            .find("e2e/Dockerfile.task")
            .ok_or_else(|| "task callback must build controller/task image".to_owned())?;
        let image_inspect = task_callback
            .find("docker image inspect")
            .ok_or_else(|| "task callback must inspect built images".to_owned())?;
        let s2_exec = task_callback
            .find("exec bash \"${ROOT}/scripts/s2-inference-inside.sh\"")
            .ok_or_else(|| "task callback must enter S2 after image checks".to_owned())?;
        assert!(
            mint_build < image_inspect
                && mcp_gw_build < image_inspect
                && task_build < image_inspect,
            "the post-S0 callback must build every local image before inspecting it"
        );
        assert!(
            image_inspect < s2_exec,
            "the post-S0 callback must inspect every local image before any kind load in S2"
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
    fn runtime_e2e_reuses_builds_and_preserves_observed_runtime_headroom() -> Result<(), String> {
        let workflow = fs::read_to_string(root().join(".github/workflows/ci.yml"))
            .map_err(|error| format!("Steward CI workflow is required: {error}"))?;
        let runtime = ci_job(&workflow, "e2e-runtime")?;

        assert!(
            runtime.contains("timeout-minutes: 105"),
            "the shared runtime E2E must preserve headroom for the observed 29m05s revocation lane while reusing one Rust build"
        );
        assert_eq!(
            runtime.matches("Restore shared Rust build cache").count(),
            1,
            "the shared runtime E2E must restore one cache for S1, S2, and S5"
        );
        assert_eq!(
            runtime.matches("cargo xtask e2e-s1").count(),
            1,
            "the shared runtime E2E must run the S1 lane exactly once"
        );
        assert_eq!(
            runtime.matches("cargo xtask e2e-s2").count(),
            1,
            "the shared runtime E2E must run the S2 lane exactly once"
        );
        assert_eq!(
            runtime.matches("cargo xtask e2e-s5").count(),
            1,
            "the shared runtime E2E must run the S5 lane exactly once"
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
            !schema.contains("\"const\": \"kata-qemu\"")
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
            "only the projected service-account token volume may provide the workload source credential path"
        );
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
    fn task_copy_smoke_contract_is_authority_bounded_and_idempotently_bootstrapped()
    -> Result<(), String> {
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
    fn task_and_bootstrap_share_one_kubernetes_token_review_audience() -> Result<(), String> {
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
            "build/connections-bridge.Dockerfile",
            "docker buildx build",
            "type=oci",
            "containerimage.digest",
            "kind load image-archive",
            "server.sandboxImagePullPolicy=Never",
            "server.defaultRuntimeClassName=openshell-runc",
            "STEWARD_OPENSHELL_RUNTIME_CLASS_NAME=openshell-runc",
            "STEWARD_CONNECTIONS_BRIDGE_IMAGE",
            "handler: runc",
            "server.oidc.issuer=",
            "--test openshell_adapter_v0098",
        ] {
            assert!(
                harness.contains(required),
                "OpenShell adapter integration harness is missing {required}"
            );
        }
        assert!(
            e2e_source.contains("assert_runtime_class_propagation")
                && !e2e_source.contains("kata_bound"),
            "the kind lane must describe runtime-class propagation, not Kata isolation"
        );
        for required in [
            "CONNECTIONS_BRIDGE_AGENT_TYPE",
            "bridge_image: Some(required(\"STEWARD_CONNECTIONS_BRIDGE_IMAGE\")?)",
            "\"/bin/sh\".to_owned()",
            "cp in/payload.bin",
        ] {
            assert!(
                e2e_source.contains(required),
                "the pinned OpenShell lane must execute the bridge stage/copy/collect path under the v0.0.98 supervisor and Landlock: missing {required}"
            );
        }
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
    fn controller_e2e_harnesses_require_authenticated_openshell_transport() -> Result<(), String> {
        let shared_harness = fs::read_to_string(root().join("scripts/s0-0-openshell-spike.sh"))
            .map_err(|error| format!("shared OpenShell E2E harness is required: {error}"))?;
        for unsafe_setting in [
            "server.disableTls=true",
            "server.auth.allowUnauthenticatedUsers=true",
        ] {
            assert!(
                !shared_harness.contains(unsafe_setting),
                "controller E2E lanes must not enable unsafe OpenShell setting {unsafe_setting}"
            );
        }
        for required in [
            "STEWARD_OPENSHELL_CA_CERTIFICATE_FILE",
            "STEWARD_OPENSHELL_CLIENT_CERTIFICATE_FILE",
            "STEWARD_OPENSHELL_CLIENT_PRIVATE_KEY_FILE",
            "STEWARD_WORKLOAD_EXCHANGE_ENDPOINT",
            "STEWARD_WORKLOAD_EXCHANGE_SERVER_NAME",
            "STEWARD_WORKLOAD_EXCHANGE_CA_CERTIFICATE_FILE",
            "STEWARD_WORKLOAD_SOURCE_CREDENTIAL_FILE",
            "STEWARD_OPENSHELL_SERVER_NAME",
            "STEWARD_OPENSHELL_RUNTIME_CLASS_NAME",
        ] {
            assert!(
                shared_harness.contains(required),
                "shared controller E2E harness must export {required}"
            );
        }

        for test_path in ["e2e/s0.rs", "e2e/s1.rs"] {
            let harness = fs::read_to_string(root().join(test_path)).map_err(|error| {
                format!("controller E2E launch path {test_path} is required: {error}")
            })?;
            for required in [
                "STEWARD_OPENSHELL_CA_CERTIFICATE_FILE",
                "STEWARD_OPENSHELL_CLIENT_CERTIFICATE_FILE",
                "STEWARD_OPENSHELL_CLIENT_PRIVATE_KEY_FILE",
                "STEWARD_WORKLOAD_EXCHANGE_ENDPOINT",
                "STEWARD_WORKLOAD_EXCHANGE_SERVER_NAME",
                "STEWARD_WORKLOAD_EXCHANGE_CA_CERTIFICATE_FILE",
                "STEWARD_WORKLOAD_SOURCE_CREDENTIAL_FILE",
                "STEWARD_OPENSHELL_SERVER_NAME",
                "STEWARD_OPENSHELL_RUNTIME_CLASS_NAME",
            ] {
                assert!(
                    harness.contains(required),
                    "controller E2E launch path {test_path} must forward {required}"
                );
            }
        }

        let in_cluster_harness = fs::read_to_string(root().join("scripts/s2-inference-inside.sh"))
            .map_err(|error| format!("in-cluster controller E2E harness is required: {error}"))?;
        let in_cluster_controller = fs::read_to_string(root().join("config/s2/stack.yaml"))
            .map_err(|error| format!("in-cluster controller fixture is required: {error}"))?;
        for required in [
            "STEWARD_OPENSHELL_CA_CERTIFICATE_FILE",
            "STEWARD_OPENSHELL_CLIENT_CERTIFICATE_FILE",
            "STEWARD_OPENSHELL_CLIENT_PRIVATE_KEY_FILE",
            "STEWARD_WORKLOAD_EXCHANGE_ENDPOINT",
            "STEWARD_WORKLOAD_EXCHANGE_SERVER_NAME",
            "STEWARD_WORKLOAD_EXCHANGE_CA_CERTIFICATE_FILE",
            "STEWARD_WORKLOAD_SOURCE_CREDENTIAL_FILE",
            "STEWARD_OPENSHELL_SERVER_NAME",
            "STEWARD_OPENSHELL_RUNTIME_CLASS_NAME",
        ] {
            assert!(
                in_cluster_harness.contains(required),
                "in-cluster controller E2E harness must require {required}"
            );
            assert!(
                in_cluster_controller.contains(required),
                "in-cluster controller fixture must provide {required}"
            );
        }
        assert!(
            in_cluster_controller
                .contains("value: https://openshell.openshell.svc.cluster.local:8080"),
            "in-cluster controller must use authenticated TLS to OpenShell"
        );

        Ok(())
    }

    #[test]
    fn s2_controller_can_create_only_task_agentruntimes() -> Result<(), String> {
        let fixture = fs::read_to_string(root().join("config/s2/stack.yaml"))
            .map_err(|error| format!("S2 controller fixture is required: {error}"))?;
        let controller_role = fixture
            .split("---")
            .find(|document| {
                document.contains("kind: ClusterRole")
                    && document.contains("name: steward-s2-controller")
            })
            .ok_or_else(|| "S2 controller ClusterRole is required".to_owned())?;

        assert!(
            controller_role.contains("resources: [\"agentruntimes\"]\n    verbs: [\"create\"]"),
            "the Task controller must be allowed to create only AgentRuntime resources"
        );
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
        let promotion_test =
            fs::read_to_string(root().join("scripts/test-promote-ecr-artifact.sh"))
                .map_err(|error| format!("ECR promotion retry tests are required: {error}"))?;
        let platform_resolution_test =
            fs::read_to_string(root().join("scripts/test-resolve-ecr-platform-digest.sh"))
                .map_err(|error| format!("ECR platform resolution tests are required: {error}"))?;
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
        let bridge_bash = fs::read_to_string(root().join("build/connections-bridge-bash"))
            .map_err(|error| {
                format!("Connections bridge bash compatibility wrapper is required: {error}")
            })?;

        for required in [
            "apiserver:",
            "controller:",
            "mint:",
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
            !templates.contains("kind: Ingress"),
            "Steward chart must not publish an Ingress"
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
        assert!(!controller.contains(".Values.secrets.jira"));
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
            "ecr-bridge-attestation-bundle.jsonl",
            "oci://${IMAGE_REPOSITORY}@${bridge_digest}",
            "docker login \"$ECR_REGISTRY\" --username AWS --password-stdin",
            "Bridge ECR signer identity:",
            "Bridge ECR source repository:",
            "Bridge ECR source commit:",
        ] {
            assert!(
                workflow.contains(artifact),
                "release workflow must record {artifact}"
            );
        }
        assert!(
            values.contains("mcpGatewayOrigin") && schema.contains("mcpGatewayOrigin"),
            "stable bridge chart values must require the controller-owned MCP-GW origin"
        );
        assert!(
            controller.contains("stable-bridge-attestation"),
            "stable bridge controller must mount the immutable provenance bundle"
        );
        assert!(
            controller.contains("STEWARD_STABLE_BRIDGE_MCP_GW_ORIGIN"),
            "stable bridge controller must receive the server-owned MCP-GW origin"
        );
        assert!(
            workflow.matches("exit-code: \"1\"").count() >= 2,
            "image and chart vulnerability scans must fail releases on critical findings"
        );
        for required in [
            "aws ecr wait image-scan-complete",
            "aws ecr describe-image-scan-findings",
            "scripts/promote-ecr-artifact.sh",
            "scripts/test-promote-ecr-artifact.sh",
            "scripts/resolve-ecr-platform-digest.sh",
            "scripts/test-resolve-ecr-platform-digest.sh",
            "ecr-$component-scan-platform.digest",
            "Scanned linux/amd64 digest",
        ] {
            assert!(
                workflow.contains(required) || release_validation.contains(required),
                "release path is missing {required}"
            );
        }
        for required in [
            "one runnable linux/amd64 manifest plus SBOM and provenance attestations",
            "missing runnable linux/amd64 manifest fails closed",
            "ambiguous runnable linux/amd64 manifests fail closed",
        ] {
            assert!(
                platform_resolution_test.contains(required),
                "ECR platform resolution tests are missing: {required}"
            );
        }
        for required in ["missing target", "matching target", "different digest"] {
            assert!(
                promotion_test.contains(required),
                "ECR promotion retry tests are missing the {required} case"
            );
        }
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
            "FROM busybox:1.37.0-musl@sha256:",
            "FROM gcr.io/distroless/cc-debian12:nonroot@sha256:",
            "COPY --chmod=0755 build/connections-bridge-bash /rootfs/usr/bin/bash",
            "cp /bin/busybox /rootfs/usr/bin/busybox",
            "for applet in cp find id ip mkdir mktemp rm sh sleep tar touch",
            "ln -s /usr/bin/busybox /rootfs/usr/bin/",
            "ln -s \"/usr/bin/${applet}\" \"/rootfs/bin/${applet}\"",
            "ln -s /usr/bin/bash /rootfs/bin/bash",
            "COPY --from=toolbox /rootfs/usr/bin/ /usr/bin/",
            "COPY --from=toolbox /rootfs/bin/ /bin/",
            "COPY --chown=65532:65532 --from=toolbox /sandbox /sandbox",
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
            !bridge_container.contains("COPY --from=toolbox /bin/busybox /bin/busybox"),
            "Connections bridge applets must resolve to executable inodes under Landlock-allowed /usr"
        );
        assert_eq!(
            bridge_bash, "#!/usr/bin/sh\nexec /usr/bin/sh \"$@\"\n",
            "the bash compatibility wrapper must delegate only the OpenShell -lc relay to BusyBox sh"
        );
        for command in ["cp", "find", "mktemp", "rm", "sleep", "tar", "touch"] {
            let smoke_check = format!("command -v {command} >/dev/null");
            assert!(
                release_validation.contains(&smoke_check),
                "Connections bridge release smoke must execute the OpenShell workspace-init prerequisite check: {smoke_check}"
            );
        }
        for exercise in [
            "mktemp -d /sandbox/workspace-init.XXXXXX",
            "touch \"${workspace_init}/source\"",
            "cp \"${workspace_init}/source\" \"${workspace_init}/copied\"",
            "find \"${workspace_init}\" -type f -name source",
            "tar -cf \"${workspace_init}/source.tar\" -C \"${workspace_init}\" source",
            "rm -rf \"${workspace_init}\"",
        ] {
            assert!(
                release_validation.contains(exercise),
                "Connections bridge release smoke must exercise the OpenShell workspace-init operation: {exercise}"
            );
        }
        for assertion in [
            "test \"$(id -g)\" = \"65532\"",
            "test -f \"${workspace_init}/copied\"",
            "test -s \"${workspace_init}/copied\"",
            "/bin/busybox cmp \"${workspace_init}/source\" \"${workspace_init}/copied\"",
            "test ! -e \"${workspace_init}\"",
        ] {
            assert!(
                release_validation.contains(assertion),
                "Connections bridge release smoke must retain its runtime postcondition: {assertion}"
            );
        }
        for assertion in [
            "/bin/bash -lc",
            "test \"$(/bin/busybox readlink /bin/sh)\" = \"/usr/bin/sh\"",
            "test \"$(/bin/busybox readlink /bin/bash)\" = \"/usr/bin/bash\"",
            "test \"$(/bin/busybox readlink /usr/bin/sh)\" = \"/usr/bin/busybox\"",
            "sleep infinity &",
            "sleep_pid=\"$!\"",
            "kill -0 \"${sleep_pid}\"",
            "kill \"${sleep_pid}\"",
            "wait \"${sleep_pid}\" || test \"$?\" = \"143\"",
            "if kill -0 \"${sleep_pid}\" 2>/dev/null; then",
        ] {
            assert!(
                release_validation.contains(assertion),
                "Connections bridge release smoke must retain its OpenShell supervisor command assertion: {assertion}"
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
            let nonce = NEXT_REPOSITORY_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("steward-migration-{}-{nonce}", std::process::id()));
            fs::create_dir_all(&path)
                .map_err(|error| format!("failed to create {}: {error}", path.display()))?;
            Ok(Self { path })
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

    fn write_executable(path: &Path, content: &str) -> Result<(), String> {
        fs::write(path, content)
            .map_err(|error| format!("failed to write {}: {error}", path.display()))?;
        let mut permissions = fs::metadata(path)
            .map_err(|error| format!("failed to inspect {}: {error}", path.display()))?
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions)
            .map_err(|error| format!("failed to make {} executable: {error}", path.display()))
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
    fn openshell_spike_dependencies_are_feature_isolated() -> Result<(), String> {
        let manifest_path = root().join("adapters/openshell/Cargo.toml");
        let content = fs::read_to_string(&manifest_path)
            .map_err(|error| format!("failed to read {}: {error}", manifest_path.display()))?;
        let manifest = toml::from_str::<toml::Table>(&content)
            .map_err(|error| format!("failed to parse {}: {error}", manifest_path.display()))?;
        let dependencies = manifest
            .get("dependencies")
            .and_then(toml::Value::as_table)
            .ok_or_else(|| "OpenShell adapter must declare dependencies".to_owned())?;
        for dependency in ["openshell-sdk", "tokio"] {
            let optional = dependencies
                .get(dependency)
                .and_then(toml::Value::as_table)
                .and_then(|specification| specification.get("optional"))
                .and_then(toml::Value::as_bool);
            assert_eq!(
                optional,
                Some(true),
                "{dependency} must be optional so normal xtask and workspace builds exclude the OpenShell SDK graph"
            );
        }
        let spike_features = manifest
            .get("features")
            .and_then(toml::Value::as_table)
            .and_then(|features| features.get("s0-spike"))
            .and_then(toml::Value::as_array)
            .map(|features| {
                features
                    .iter()
                    .filter_map(toml::Value::as_str)
                    .collect::<Vec<_>>()
            })
            .ok_or_else(|| "OpenShell adapter must declare the s0-spike feature".to_owned())?;
        for dependency in ["dep:openshell-sdk", "dep:tokio"] {
            assert!(
                spike_features.contains(&dependency),
                "s0-spike must activate {dependency}"
            );
        }

        let required_features = manifest
            .get("example")
            .and_then(toml::Value::as_array)
            .and_then(|examples| {
                examples.iter().find(|example| {
                    example.get("name").and_then(toml::Value::as_str) == Some("workspace_contract")
                })
            })
            .and_then(|example| example.get("required-features"))
            .and_then(toml::Value::as_array)
            .map(|features| {
                features
                    .iter()
                    .filter_map(toml::Value::as_str)
                    .collect::<Vec<_>>()
            });
        assert_eq!(
            required_features,
            Some(vec!["s0-spike"]),
            "the live workspace example must require the feature that activates its SDK dependencies"
        );
        Ok(())
    }

    #[test]
    fn openshell_spike_selects_the_host_cli_artifact_before_cluster_setup() -> Result<(), String> {
        let operating_system = std::env::consts::OS;
        let architecture = std::env::consts::ARCH;
        let expected = match (operating_system, architecture) {
            ("macos", "aarch64") => "openshell-aarch64-apple-darwin.tar.gz",
            ("linux", "aarch64") => "openshell-aarch64-unknown-linux-musl.tar.gz",
            ("linux", "x86_64") => "openshell-x86_64-unknown-linux-musl.tar.gz",
            _ => return Ok(()),
        };
        let output = Command::new("bash")
            .arg(root().join("scripts/s0-0-openshell-spike.sh"))
            .arg("--print-openshell-cli-asset")
            .output()
            .map_err(|error| format!("failed to inspect OpenShell CLI selection: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "OpenShell CLI selection failed before cluster setup: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            expected,
            "the spike must download the OpenShell CLI artifact for its host platform"
        );
        Ok(())
    }

    #[test]
    fn carried_openshell_patch_has_a_reproducible_image_build_contract() -> Result<(), String> {
        let output = Command::new("bash")
            .arg(root().join("scripts/build-patched-openshell-supervisor.sh"))
            .arg("--print-contract")
            .output()
            .map_err(|error| format!("failed to inspect patched supervisor build: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "patched supervisor build contract failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            concat!(
                "source=https://github.com/NVIDIA/OpenShell.git\n",
                "commit=1d4ac708f1d2a9ab94204cdce6ca0eee7e792839\n",
                "patch=third_party/openshell-patches/v0.0.90/",
                "0001-prepare-supervisor-identity-mount-namespace.patch\n",
                "image=openshell/supervisor:steward-spiffe-v0090\n",
                "rust=1.95.0\n",
                "zig=0.14.1\n",
                "cargo-zigbuild=0.22.3\n",
                "dockerfile-frontend=docker/dockerfile:1.4@sha256:",
                "9ba7531bd80fb0a858632727cf7a112fbfd19b17e94c4e84ced81e24ef1a0dbc\n",
                "supervisor-base=alpine:3.22@sha256:",
                "14358309a308569c32bdc37e2e0e9694be33a9d99e68afb0f5ff33cc1f695dce\n",
                "apk-packages=alpine-baselayout=3.7.0-r0 ",
                "alpine-baselayout-data=3.7.0-r0 alpine-keys=2.5-r0 ",
                "alpine-release=3.22.5-r0 apk-tools=2.14.10-r0 ",
                "busybox=1.37.0-r20 busybox-binsh=1.37.0-r20 ",
                "ca-certificates-bundle=20260611-r0 gmp=6.3.0-r3 ",
                "iptables=1.8.11-r1 iptables-legacy=1.8.11-r1 ",
                "jansson=2.14.1-r0 libapk2=2.14.10-r0 libcrypto3=3.5.7-r0 ",
                "libip4tc=1.8.11-r1 libip6tc=1.8.11-r1 libmnl=1.0.5-r2 ",
                "libncursesw=6.5_p20250503-r0 libnftnl=1.2.9-r0 ",
                "libssl3=3.5.7-r0 libxtables=1.8.11-r1 musl=1.2.5-r12 ",
                "musl-utils=1.2.5-r12 ncurses-terminfo-base=6.5_p20250503-r0 ",
                "nftables=1.1.3-r0 readline=8.2.13-r1 scanelf=1.3.8-r1 ",
                "ssl_client=1.37.0-r20 zlib=1.3.2-r0\n",
                "apk-artifact-base=https://dl-cdn.alpinelinux.org/alpine/v3.22/main\n",
                "apk-artifacts-aarch64=",
                "gmp-6.3.0-r3.apk=0d2eb1079b1b5692e9e6652ff0e269caeb9c812f483e34c88d461c03bcf75460 ",
                "iptables-1.8.11-r1.apk=0a10fe634e3525082a1219487cd044d987ac4a55ed5aa551bf758e81223e1cfb ",
                "iptables-legacy-1.8.11-r1.apk=8beced6a354697e50014e2cebe0dcea3dc65d2dbbb708006a149999b3b029919 ",
                "jansson-2.14.1-r0.apk=f57c3bceb823add72ee0ccbf17aa7fc98596ff9eb0c11d0e5a2c28260ea87dcd ",
                "libip4tc-1.8.11-r1.apk=6418c50ff5287f6aca02ba7d8baab7cfe729a898793c9a8856cf8975b3f759c2 ",
                "libip6tc-1.8.11-r1.apk=48ec17436d0e6fd754c8eb50862b5604e86e80ac7d19d7c7a48f9f622f305306 ",
                "libmnl-1.0.5-r2.apk=213a7e87553bed3d9159b2e74d2627885c259883e61714b357949ca806eb1f8d ",
                "libncursesw-6.5_p20250503-r0.apk=419b375e8a4345e7172b1f0f3a3c57db61374f5408cdb875d9e860bd4c243aca ",
                "libnftnl-1.2.9-r0.apk=6912a5d56b31d3365b8dd0d6339bd70a2bfc9e25dab0c387f77174113c43e664 ",
                "libxtables-1.8.11-r1.apk=e84f0d6b69d4318f297056d00ccbe433e6c7d163fe6767405e373248c42e3e88 ",
                "ncurses-terminfo-base-6.5_p20250503-r0.apk=3d37403e0b5ab9eb0c1ce269444e4a385faec9fe6af452c1c6956806b13d2bd6 ",
                "nftables-1.1.3-r0.apk=e48df72b87580444a6b3094e2292886994c3d9c600435d7a62949e3706ce2c07 ",
                "readline-8.2.13-r1.apk=334af29dbf6b5a71a87af4d6a58e2967a8f711a51d00093de0e1498daf83ceb2\n",
                "apk-artifacts-x86_64=",
                "gmp-6.3.0-r3.apk=d3f987ae3836ac7774324bff443dd49d03b846209660729d0c30dfff5546e138 ",
                "iptables-1.8.11-r1.apk=defe876173d08fe30b664b2d9f60d0237298a639e3a7d0e3f83ac637fd6519db ",
                "iptables-legacy-1.8.11-r1.apk=aebfc932aa2e3b27e4895250aa4b73ead107116a3e8cd33ebf2d4036e46c043e ",
                "jansson-2.14.1-r0.apk=7fde81421482507163410715a7ef7d7df6a091870edab855c24e8f6fc84e3e2d ",
                "libip4tc-1.8.11-r1.apk=3ca5cd36732d59374d970d937f781469969bf668ff5eb65207892f7cfcd27f02 ",
                "libip6tc-1.8.11-r1.apk=dc0e2aa34b2454bce4c8f6860484925fd6e5bea8eeb24e53d4b9526a24ed4c2c ",
                "libmnl-1.0.5-r2.apk=e9dc63c95a0c8a263dc7f0705e6f7a2220d632a675ce85db798d33a40b1c1b0b ",
                "libncursesw-6.5_p20250503-r0.apk=aeafdfca68147b014705b4e2564639ade6345198debb522f2c6c51d32e417651 ",
                "libnftnl-1.2.9-r0.apk=bef674635aec00dca296b8206751245f7664ba10c416f31c01a563af596720ae ",
                "libxtables-1.8.11-r1.apk=5367f1f5c309a0aeede0a08d73af717f582f7a135ff97080aa0e5b7c72fd97af ",
                "ncurses-terminfo-base-6.5_p20250503-r0.apk=0815a5f0403974bb9c34d456e71dc9c0222cb5455d393bc63c44b573da3d7fe0 ",
                "nftables-1.1.3-r0.apk=96bdef738b0ae22ad86500af3345622c7f5bdc6ade0a407d087d1ea3223c8bc7 ",
                "readline-8.2.13-r1.apk=520fa586c689144928191bee13e2c85ff4e170ad87d1471ec48e3e97611673d8\n",
                "build-script-sha256=",
                "1d2caafeff04d08627cfcaf436edbcdda8ea0b57223744056e2741bed321fac8\n",
            ),
            "the carried patch must build from its recorded immutable source and image contract"
        );
        Ok(())
    }

    #[test]
    fn mcp_gateway_patch_is_applied_in_an_isolated_repository() -> Result<(), String> {
        let script_path = root().join("scripts/build-patched-mcp-gw.sh");
        let script = fs::read_to_string(&script_path)
            .map_err(|error| format!("failed to read {}: {error}", script_path.display()))?;
        let source_init = script
            .find("git -C \"${source_dir}\" init --quiet")
            .ok_or_else(|| {
                "the mcp-gw source archive must become an isolated repository before patching"
                    .to_owned()
            })?;
        let patch_check = script
            .find("git -C \"${source_dir}\" apply --check")
            .ok_or_else(|| "the mcp-gw build must check its carried patch".to_owned())?;
        assert!(
            source_init < patch_check,
            "the carried patch must not resolve against Steward's parent repository"
        );
        Ok(())
    }

    #[test]
    fn supervisor_build_isolates_target_and_compiler_environment() -> Result<(), String> {
        let script_path = root().join("scripts/build-patched-openshell-supervisor.sh");
        let script = fs::read_to_string(&script_path)
            .map_err(|error| format!("failed to read {}: {error}", script_path.display()))?;
        for required in [
            "SUPERVISOR_TARGET_DIR=\"${RUN_DIR}/cargo-target\"",
            "binary=\"${SUPERVISOR_TARGET_DIR}/${rust_target}/release/openshell-sandbox\"",
            "CARGO_TARGET_DIR=\"${SUPERVISOR_TARGET_DIR}\"",
            "rustup target add --toolchain \"${RUST_TOOLCHAIN}\" \"${rust_target}\"",
            "cargo +\"${RUST_TOOLCHAIN}\" zigbuild",
            "scrub_ambient_compiler_overrides",
        ] {
            assert!(
                script.contains(required),
                "the supervisor build must isolate its target directory and compiler environment: missing {required}"
            );
        }
        Ok(())
    }

    #[test]
    fn supervisor_build_scrubs_target_specific_compiler_overrides() -> Result<(), String> {
        let script_path = root().join("scripts/build-patched-openshell-supervisor.sh");
        let script = fs::read_to_string(&script_path)
            .map_err(|error| format!("failed to read {}: {error}", script_path.display()))?;
        for required in [
            "scrub_ambient_compiler_overrides()",
            "CARGO_TARGET_*",
            "CARGO_BUILD_* | CARGO_PROFILE_*",
            "CC | CC_* | *_CC",
            "CFLAGS | CFLAGS_* | *_CFLAGS",
            "SOURCE_DATE_EPOCH",
            "unset \"${variable_name}\"",
        ] {
            assert!(
                script.contains(required),
                "the supervisor build must remove target-specific ambient compiler inputs: missing {required}"
            );
        }
        let scrub = script
            .find("\nscrub_ambient_compiler_overrides\n")
            .ok_or_else(|| "the build must scrub its parent environment".to_owned())?;
        let source_checkout = script
            .find("git init --quiet")
            .ok_or_else(|| "the build must retain its pinned source checkout".to_owned())?;
        let image_build = script
            .find("docker buildx build")
            .ok_or_else(|| "the build must retain its pinned image packaging".to_owned())?;
        assert!(
            scrub < source_checkout && scrub < image_build,
            "ambient compiler inputs must be scrubbed in the parent before both compilation and image packaging"
        );
        Ok(())
    }

    #[test]
    fn supervisor_cache_key_includes_its_build_implementation() -> Result<(), String> {
        let script_path = root().join("scripts/build-patched-openshell-supervisor.sh");
        let script = fs::read_to_string(&script_path)
            .map_err(|error| format!("failed to read {}: {error}", script_path.display()))?;
        for required in [
            "build_script_sha256()",
            "\"build-script-sha256=$(build_script_sha256)\"",
        ] {
            assert!(
                script.contains(required),
                "the reusable supervisor image must be invalidated by build-logic changes: missing {required}"
            );
        }
        Ok(())
    }

    #[test]
    fn supervisor_build_isolates_git_and_cargo_config_discovery() -> Result<(), String> {
        let script_path = root().join("scripts/build-patched-openshell-supervisor.sh");
        let script = fs::read_to_string(&script_path)
            .map_err(|error| format!("failed to read {}: {error}", script_path.display()))?;
        for required in [
            "GIT_CONFIG_GLOBAL=/dev/null",
            "GIT_CONFIG_SYSTEM=/dev/null",
            "GIT_CONFIG_NOSYSTEM=1",
            "\"${SOURCE_DIR}/.cargo/config.toml\"",
            "\"${SUPERVISOR_CARGO_HOME}/config.toml\"",
            "cd /",
            "--manifest-path \"${SOURCE_DIR}/Cargo.toml\"",
        ] {
            assert!(
                script.contains(required),
                "the supervisor build must isolate ambient Git and Cargo config while retaining the pinned upstream config: missing {required}"
            );
        }
        Ok(())
    }

    #[test]
    fn supervisor_runtime_locks_the_complete_apk_closure() -> Result<(), String> {
        let script_path = root().join("scripts/build-patched-openshell-supervisor.sh");
        let script = fs::read_to_string(&script_path)
            .map_err(|error| format!("failed to read {}: {error}", script_path.display()))?;
        for package in [
            "alpine-baselayout=3.7.0-r0",
            "alpine-baselayout-data=3.7.0-r0",
            "alpine-keys=2.5-r0",
            "alpine-release=3.22.5-r0",
            "apk-tools=2.14.10-r0",
            "busybox=1.37.0-r20",
            "busybox-binsh=1.37.0-r20",
            "ca-certificates-bundle=20260611-r0",
            "gmp=6.3.0-r3",
            "iptables=1.8.11-r1",
            "iptables-legacy=1.8.11-r1",
            "jansson=2.14.1-r0",
            "libapk2=2.14.10-r0",
            "libcrypto3=3.5.7-r0",
            "libip4tc=1.8.11-r1",
            "libip6tc=1.8.11-r1",
            "libmnl=1.0.5-r2",
            "libncursesw=6.5_p20250503-r0",
            "libnftnl=1.2.9-r0",
            "libssl3=3.5.7-r0",
            "libxtables=1.8.11-r1",
            "musl=1.2.5-r12",
            "musl-utils=1.2.5-r12",
            "ncurses-terminfo-base=6.5_p20250503-r0",
            "nftables=1.1.3-r0",
            "readline=8.2.13-r1",
            "scanelf=1.3.8-r1",
            "ssl_client=1.37.0-r20",
            "zlib=1.3.2-r0",
        ] {
            assert!(
                script.contains(package),
                "the pinned supervisor runtime package closure is incomplete: missing {package}"
            );
        }
        assert!(
            script.contains(
                "RUN --network=none apk --no-cache --no-network --repositories-file /dev/null add /tmp/steward-apks/*.apk"
            ),
            "the generated runtime image must install the locked package closure without repository resolution"
        );
        Ok(())
    }

    #[test]
    fn supervisor_runtime_installs_checksum_pinned_apks_without_repository_indexes()
    -> Result<(), String> {
        let script_path = root().join("scripts/build-patched-openshell-supervisor.sh");
        let script = fs::read_to_string(&script_path)
            .map_err(|error| format!("failed to read {}: {error}", script_path.display()))?;
        for required in [
            "APK_ARTIFACTS_AARCH64=(",
            "APK_ARTIFACTS_X86_64=(",
            "download_apk_artifacts()",
            "openssl dgst -sha256 -r",
            "RUN --network=none apk --no-cache --no-network ",
            "--repositories-file /dev/null add /tmp/steward-apks/*.apk",
        ] {
            assert!(
                script.contains(required),
                "the supervisor runtime must install checksum-pinned APK artifacts without a mutable repository index: missing {required}"
            );
        }
        assert!(
            !script.contains("apk add --no-cache ${APK_PACKAGE_CLOSURE[*]}"),
            "the supervisor runtime must not resolve its locked package closure through a mutable APKINDEX"
        );
        Ok(())
    }

    #[test]
    fn supervisor_build_signals_terminate_after_cleanup() -> Result<(), String> {
        let script_path = root().join("scripts/build-patched-openshell-supervisor.sh");
        let script = fs::read_to_string(&script_path)
            .map_err(|error| format!("failed to read {}: {error}", script_path.display()))?;
        for required in [
            "trap cleanup EXIT",
            "trap 'exit 130' INT",
            "trap 'exit 143' TERM",
        ] {
            assert!(
                script.contains(required),
                "the supervisor build must terminate after handling signals: missing {required}"
            );
        }
        assert!(
            !script.contains("trap cleanup EXIT INT TERM"),
            "cleanup alone must not handle INT or TERM because Bash can resume execution afterwards"
        );
        Ok(())
    }

    #[test]
    fn supervisor_build_pins_runtime_layer_inputs() -> Result<(), String> {
        let script_path = root().join("scripts/build-patched-openshell-supervisor.sh");
        let script = fs::read_to_string(&script_path)
            .map_err(|error| format!("failed to read {}: {error}", script_path.display()))?;
        for required in [
            "# syntax=${DOCKERFILE_FRONTEND_IMAGE}",
            "FROM ${SUPERVISOR_BASE_IMAGE} AS supervisor",
            "COPY deploy/docker/.build/steward-apks/ /tmp/steward-apks/",
            "RUN --network=none apk --no-cache --no-network ",
            "--repositories-file /dev/null add /tmp/steward-apks/*.apk",
        ] {
            assert!(
                script.contains(required),
                "the supervisor runtime layer must be generated from pinned inputs: missing {required}"
            );
        }
        Ok(())
    }

    #[test]
    fn cached_openshell_supervisor_must_match_the_build_contract() -> Result<(), String> {
        let fixture = TestRepository::create()?;
        let bin = fixture.path.join("bin");
        fs::create_dir(&bin)
            .map_err(|error| format!("failed to create fake tool directory: {error}"))?;
        write_executable(
            &bin.join("docker"),
            r#"#!/usr/bin/env bash
set -euo pipefail
case "$1:$2" in
  info:--format)
    printf '%s\n' "${FAKE_DOCKER_ENGINE_ARCHITECTURE}"
    ;;
  image:inspect)
    printf '%s\n' "${FAKE_DOCKER_IMAGE_METADATA}"
    ;;
  *)
    exit 2
    ;;
esac
"#,
        )?;

        let system_path =
            std::env::var("PATH").map_err(|error| format!("PATH is unset: {error}"))?;
        let path = format!("{}:{system_path}", bin.display());
        let script = root().join("scripts/build-patched-openshell-supervisor.sh");
        let current_metadata =
            "d29214a0fd77894403531f86abfa7bc52db6c375c251d924be300552c1285c3d|arm64";
        let current = Command::new("bash")
            .arg(&script)
            .arg("--image-is-current")
            .env("PATH", &path)
            .env("FAKE_DOCKER_ENGINE_ARCHITECTURE", "aarch64")
            .env("FAKE_DOCKER_IMAGE_METADATA", current_metadata)
            .output()
            .map_err(|error| format!("failed to validate current supervisor image: {error}"))?;
        assert!(
            current.status.success(),
            "an image matching the source, patch content, and Docker architecture must be reusable: {}",
            String::from_utf8_lossy(&current.stderr)
        );

        for stale_metadata in [
            // The prior build logic and partial APK closure must not be reusable.
            "f445d04ba50e2d50690b58696fd67111ab36c74060e4229d5e0b7f33e4934d2d|arm64",
            "d29214a0fd77894403531f86abfa7bc52db6c375c251d924be300552c1285c3d|amd64",
        ] {
            let stale = Command::new("bash")
                .arg(&script)
                .arg("--image-is-current")
                .env("PATH", &path)
                .env("FAKE_DOCKER_ENGINE_ARCHITECTURE", "aarch64")
                .env("FAKE_DOCKER_IMAGE_METADATA", stale_metadata)
                .output()
                .map_err(|error| format!("failed to validate stale supervisor image: {error}"))?;
            assert!(
                !stale.status.success(),
                "an image with a stale build contract or architecture must be rebuilt"
            );
        }
        Ok(())
    }

    #[test]
    fn openshell_identity_spike_validates_a_cached_supervisor_before_reuse() -> Result<(), String> {
        let script_path = root().join("scripts/s0-0-openshell-spike.sh");
        let script = fs::read_to_string(&script_path)
            .map_err(|error| format!("failed to read {}: {error}", script_path.display()))?;
        assert!(
            script.contains(
                "\"${ROOT}/scripts/build-patched-openshell-supervisor.sh\" --image-is-current"
            ),
            "the identity spike must validate the cached image against the pinned build contract"
        );
        Ok(())
    }

    #[test]
    fn exact_mise_cross_tools_override_mismatched_path_tools() -> Result<(), String> {
        let fixture = TestRepository::create()?;
        let ambient_bin = fixture.path.join("ambient-bin");
        let pinned_zig_bin = fixture.path.join("pinned-zig");
        let pinned_zigbuild_bin = fixture.path.join("pinned-zigbuild");
        for directory in [&ambient_bin, &pinned_zig_bin, &pinned_zigbuild_bin] {
            fs::create_dir(directory)
                .map_err(|error| format!("failed to create {}: {error}", directory.display()))?;
        }
        for command in ["cargo", "git"] {
            write_executable(&ambient_bin.join(command), "#!/bin/sh\nexit 0\n")?;
        }
        write_executable(
            &ambient_bin.join("rustup"),
            r#"#!/bin/sh
if [ "$1:$2:$3" = "run:1.95.0:rustc" ]; then
  printf 'rustc 1.95.0 (59807616e 2026-04-14)\n'
fi
exit 0
"#,
        )?;
        write_executable(
            &ambient_bin.join("docker"),
            r#"#!/bin/sh
if [ "$1:$2" = "buildx:version" ]; then
  exit 0
fi
if [ "$1:$2" = "info:--format" ]; then
  printf 'linux\n'
  exit 0
fi
exit 2
"#,
        )?;
        write_executable(&ambient_bin.join("zig"), "#!/bin/sh\nprintf '0.99.0\\n'\n")?;
        write_executable(
            &ambient_bin.join("cargo-zigbuild"),
            "#!/bin/sh\nprintf 'cargo-zigbuild 0.99.0\\n'\n",
        )?;
        write_executable(
            &ambient_bin.join("mise"),
            r#"#!/bin/sh
case "$2" in
  zig@0.14.1)
    printf '%s\n' "${FAKE_PINNED_ZIG_BIN}"
    ;;
  github:rust-cross/cargo-zigbuild@0.22.3)
    printf '%s\n' "${FAKE_PINNED_ZIGBUILD_BIN}"
    ;;
  *)
    exit 1
    ;;
esac
"#,
        )?;
        write_executable(
            &pinned_zig_bin.join("zig"),
            "#!/bin/sh\nprintf '0.14.1\\n'\n",
        )?;
        write_executable(
            &pinned_zigbuild_bin.join("cargo-zigbuild"),
            "#!/bin/sh\nprintf 'cargo-zigbuild 0.22.3\\n'\n",
        )?;

        let system_path =
            std::env::var("PATH").map_err(|error| format!("PATH is unset: {error}"))?;
        let output = Command::new("bash")
            .arg(root().join("scripts/build-patched-openshell-supervisor.sh"))
            .arg("--check-prerequisites")
            .env("PATH", format!("{}:{system_path}", ambient_bin.display()))
            .env("FAKE_PINNED_ZIG_BIN", &pinned_zig_bin)
            .env("FAKE_PINNED_ZIGBUILD_BIN", &pinned_zigbuild_bin)
            .output()
            .map_err(|error| format!("failed to inspect cross-tool selection: {error}"))?;
        assert!(
            output.status.success(),
            "exact mise tools must be selected when ambient versions differ: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }

    #[test]
    fn supervisor_build_requires_openssl_before_expensive_work() -> Result<(), String> {
        let script_path = root().join("scripts/build-patched-openshell-supervisor.sh");
        let script = fs::read_to_string(&script_path)
            .map_err(|error| format!("failed to read {}: {error}", script_path.display()))?;
        let requirement = script
            .find("for command_name in cargo curl docker git openssl rustup;")
            .ok_or_else(|| {
                "the supervisor build must reject a missing OpenSSL before cloning or compiling"
                    .to_owned()
            })?;
        let source_checkout = script
            .find("git init --quiet")
            .ok_or_else(|| "the supervisor build must check out its pinned source".to_owned())?;
        assert!(
            requirement < source_checkout,
            "OpenSSL must be validated before the supervisor source checkout begins"
        );
        Ok(())
    }

    #[test]
    fn s0_e2e_does_not_require_identity_demo_jq() -> Result<(), String> {
        let script_path = root().join("scripts/s0-0-openshell-spike.sh");
        let script = fs::read_to_string(&script_path)
            .map_err(|error| format!("failed to read {}: {error}", script_path.display()))?;
        let common_requirements = script
            .split_once("for command in ")
            .and_then(|(_, remainder)| remainder.split_once("; do"))
            .map(|(commands, _)| commands)
            .ok_or_else(|| "the common S0 prerequisite set must be explicit".to_owned())?;
        assert!(
            !common_requirements
                .split_whitespace()
                .any(|command| command == "jq"),
            "the common S0 prerequisite set must not include identity-demo-only jq"
        );
        let jq_requirement = script
            .find("if [[ \"$#\" -eq 0 ]] && ! command -v jq")
            .ok_or_else(|| "the identity-demo mode must still require jq".to_owned())?;
        let image_build = script
            .find("build-patched-openshell-supervisor.sh\" --image-is-current")
            .ok_or_else(|| "the identity demo must validate its patched image".to_owned())?;
        let cluster_setup = script
            .find("kind create cluster")
            .ok_or_else(|| "the spike must retain its ephemeral cluster setup".to_owned())?;
        assert!(
            jq_requirement < image_build && jq_requirement < cluster_setup,
            "identity-demo-only jq must be checked before image build or cluster setup"
        );
        Ok(())
    }

    #[test]
    fn openshell_identity_spike_defaults_to_the_carried_supervisor_image() -> Result<(), String> {
        let output = Command::new("bash")
            .arg(root().join("scripts/s0-0-openshell-spike.sh"))
            .arg("--print-identity-supervisor-image")
            .output()
            .map_err(|error| format!("failed to inspect identity supervisor image: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "identity supervisor selection failed before cluster setup: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "openshell/supervisor:steward-spiffe-v0090",
            "the identity spike must use the supervisor built from the carried patch"
        );
        Ok(())
    }

    #[test]
    fn openshell_identity_spike_declares_its_spire_issuer_trust_bundle() -> Result<(), String> {
        let output = Command::new("bash")
            .arg(root().join("scripts/s0-0-openshell-spike.sh"))
            .arg("--print-spire-issuer-ca-configmap")
            .output()
            .map_err(|error| format!("failed to inspect SPIRE issuer trust bundle: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "SPIRE issuer trust selection failed before cluster setup: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "openshell-spire-oidc-ca",
            "the token issuer must consume a named run-scoped SPIRE CA ConfigMap"
        );
        Ok(())
    }

    #[test]
    fn openshell_spike_cleanup_never_updates_the_ambient_kubeconfig() -> Result<(), String> {
        let script_path = root().join("scripts/s0-0-openshell-spike.sh");
        let script = fs::read_to_string(&script_path)
            .map_err(|error| format!("failed to read {}: {error}", script_path.display()))?;
        assert!(
            script.contains(
                "KUBECONFIG=\"${KUBECONFIG_PATH}\" kind delete cluster --name \"${CLUSTER_NAME}\""
            ),
            "owned kind-cluster cleanup must use the run kubeconfig even on early interruption"
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
