use std::collections::BTreeSet;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use steward_adapter_fake::IMPLEMENTED_PORTS as FAKE_PORTS;
use steward_ports::{Maturity, PORTS};
use xtask::{
    local_test_context_is_safe, migration_history_violations, neutrality_violations,
    secret_violations, validate_register_content,
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
        "policy-test" if rest.is_empty() => policy_test(),
        "migrate-check" if rest.is_empty() => migrate_check(),
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
        "  policy-test",
        "  migrate-check",
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
    run("cargo", &["fmt", "--all", "--", "--check"])?;
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
    conformance(&["--pinned".to_owned()])?;
    register(&["--check".to_owned()])?;
    ports_check()?;
    layering_test()
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
    let base = env::var("STEWARD_MIGRATION_BASE").unwrap_or_else(|_| "origin/main".to_owned());
    let range = format!("{base}...HEAD");
    let output = Command::new("git")
        .args([
            "diff",
            "--name-status",
            "--find-renames",
            &range,
            "--",
            ":(glob)migrations/*.sql",
        ])
        .current_dir(root())
        .output()
        .map_err(|error| format!("failed to inspect migration history: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git could not compare migration history against {base}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let changes = String::from_utf8(output.stdout)
        .map_err(|_| "git returned non-UTF-8 migration paths".to_owned())?;
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

fn verify_manifests() -> TaskResult {
    let directory = root().join("manifests");
    ensure_directory(&directory)?;
    let generated = files_with_extension(&directory, "yaml")?;
    if !generated.is_empty() {
        return Err(
            "generated manifests exist before steward-types provides the S0 CRD generator"
                .to_owned(),
        );
    }
    println!("verify-manifests: no generated CRD is expected before S0");
    Ok(())
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
    println!(
        "conformance {}: suite is introduced by prerequisite S0.0; register shape is valid",
        arguments[0]
    );
    Ok(())
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
    validate_register_content(&content)
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
    write_crate(directory, "vendor", "openshell-client", "")?;
    write_crate(
        directory,
        "adapter",
        "steward-adapter-openshell",
        "openshell-client = { path = \"../vendor\", version = \"=0.0.0\" }",
    )?;
    if include_violation {
        write_crate(
            directory,
            "core",
            "steward-controller",
            "openshell-client = { path = \"../vendor\", version = \"=0.0.0\" }",
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
