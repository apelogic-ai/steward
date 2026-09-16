use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

const PER_WORKTREE_WARNING_BYTES: u64 = 15 * 1024 * 1024 * 1024;
const AGGREGATE_WARNING_BYTES: u64 = 60 * 1024 * 1024 * 1024;
const INACTIVE_WARNING_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const LEGACY_TARGETS: [&str; 2] = ["e2e/target", "conformance/target"];

#[derive(Default)]
struct DirectoryUsage {
    bytes: u64,
    newest: Option<SystemTime>,
}

#[derive(Debug, PartialEq)]
struct WorktreeEntry {
    path: PathBuf,
    prunable: bool,
}

pub(crate) fn check(repository: &Path) -> Result<(), String> {
    validate_cargo_config(repository)?;
    validate_target_override(repository)?;
    validate_root_target(repository)?;

    let legacy_targets = LEGACY_TARGETS
        .iter()
        .map(|relative| repository.join(relative))
        .filter(|path| path.exists())
        .collect::<Vec<_>>();
    if !legacy_targets.is_empty() {
        return Err(format!(
            "storage policy rejected legacy Cargo artifact directories:\n{}\nremove only these generated directories, then retry",
            legacy_targets
                .iter()
                .map(|path| format!("  {}", path.display()))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }

    println!(
        "storage check: root, E2E, and conformance builds use {}",
        repository.join("target").display()
    );
    Ok(())
}

pub(crate) fn audit(repository: &Path) -> Result<(), String> {
    validate_cargo_config(repository)?;
    validate_target_override(repository)?;
    validate_root_target(repository)?;

    let worktrees = worktree_paths(repository)?;
    let primary_worktree = worktrees
        .first()
        .map(|entry| entry.path.as_path())
        .ok_or_else(|| "git worktree list returned no Steward worktrees".to_owned())?;
    let now = SystemTime::now();
    let mut aggregate_bytes = 0_u64;
    let mut aggregate_legacy_bytes = 0_u64;
    let mut active_worktrees = 0_usize;
    let mut stale_worktrees = 0_usize;
    let mut warnings = Vec::new();

    println!("storage audit:");
    for entry in &worktrees {
        let worktree = &entry.path;
        if entry.prunable || !worktree.is_dir() {
            stale_worktrees += 1;
            println!(
                "  {} | stale metadata | target unknown | legacy unknown | activity unknown",
                worktree.display()
            );
            warnings.push(format!(
                "{} has stale worktree metadata; inspect it and run `git worktree prune` when safe",
                worktree.display()
            ));
            if !worktree_location_is_allowed(primary_worktree, worktree) {
                warnings.push(format!(
                    "{} is outside {}/.worktrees",
                    worktree.display(),
                    primary_worktree.display()
                ));
            }
            continue;
        }
        active_worktrees += 1;
        let target = directory_usage(&worktree.join("target"))?;
        let mut legacy_bytes = 0_u64;
        let mut newest = target.newest;
        let mut legacy_paths = Vec::new();
        for relative in LEGACY_TARGETS {
            let path = worktree.join(relative);
            let usage = directory_usage(&path)?;
            if usage.bytes > 0 || path.exists() {
                legacy_paths.push(path);
            }
            legacy_bytes = legacy_bytes.saturating_add(usage.bytes);
            newest = newest_time(newest, usage.newest);
        }

        let total = target.bytes.saturating_add(legacy_bytes);
        aggregate_bytes = aggregate_bytes.saturating_add(total);
        aggregate_legacy_bytes = aggregate_legacy_bytes.saturating_add(legacy_bytes);
        let state = worktree_state(worktree)?;
        let activity = activity_age(now, newest);
        println!(
            "  {} | {state} | target {} | legacy {} | activity {activity}",
            worktree.display(),
            human_bytes(target.bytes),
            human_bytes(legacy_bytes),
        );

        if !worktree_location_is_allowed(primary_worktree, worktree) {
            warnings.push(format!(
                "{} is outside {}/.worktrees",
                worktree.display(),
                primary_worktree.display()
            ));
        }

        if total > PER_WORKTREE_WARNING_BYTES {
            warnings.push(format!(
                "{} uses {}, above the 15 GiB per-worktree budget",
                worktree.display(),
                human_bytes(total)
            ));
        }
        if !legacy_paths.is_empty() {
            warnings.push(format!(
                "{} contains legacy targets: {}",
                worktree.display(),
                legacy_paths
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if total > 0 && is_inactive(now, newest) {
            warnings.push(format!(
                "{} has build artifacts with no activity in at least seven days",
                worktree.display()
            ));
        }
    }

    println!(
        "storage audit: {active_worktrees} active and {stale_worktrees} stale worktrees use {} in Cargo artifacts ({} legacy)",
        human_bytes(aggregate_bytes),
        human_bytes(aggregate_legacy_bytes)
    );
    if aggregate_bytes > AGGREGATE_WARNING_BYTES {
        warnings.push(format!(
            "Steward uses {}, above the 60 GiB aggregate budget",
            human_bytes(aggregate_bytes)
        ));
    }
    for warning in warnings {
        eprintln!("warning: {warning}");
    }

    Ok(())
}

fn validate_cargo_config(repository: &Path) -> Result<(), String> {
    let path = repository.join(".cargo/config.toml");
    let content = fs::read_to_string(&path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    validate_cargo_config_content(&content).map_err(|error| format!("{}: {error}", path.display()))
}

fn validate_cargo_config_content(content: &str) -> Result<(), String> {
    let config = toml::from_str::<toml::Table>(content)
        .map_err(|error| format!("Cargo configuration is invalid TOML: {error}"))?;
    let target_dir = config
        .get("build")
        .and_then(|build| build.get("target-dir"))
        .and_then(toml::Value::as_str);
    if target_dir == Some("target") {
        Ok(())
    } else {
        Err(
            "build.target-dir must be exactly `target` so each worktree owns one artifact root"
                .to_owned(),
        )
    }
}

fn validate_target_override(repository: &Path) -> Result<(), String> {
    let expected = repository.join("target");
    let overrides = invalid_target_override_names(
        &expected,
        env::var_os("CARGO_TARGET_DIR").as_deref(),
        env::var_os("CARGO_BUILD_TARGET_DIR").as_deref(),
    );
    if overrides.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "unset {} or set it to {}; Steward builds cannot leave their worktree target",
            overrides.join(" and "),
            expected.display()
        ))
    }
}

fn invalid_target_override_names(
    expected: &Path,
    cargo_target_dir: Option<&OsStr>,
    cargo_build_target_dir: Option<&OsStr>,
) -> Vec<&'static str> {
    [
        ("CARGO_TARGET_DIR", cargo_target_dir),
        ("CARGO_BUILD_TARGET_DIR", cargo_build_target_dir),
    ]
    .into_iter()
    .filter_map(|(name, value)| {
        value
            .is_some_and(|value| Path::new(value) != expected)
            .then_some(name)
    })
    .collect()
}

fn validate_root_target(repository: &Path) -> Result<(), String> {
    let target = repository.join("target");
    let metadata = match fs::symlink_metadata(&target) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "failed to inspect Cargo target {}: {error}",
                target.display()
            ));
        }
    };
    if metadata.file_type().is_symlink() {
        Err(format!(
            "Cargo target {} must not be a symlink outside its worktree",
            target.display()
        ))
    } else if metadata.is_dir() {
        Ok(())
    } else {
        Err(format!(
            "Cargo target {} exists but is not a directory",
            target.display()
        ))
    }
}

fn worktree_paths(repository: &Path) -> Result<Vec<WorktreeEntry>, String> {
    let output = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(repository)
        .output()
        .map_err(|error| format!("failed to list Steward worktrees: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git worktree list exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let paths = parse_worktree_paths(&String::from_utf8_lossy(&output.stdout));
    if paths.is_empty() {
        Err("git worktree list returned no Steward worktrees".to_owned())
    } else {
        Ok(paths)
    }
}

fn parse_worktree_paths(output: &str) -> Vec<WorktreeEntry> {
    output
        .split("\n\n")
        .filter_map(|record| {
            let path = record
                .lines()
                .find_map(|line| line.strip_prefix("worktree "))?;
            Some(WorktreeEntry {
                path: PathBuf::from(path),
                prunable: record.lines().any(|line| line.starts_with("prunable ")),
            })
        })
        .collect()
}

fn worktree_location_is_allowed(primary: &Path, candidate: &Path) -> bool {
    candidate == primary || candidate.starts_with(primary.join(".worktrees"))
}

fn worktree_state(worktree: &Path) -> Result<&'static str, String> {
    let output = Command::new("git")
        .args(["status", "--porcelain=v1", "--untracked-files=normal"])
        .current_dir(worktree)
        .output()
        .map_err(|error| format!("failed to inspect {}: {error}", worktree.display()))?;
    if !output.status.success() {
        return Err(format!(
            "git status failed for {} with {}: {}",
            worktree.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    if output.stdout.is_empty() {
        Ok("clean")
    } else {
        Ok("dirty")
    }
}

fn directory_usage(path: &Path) -> Result<DirectoryUsage, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DirectoryUsage::default());
        }
        Err(error) => return Err(format!("failed to inspect {}: {error}", path.display())),
    };
    if metadata.file_type().is_symlink() {
        return Ok(DirectoryUsage::default());
    }

    let mut usage = DirectoryUsage {
        bytes: allocated_bytes(&metadata),
        newest: metadata.modified().ok(),
    };
    if !metadata.is_dir() {
        return Ok(usage);
    }

    let mut pending = vec![path.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let entries = fs::read_dir(&directory)
            .map_err(|error| format!("failed to read {}: {error}", directory.display()))?;
        for entry in entries {
            let entry = entry
                .map_err(|error| format!("failed to read {}: {error}", directory.display()))?;
            let metadata = fs::symlink_metadata(entry.path()).map_err(|error| {
                format!("failed to inspect {}: {error}", entry.path().display())
            })?;
            if metadata.file_type().is_symlink() {
                continue;
            }
            usage.bytes = usage.bytes.saturating_add(allocated_bytes(&metadata));
            usage.newest = newest_time(usage.newest, metadata.modified().ok());
            if metadata.is_dir() {
                pending.push(entry.path());
            }
        }
    }
    Ok(usage)
}

#[cfg(unix)]
fn allocated_bytes(metadata: &fs::Metadata) -> u64 {
    metadata.blocks().saturating_mul(512)
}

#[cfg(not(unix))]
fn allocated_bytes(metadata: &fs::Metadata) -> u64 {
    metadata.len()
}

fn newest_time(left: Option<SystemTime>, right: Option<SystemTime>) -> Option<SystemTime> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (Some(time), None) | (None, Some(time)) => Some(time),
        (None, None) => None,
    }
}

fn is_inactive(now: SystemTime, newest: Option<SystemTime>) -> bool {
    newest
        .and_then(|time| now.duration_since(time).ok())
        .is_some_and(|age| age >= INACTIVE_WARNING_AGE)
}

fn activity_age(now: SystemTime, newest: Option<SystemTime>) -> String {
    let Some(age) = newest.and_then(|time| now.duration_since(time).ok()) else {
        return "none".to_owned();
    };
    let hours = age.as_secs() / 3600;
    if hours < 1 {
        "<1h".to_owned()
    } else if hours < 24 {
        format!("{hours}h")
    } else {
        format!("{}d", hours / 24)
    }
}

fn human_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let bytes = bytes as f64;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes / GIB)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes / MIB)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes / KIB)
    } else {
        format!("{bytes:.0} B")
    }
}

#[cfg(test)]
mod tests {
    use super::{
        WorktreeEntry, human_bytes, invalid_target_override_names, is_inactive,
        parse_worktree_paths, validate_cargo_config_content, worktree_location_is_allowed,
    };
    use std::ffi::OsStr;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    #[test]
    fn cargo_config_requires_one_worktree_relative_target() {
        assert!(
            validate_cargo_config_content(
                "[alias]\nxtask = \"run --package xtask --\"\n[build]\ntarget-dir = \"target\"\n"
            )
            .is_ok()
        );
        assert!(
            validate_cargo_config_content("[build]\ntarget-dir = \"../shared-target\"\n").is_err()
        );
        assert!(validate_cargo_config_content("[alias]\nxtask = \"run\"\n").is_err());
    }

    #[test]
    fn worktree_porcelain_parser_preserves_paths() {
        let output = concat!(
            "worktree /workspace/steward\nHEAD abc\nbranch refs/heads/main\n\n",
            "worktree /workspace/steward/.worktrees/feature one\nHEAD def\ndetached\n",
            "prunable gitdir file points to non-existent location\n"
        );
        assert_eq!(
            parse_worktree_paths(output),
            vec![
                WorktreeEntry {
                    path: PathBuf::from("/workspace/steward"),
                    prunable: false,
                },
                WorktreeEntry {
                    path: PathBuf::from("/workspace/steward/.worktrees/feature one"),
                    prunable: true,
                }
            ]
        );
    }

    #[test]
    fn worktree_locations_are_repository_local() {
        let primary = PathBuf::from("/workspace/steward");
        assert!(worktree_location_is_allowed(&primary, &primary));
        assert!(worktree_location_is_allowed(
            &primary,
            &primary.join(".worktrees/feature")
        ));
        assert!(!worktree_location_is_allowed(
            &primary,
            &PathBuf::from("/private/tmp/steward-feature")
        ));
    }

    #[test]
    fn cargo_target_environment_overrides_are_rejected() {
        let expected = PathBuf::from("/workspace/steward/target");
        assert!(invalid_target_override_names(&expected, None, None).is_empty());
        assert!(
            invalid_target_override_names(
                &expected,
                Some(OsStr::new("/workspace/steward/target")),
                None
            )
            .is_empty()
        );
        assert_eq!(
            invalid_target_override_names(&expected, Some(OsStr::new("/tmp/shared")), None),
            vec!["CARGO_TARGET_DIR"]
        );
        assert_eq!(
            invalid_target_override_names(
                &expected,
                Some(OsStr::new("target")),
                Some(OsStr::new("target"))
            ),
            vec!["CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR"]
        );
    }

    #[test]
    fn inactivity_threshold_is_seven_days() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10 * 24 * 60 * 60);
        assert!(is_inactive(
            now,
            Some(now - Duration::from_secs(7 * 24 * 60 * 60))
        ));
        assert!(!is_inactive(
            now,
            Some(now - Duration::from_secs(6 * 24 * 60 * 60))
        ));
        assert!(!is_inactive(now, None));
    }

    #[test]
    fn byte_counts_are_human_readable() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(2 * 1024 * 1024 * 1024), "2.0 GiB");
    }
}
