//! Cross-product conformance for the runner-facing Task output archive.
//!
//! Generated GitHub Actions callers pin steward-run 0.8.1 (tag `v0.8.1`, commit
//! `26200e28d078e46db7b29fb29c6b48383e09dbe1`). Its `extractOutputArchive` (archive module)
//! decodes `GET /v1/tasks/{taskUid}/outputs` with tar-stream 3.1.7 and, when the authenticated
//! Task status reports `diagnostics.executionLog: full`, requires the reserved execution
//! transcript inside that archive. Steward 0.3.11 delivered an `out/`-only archive to those
//! callers and every such run failed after its Task succeeded.
//!
//! steward-run itself is not available to this workspace's CI, so this file carries a model of
//! the acceptance rules from that exact revision: tar-stream header decoding (checksum,
//! ustar/GNU magic, prefix, numeric fields, typeflag, GNU long
//! paths, PAX `path`/`size` with global-header merging, and no data consumption for directory
//! entries), then the reserved-diagnostics, declared-output, ancestor, duplicate, entry-type,
//! and 4 MiB per-stream rules of `extractOutputArchive`.
//!
//! Known omissions: filesystem effects while writing outputs (symbolic-link and non-directory
//! checks belong to the runner workspace, not the archive), base-256 numeric fields (rejected
//! here, and by Steward), lossy decoding of non-UTF-8 names (rejected here, and by Steward), and
//! tar-stream's `NaN` for non-octal numeric fields. Update this model only from steward-run's
//! released source.

use std::collections::BTreeMap;

use steward_types::direct_package::{
    EXECUTION_STDERR_ARCHIVE_PATH, EXECUTION_STDOUT_ARCHIVE_PATH, MAX_EXECUTION_STREAM_BYTES,
};
use steward_types::task_output_archive::{
    InvalidTaskOutputArchive, TaskOutputTranscriptError,
    task_output_archive_with_execution_transcript,
};

const DIAGNOSTICS_ROOT: &str = ".steward/diagnostics";
const STDOUT_PATH: &str = ".steward/diagnostics/stdout.log";
const STDERR_PATH: &str = ".steward/diagnostics/stderr.log";
const TRANSCRIPT_LIMIT: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExecutionLog {
    Off,
    Full,
}

#[derive(Debug, Default, Eq, PartialEq)]
struct Extraction {
    written_files: Vec<String>,
    transcript: Option<(Vec<u8>, Vec<u8>)>,
}

#[derive(Debug)]
struct DecodedHeader {
    name: String,
    kind: Option<&'static str>,
    size: usize,
}

/// tar-stream 3.1.7 `decodeOct`: skip leading spaces, stop at the first space, skip leading
/// NULs, then `parseInt(..., 8)`, which stops at the first non-octal character.
fn decode_oct(field: &[u8]) -> Result<usize, String> {
    if field.first().is_some_and(|byte| byte & 0x80 != 0) {
        return Err("base-256 numeric fields are outside this model".to_owned());
    }
    let mut start = 0;
    while start < field.len() && field[start] == b' ' {
        start += 1;
    }
    let end = field[start..]
        .iter()
        .position(|byte| *byte == b' ')
        .map_or(field.len(), |position| start + position);
    while start < end && field[start] == 0 {
        start += 1;
    }
    let digits = field[start..end]
        .iter()
        .take_while(|byte| (b'0'..=b'7').contains(byte))
        .map(|byte| char::from(*byte))
        .collect::<String>();
    if digits.is_empty() {
        return Ok(0);
    }
    usize::from_str_radix(&digits, 8).map_err(|error| error.to_string())
}

fn decode_str(field: &[u8]) -> Result<String, String> {
    let end = field
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(field.len());
    String::from_utf8(field[..end].to_vec()).map_err(|error| error.to_string())
}

/// tar-stream 3.1.7 header `decode` (`headers` module); `Ok(None)` is its all-zero "null header".
fn decode_header(block: &[u8]) -> Result<Option<DecodedHeader>, String> {
    let typeflag = if block[156] == 0 {
        0_i32
    } else {
        i32::from(block[156]) - i32::from(b'0')
    };
    let mut name = decode_str(&block[..100])?;
    let size = decode_oct(&block[124..136])?;
    let checksum = 8 * 32
        + block[..148]
            .iter()
            .chain(&block[156..])
            .map(|byte| usize::from(*byte))
            .sum::<usize>();
    if checksum == 8 * 32 {
        return Ok(None);
    }
    if checksum != decode_oct(&block[148..156])? {
        return Err("Invalid tar header. Maybe the tar is corrupted".to_owned());
    }
    if &block[257..263] == b"ustar\0" {
        if block[345] != 0 {
            name = format!("{}/{name}", decode_str(&block[345..500])?);
        }
    } else if !(&block[257..263] == b"ustar " && &block[263..265] == b" \0") {
        return Err("Invalid tar header: unknown format.".to_owned());
    }
    // tar-stream 3.1.7 computes `type` before its trailing-slash directory fix-up and never
    // recomputes it, so a regular-file header named with a trailing slash stays a file.
    let kind = match typeflag {
        0 => Some("file"),
        1 => Some("link"),
        2 => Some("symlink"),
        3 => Some("character-device"),
        4 => Some("block-device"),
        5 => Some("directory"),
        6 => Some("fifo"),
        7 => Some("contiguous-file"),
        72 => Some("pax-header"),
        55 => Some("pax-global-header"),
        27 => Some("gnu-long-link-path"),
        28 | 30 => Some("gnu-long-path"),
        _ => None,
    };
    Ok(Some(DecodedHeader { name, kind, size }))
}

/// tar-stream 3.1.7 `decodePax` (`headers` module).
fn decode_pax(data: &[u8]) -> BTreeMap<String, String> {
    let mut result = BTreeMap::new();
    let mut remaining = data;
    while !remaining.is_empty() {
        let space = remaining
            .iter()
            .position(|byte| *byte == b' ')
            .unwrap_or(remaining.len());
        let length = String::from_utf8_lossy(&remaining[..space])
            .parse::<usize>()
            .unwrap_or(0);
        if length == 0 || length > remaining.len() || space + 1 > length - 1 {
            return result;
        }
        let record = String::from_utf8_lossy(&remaining[space + 1..length - 1]).into_owned();
        let Some((key, value)) = record.split_once('=') else {
            return result;
        };
        result.insert(key.to_owned(), value.to_owned());
        remaining = &remaining[length..];
    }
    result
}

/// steward-run 0.8.1 `normalizeWorkspacePath` for declared output paths.
fn normalize_workspace_path(value: &str) -> Result<String, String> {
    let mut candidate = value.trim();
    while let Some(rest) = candidate.strip_prefix("./") {
        candidate = rest;
    }
    if candidate.is_empty()
        || candidate == "."
        || candidate.contains('\\')
        || candidate.contains('\0')
        || candidate.starts_with('/')
    {
        return Err(format!("workspace-relative path is invalid: {value:?}"));
    }
    let normalized = posix_normalize(candidate);
    if normalized == ".." || normalized.starts_with("../") {
        return Err(format!("workspace-relative path is invalid: {value:?}"));
    }
    Ok(normalized)
}

/// Node `normalize` from `path` (POSIX) for relative paths (trailing slash preserved).
fn posix_normalize(value: &str) -> String {
    let trailing = value.ends_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for component in value.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|last| *last != "..") {
                    parts.pop();
                } else {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    let mut normalized = parts.join("/");
    if normalized.is_empty() {
        normalized = ".".to_owned();
    }
    if trailing && normalized != "." {
        normalized.push('/');
    }
    normalized
}

/// steward-run 0.8.1 `normalizeArchivePath`.
fn normalize_archive_path(value: &str) -> Result<String, String> {
    if value.is_empty() || value.contains('\\') || value.contains('\0') || value.starts_with('/') {
        return Err(format!("unsafe archive path: {value:?}"));
    }
    let normalized = posix_normalize(value.strip_suffix('/').unwrap_or(value));
    if normalized.is_empty()
        || normalized == "."
        || normalized == ".."
        || normalized.starts_with("../")
    {
        return Err(format!("unsafe archive path: {value:?}"));
    }
    Ok(normalized)
}

fn has_canonical_reserved_spelling(archive_path: &str, normalized: &str, kind: &str) -> bool {
    let without_root = archive_path.strip_prefix("./").unwrap_or(archive_path);
    let candidate = if kind == "directory" {
        without_root.strip_suffix('/').unwrap_or(without_root)
    } else {
        without_root
    };
    candidate == normalized
}

/// Model of steward-run 0.8.1 `extractOutputArchive`.
fn steward_run_v0_8_1_extract(
    archive: &[u8],
    declared_paths: &[&str],
    execution_log: ExecutionLog,
) -> Result<Extraction, String> {
    let outputs = declared_paths
        .iter()
        .map(|path| normalize_workspace_path(path))
        .collect::<Result<Vec<_>, _>>()?;
    let mut seen = std::collections::BTreeSet::new();
    let mut saw_root_directory = false;
    let mut extraction = Extraction::default();
    let (mut stdout, mut stderr) = (None, None);
    let mut gnu_long_path: Option<String> = None;
    let mut pax: Option<BTreeMap<String, String>> = None;
    let mut pax_global: Option<BTreeMap<String, String>> = None;
    let mut offset = 0;
    while offset + 512 <= archive.len() {
        let block = &archive[offset..offset + 512];
        offset += 512;
        let Some(header) = decode_header(block)? else {
            continue;
        };
        let kind = header.kind.unwrap_or("unknown");
        if matches!(
            kind,
            "pax-header" | "pax-global-header" | "gnu-long-path" | "gnu-long-link-path"
        ) {
            let data_end = offset + header.size;
            if data_end > archive.len() {
                return Err("Unexpected end of data".to_owned());
            }
            let data = &archive[offset..data_end];
            offset += header.size.div_ceil(512) * 512;
            match kind {
                "pax-header" => {
                    let mut merged = pax_global.clone().unwrap_or_default();
                    merged.extend(decode_pax(data));
                    pax = Some(merged);
                }
                "pax-global-header" => pax_global = Some(decode_pax(data)),
                "gnu-long-path" => gnu_long_path = Some(decode_str(data)?),
                _ => {}
            }
            continue;
        }
        // `_applyLongHeaders`: the GNU long path first, then PAX path and size override.
        let mut name = gnu_long_path.take().unwrap_or(header.name);
        let mut size = header.size;
        if let Some(attributes) = pax.take() {
            if let Some(path) = attributes.get("path").filter(|path| !path.is_empty()) {
                name.clone_from(path);
            }
            if let Some(value) = attributes.get("size") {
                size = value.parse::<usize>().unwrap_or(0);
            }
        }
        // tar-stream consumes no data for an empty entry or any directory entry.
        let data = if size == 0 || kind == "directory" {
            &archive[offset..offset]
        } else {
            let data_end = offset + size;
            if data_end > archive.len() {
                return Err("Unexpected end of data".to_owned());
            }
            let data = &archive[offset..data_end];
            offset += size.div_ceil(512) * 512;
            data
        };

        if name == "./" && kind == "directory" {
            if saw_root_directory {
                return Err("duplicate archive entry: ./".to_owned());
            }
            saw_root_directory = true;
            continue;
        }
        let relative = normalize_archive_path(&name)?;
        let is_diagnostics =
            relative == DIAGNOSTICS_ROOT || relative.starts_with(&format!("{DIAGNOSTICS_ROOT}/"));
        if is_diagnostics || (execution_log == ExecutionLog::Full && relative == ".steward") {
            if execution_log != ExecutionLog::Full {
                return Err("reserved diagnostics require full execution logging".to_owned());
            }
            if !has_canonical_reserved_spelling(&name, &relative, kind) {
                return Err("reserved diagnostics archive path is not canonical".to_owned());
            }
            if !seen.insert(relative.clone()) {
                return Err(format!("duplicate archive entry: {relative}"));
            }
            if relative == ".steward" || relative == DIAGNOSTICS_ROOT {
                if kind != "directory" {
                    return Err(format!(
                        "reserved diagnostics ancestor must be a directory: {relative}"
                    ));
                }
            } else if relative == STDOUT_PATH || relative == STDERR_PATH {
                if kind != "file" {
                    return Err(format!(
                        "reserved execution transcript must be a file: {relative}"
                    ));
                }
                if data.len() > TRANSCRIPT_LIMIT {
                    return Err(format!(
                        "execution transcript exceeds {TRANSCRIPT_LIMIT} bytes: {relative}"
                    ));
                }
                if relative == STDOUT_PATH {
                    stdout = Some(data.to_vec());
                } else {
                    stderr = Some(data.to_vec());
                }
            } else {
                return Err(format!("unknown reserved diagnostics path: {relative}"));
            }
            continue;
        }
        let is_declared = outputs
            .iter()
            .any(|root| relative == *root || relative.starts_with(&format!("{root}/")));
        let is_ancestor = outputs
            .iter()
            .any(|output| output.starts_with(&format!("{relative}/")));
        if !is_declared && !is_ancestor {
            return Err(format!("archive path is not a declared output: {relative}"));
        }
        if !seen.insert(relative.clone()) {
            return Err(format!("duplicate archive entry: {relative}"));
        }
        if !is_declared && kind != "directory" {
            return Err(format!(
                "archive ancestor entry type is not allowed: {kind}"
            ));
        }
        match kind {
            "directory" => {}
            "file" => extraction.written_files.push(relative),
            other => return Err(format!("archive entry type is not allowed: {other}")),
        }
    }
    if execution_log == ExecutionLog::Full {
        match (stdout, stderr) {
            (Some(stdout), Some(stderr)) => extraction.transcript = Some((stdout, stderr)),
            _ => return Err("missing reserved execution transcript".to_owned()),
        }
    }
    Ok(extraction)
}

/// GNU tar 1.35 `tar -cf - -C /sandbox/steward-output out` header layout ("ustar  \0").
fn gnu_entry(archive: &mut Vec<u8>, path: &str, content: &[u8], kind: u8) {
    raw_entry(archive, path, content, kind, b"ustar  \0");
}

fn ustar_entry(archive: &mut Vec<u8>, path: &str, content: &[u8], kind: u8) {
    raw_entry(archive, path, content, kind, b"ustar\x0000");
}

fn raw_entry(archive: &mut Vec<u8>, path: &str, content: &[u8], kind: u8, magic: &[u8; 8]) {
    let header = archive.len();
    archive.resize(header + 512, 0);
    archive[header..header + path.len()].copy_from_slice(path.as_bytes());
    let mode: &[u8] = if kind == b'5' {
        b"0000755\0"
    } else {
        b"0000644\0"
    };
    archive[header + 100..header + 108].copy_from_slice(mode);
    archive[header + 108..header + 116].copy_from_slice(b"0001750\0");
    archive[header + 116..header + 124].copy_from_slice(b"0001750\0");
    archive[header + 124..header + 136]
        .copy_from_slice(format!("{:011o}\0", content.len()).as_bytes());
    archive[header + 136..header + 148].copy_from_slice(b"15073551234\0");
    archive[header + 148..header + 156].fill(b' ');
    archive[header + 156] = kind;
    archive[header + 257..header + 265].copy_from_slice(magic);
    let checksum = archive[header..header + 512]
        .iter()
        .map(|byte| usize::from(*byte))
        .sum::<usize>();
    archive[header + 148..header + 156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
    archive.extend_from_slice(content);
    archive.resize(header + 512 + content.len().div_ceil(512) * 512, 0);
}

/// Rewrite one header in place and refresh its checksum.
fn patch_header(archive: &mut [u8], offset: usize, patch: impl Fn(&mut [u8])) {
    let header = &mut archive[offset..offset + 512];
    patch(header);
    header[148..156].fill(b' ');
    let checksum = header.iter().map(|byte| usize::from(*byte)).sum::<usize>();
    header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
}

/// A stored `steward.task-output/v1` archive as the OpenShell adapter produces it, padded to
/// GNU tar's 10 KiB record.
fn stored_out_archive() -> Vec<u8> {
    let mut archive = Vec::new();
    gnu_entry(&mut archive, "out/", b"", b'5');
    gnu_entry(&mut archive, "out/reports/", b"", b'5');
    gnu_entry(&mut archive, "out/reports/summary.md", b"# Summary\n", b'0');
    gnu_entry(&mut archive, "out/result.txt", b"complete\n", b'0');
    archive.resize((archive.len() + 1024).div_ceil(10240) * 10240, 0);
    archive
}

fn deliver(stdout: &[u8], stderr: &[u8]) -> Result<Vec<u8>, String> {
    task_output_archive_with_execution_transcript(&stored_out_archive(), stdout, stderr)
        .map_err(|error| format!("runner archive was not produced: {error:?}"))
}

#[test]
fn full_execution_log_delivery_is_accepted_by_steward_run_0_8_1() -> Result<(), String> {
    let delivered = deliver(b"agent stdout\n", b"agent stderr\n")?;
    let extraction = steward_run_v0_8_1_extract(&delivered, &["out"], ExecutionLog::Full)?;
    assert_eq!(
        extraction.written_files,
        ["out/reports/summary.md", "out/result.txt"]
    );
    assert_eq!(
        extraction.transcript,
        Some((b"agent stdout\n".to_vec(), b"agent stderr\n".to_vec()))
    );

    let narrower = steward_run_v0_8_1_extract(&delivered, &["out/reports"], ExecutionLog::Full);
    assert_eq!(
        narrower.err().as_deref(),
        Some("archive path is not a declared output: out/result.txt"),
        "the transcript must not loosen declared-output enforcement"
    );
    Ok(())
}

#[test]
fn empty_and_maximum_streams_are_accepted() -> Result<(), String> {
    let delivered = deliver(b"", b"")?;
    let extraction = steward_run_v0_8_1_extract(&delivered, &["out"], ExecutionLog::Full)?;
    assert_eq!(extraction.transcript, Some((Vec::new(), Vec::new())));

    let maximum =
        vec![b'x'; usize::try_from(MAX_EXECUTION_STREAM_BYTES).map_err(|e| e.to_string())?];
    let delivered = deliver(&maximum, &maximum)?;
    let extraction = steward_run_v0_8_1_extract(&delivered, &["out"], ExecutionLog::Full)?;
    assert_eq!(extraction.transcript, Some((maximum.clone(), maximum)));
    Ok(())
}

#[test]
fn oversized_transcripts_fail_closed_before_delivery() -> Result<(), String> {
    let limit = usize::try_from(MAX_EXECUTION_STREAM_BYTES).map_err(|e| e.to_string())?;
    let oversized = vec![b'x'; limit + 1];
    for (stdout, stderr) in [(&oversized[..], &b""[..]), (&b""[..], &oversized[..])] {
        assert_eq!(
            task_output_archive_with_execution_transcript(&stored_out_archive(), stdout, stderr),
            Err(TaskOutputTranscriptError::TranscriptTooLarge)
        );
    }
    Ok(())
}

#[test]
fn the_0_3_11_out_only_delivery_fails_full_execution_log_callers() -> Result<(), String> {
    assert_eq!(
        steward_run_v0_8_1_extract(&stored_out_archive(), &["out"], ExecutionLog::Full)
            .err()
            .as_deref(),
        Some("missing reserved execution transcript")
    );
    Ok(())
}

#[test]
fn execution_log_off_callers_still_require_an_out_only_archive() -> Result<(), String> {
    let extraction =
        steward_run_v0_8_1_extract(&stored_out_archive(), &["out"], ExecutionLog::Off)?;
    assert_eq!(
        extraction,
        Extraction {
            written_files: vec![
                "out/reports/summary.md".to_owned(),
                "out/result.txt".to_owned()
            ],
            transcript: None,
        }
    );
    let delivered = deliver(b"stdout", b"stderr")?;
    assert_eq!(
        steward_run_v0_8_1_extract(&delivered, &["out"], ExecutionLog::Off)
            .err()
            .as_deref(),
        Some("reserved diagnostics require full execution logging"),
        "a transcript must never reach a caller that did not request it"
    );
    Ok(())
}

#[test]
fn agent_output_cannot_create_or_replace_the_reserved_namespace() -> Result<(), String> {
    for (path, kind) in [
        (EXECUTION_STDOUT_ARCHIVE_PATH, b'0'),
        (EXECUTION_STDERR_ARCHIVE_PATH, b'0'),
        (".steward/", b'5'),
        (".steward/diagnostics/", b'5'),
        (".steward/diagnostics/trace.log", b'0'),
        ("out/../.steward/diagnostics/stdout.log", b'0'),
    ] {
        let mut forged = Vec::new();
        gnu_entry(&mut forged, "out/", b"", b'5');
        gnu_entry(&mut forged, "out/result.txt", b"complete\n", b'0');
        gnu_entry(&mut forged, path, b"forged\n", kind);
        forged.resize(forged.len() + 1024, 0);
        assert!(
            matches!(
                task_output_archive_with_execution_transcript(&forged, b"stdout", b"stderr"),
                Err(TaskOutputTranscriptError::Archive(
                    InvalidTaskOutputArchive::Malformed
                ))
            ),
            "agent output must not supply {path}"
        );
    }
    Ok(())
}

/// One appended tar entry: path, content, and typeflag.
type ForgedEntry<'a> = (&'a str, &'a [u8], u8);

#[test]
fn the_model_rejects_the_shapes_steward_run_0_8_1_rejects() -> Result<(), String> {
    let mut base = Vec::new();
    gnu_entry(&mut base, "out/", b"", b'5');
    gnu_entry(&mut base, "out/result.txt", b"complete\n", b'0');
    let cases: [(&[ForgedEntry], &str); 6] = [
        (
            &[
                (STDOUT_PATH, b"a", b'0'),
                (STDOUT_PATH, b"b", b'0'),
                (STDERR_PATH, b"", b'0'),
            ],
            "duplicate archive entry: .steward/diagnostics/stdout.log",
        ),
        (
            &[
                (".steward//diagnostics/stdout.log", b"a", b'0'),
                (STDERR_PATH, b"", b'0'),
            ],
            "reserved diagnostics archive path is not canonical",
        ),
        (
            &[
                (".steward/diagnostics/stdout.log/", b"", b'5'),
                (STDERR_PATH, b"", b'0'),
            ],
            "reserved execution transcript must be a file: .steward/diagnostics/stdout.log",
        ),
        (
            &[(".steward", b"", b'0')],
            "reserved diagnostics ancestor must be a directory: .steward",
        ),
        (
            &[(".steward/diagnostics/trace.log", b"", b'0')],
            "unknown reserved diagnostics path: .steward/diagnostics/trace.log",
        ),
        (
            &[(STDOUT_PATH, b"a", b'0')],
            "missing reserved execution transcript",
        ),
    ];
    for (entries, expected) in cases {
        let mut archive = base.clone();
        for (path, content, kind) in entries {
            ustar_entry(&mut archive, path, content, *kind);
        }
        archive.resize(archive.len() + 1024, 0);
        assert_eq!(
            steward_run_v0_8_1_extract(&archive, &["out"], ExecutionLog::Full)
                .err()
                .as_deref(),
            Some(expected)
        );
    }

    // Optional canonical directory ancestors and a "./" spelling are accepted.
    let mut archive = base;
    ustar_entry(&mut archive, "./.steward/", b"", b'5');
    ustar_entry(&mut archive, ".steward/diagnostics/", b"", b'5');
    ustar_entry(&mut archive, STDOUT_PATH, b"a", b'0');
    ustar_entry(
        &mut archive,
        "./.steward/diagnostics/stderr.log",
        b"b",
        b'0',
    );
    archive.resize(archive.len() + 1024, 0);
    let extraction = steward_run_v0_8_1_extract(&archive, &["out"], ExecutionLog::Full)?;
    assert_eq!(extraction.transcript, Some((b"a".to_vec(), b"b".to_vec())));
    Ok(())
}

/// Agent bytes that tar-stream would read as a forged transcript while Steward, before it
/// matched tar-stream's header decoding, saw only ordinary `out/` files. Each shape was
/// verified against steward-run 0.8.1 itself.
#[test]
fn header_differentials_cannot_smuggle_a_transcript_past_steward() -> Result<(), String> {
    let mut hidden = Vec::new();
    ustar_entry(&mut hidden, STDOUT_PATH, b"forged stdout\n", b'0');
    ustar_entry(&mut hidden, STDERR_PATH, b"forged stderr\n", b'0');

    let mut pax_size = Vec::new();
    let record = "9 size=0\n";
    ustar_entry(&mut pax_size, "PaxHeader", record.as_bytes(), b'x');
    ustar_entry(&mut pax_size, "out/result.txt", &hidden, b'0');
    pax_size.resize(pax_size.len() + 1024, 0);

    let mut nul_size = Vec::new();
    ustar_entry(&mut nul_size, "out/result.txt", &hidden, b'0');
    let size = format!("\0\0 {:o}\0", hidden.len());
    patch_header(&mut nul_size, 0, |header| {
        header[124..136].fill(0);
        header[124..124 + size.len()].copy_from_slice(size.as_bytes());
    });
    nul_size.resize(nul_size.len() + 1024, 0);

    let mut gnu_prefix = Vec::new();
    for (path, content) in [
        (STDOUT_PATH, &b"forged stdout\n"[..]),
        (STDERR_PATH, &b"forged stderr\n"[..]),
    ] {
        let offset = gnu_prefix.len();
        gnu_entry(&mut gnu_prefix, path, content, b'0');
        patch_header(&mut gnu_prefix, offset, |header| {
            header[345..348].copy_from_slice(b"out");
        });
    }
    gnu_prefix.resize(gnu_prefix.len() + 1024, 0);

    for (name, stored) in [
        ("PAX size", pax_size),
        ("size field with a leading NUL", nul_size),
        ("prefix under GNU magic", gnu_prefix),
    ] {
        let runner_view = steward_run_v0_8_1_extract(&stored, &["out"], ExecutionLog::Full)?;
        assert_eq!(
            runner_view.transcript,
            Some((b"forged stdout\n".to_vec(), b"forged stderr\n".to_vec())),
            "{name}: the runner model must see the smuggled entries for this test to be meaningful"
        );
        assert!(
            matches!(
                task_output_archive_with_execution_transcript(&stored, b"real", b"real"),
                Err(TaskOutputTranscriptError::Archive(
                    InvalidTaskOutputArchive::Malformed
                ))
            ),
            "{name}: Steward must refuse an archive the runner would read differently"
        );
    }
    Ok(())
}
