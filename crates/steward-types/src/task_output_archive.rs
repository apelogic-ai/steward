//! Canonical validation for the durable Task output archive.
//!
//! New archives contain only the declared `out/` tree. A bounded compatibility mode keeps
//! historical archives readable when they also contain the two formerly embedded execution-log
//! transcripts. Transcript bytes are never returned as Task outputs.
//!
//! The stored archive stays `out/`-only. The authenticated runner download of a Task whose
//! snapshotted diagnostics requested `executionLog: full` is rebuilt from the validated entries
//! with Steward-written headers and gains the server-owned transcript, see
//! [`task_output_archive_with_execution_transcript`].

use std::collections::BTreeSet;

use crate::direct_package::{
    EXECUTION_STDERR_ARCHIVE_PATH, EXECUTION_STDOUT_ARCHIVE_PATH, MAX_EXECUTION_STREAM_BYTES,
    RelativePath,
};

const TAR_BLOCK_BYTES: usize = 512;

/// Durable marker written beside a newly validated `out/`-only archive.
pub const TASK_OUTPUT_ARCHIVE_CONTRACT: &str = "steward.task-output/v1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskOutputArchiveCompatibility {
    Strict,
    HistoricalMixedDiagnostics,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskOutputArchiveEntry {
    pub path: String,
    pub offset: usize,
    pub size: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidTaskOutputArchive {
    Malformed,
    UnsupportedLink,
}

/// Validate one tar archive and return only regular files below its declared `out/` root.
///
/// GNU long-name and per-entry PAX path records are resolved before the path boundary is
/// checked. Global PAX metadata is tolerated only when it does not redefine a path. Symbolic and
/// hard links are rejected rather than followed or exposed as outputs.
pub fn task_output_archive_entries(
    archive: &[u8],
    compatibility: TaskOutputArchiveCompatibility,
) -> Result<Vec<TaskOutputArchiveEntry>, InvalidTaskOutputArchive> {
    walk_task_output_archive(archive, compatibility).map(|walked| {
        walked
            .into_iter()
            .filter_map(|entry| match entry {
                WalkedEntry::File { entry, .. } => Some(entry),
                WalkedEntry::Directory(_) => None,
            })
            .collect()
    })
}

/// One validated `out/` entry in archive order; directory paths are relative to `out/`, with
/// the empty string naming `out/` itself.
enum WalkedEntry {
    Directory(String),
    File {
        entry: TaskOutputArchiveEntry,
        executable: bool,
    },
}

fn walk_task_output_archive(
    archive: &[u8],
    compatibility: TaskOutputArchiveCompatibility,
) -> Result<Vec<WalkedEntry>, InvalidTaskOutputArchive> {
    if archive.len() < TAR_BLOCK_BYTES * 3 || !archive.len().is_multiple_of(TAR_BLOCK_BYTES) {
        return Err(InvalidTaskOutputArchive::Malformed);
    }

    let mut offset = 0_usize;
    let mut entries = Vec::new();
    let mut seen = BTreeSet::new();
    let mut next_path = None;
    while offset
        .checked_add(TAR_BLOCK_BYTES)
        .filter(|end| *end <= archive.len())
        .is_some()
    {
        let header = &archive[offset..offset + TAR_BLOCK_BYTES];
        if header.iter().all(|byte| *byte == 0) {
            if next_path.is_some() {
                return Err(InvalidTaskOutputArchive::Malformed);
            }
            return archive[offset..]
                .iter()
                .all(|byte| *byte == 0)
                .then_some(entries)
                .ok_or(InvalidTaskOutputArchive::Malformed);
        }
        validate_checksum(header)?;
        let header_path = tar_path(header)?;
        let size = tar_octal(&header[124..136])?;
        let data_offset = offset
            .checked_add(TAR_BLOCK_BYTES)
            .ok_or(InvalidTaskOutputArchive::Malformed)?;
        let data_end = data_offset
            .checked_add(size)
            .filter(|end| *end <= archive.len())
            .ok_or(InvalidTaskOutputArchive::Malformed)?;
        let kind = header[156];
        let data = &archive[data_offset..data_end];

        match kind {
            b'L' => {
                if next_path.is_some() {
                    return Err(InvalidTaskOutputArchive::Malformed);
                }
                next_path = Some(gnu_long_name(data)?);
            }
            b'x' => {
                let attributes = pax_attributes(data)?;
                if attributes.link_path {
                    return Err(InvalidTaskOutputArchive::UnsupportedLink);
                }
                if let Some(path) = attributes.path
                    && next_path.replace(path).is_some()
                {
                    return Err(InvalidTaskOutputArchive::Malformed);
                }
            }
            b'g' => {
                let attributes = pax_attributes(data)?;
                if attributes.link_path {
                    return Err(InvalidTaskOutputArchive::UnsupportedLink);
                }
                if attributes.path.is_some() {
                    return Err(InvalidTaskOutputArchive::Malformed);
                }
            }
            0 | b'0' => {
                let path = next_path.take().unwrap_or(header_path);
                if is_historical_diagnostic(&path) {
                    if compatibility != TaskOutputArchiveCompatibility::HistoricalMixedDiagnostics {
                        return Err(InvalidTaskOutputArchive::Malformed);
                    }
                } else {
                    let relative = path
                        .strip_prefix("out/")
                        .ok_or(InvalidTaskOutputArchive::Malformed)?;
                    let relative = RelativePath::parse(relative.to_owned())
                        .map_err(|_| InvalidTaskOutputArchive::Malformed)?;
                    if !seen.insert(relative.as_str().to_owned()) {
                        return Err(InvalidTaskOutputArchive::Malformed);
                    }
                    entries.push(WalkedEntry::File {
                        entry: TaskOutputArchiveEntry {
                            path: relative.as_str().to_owned(),
                            offset: data_offset,
                            size,
                        },
                        executable: tar_octal(&header[100..108])
                            .is_ok_and(|mode| mode & 0o111 != 0),
                    });
                }
            }
            b'5' => {
                let path = next_path.take().unwrap_or(header_path);
                entries.push(WalkedEntry::Directory(validate_directory(&path, size)?));
            }
            b'1' | b'2' | b'K' => return Err(InvalidTaskOutputArchive::UnsupportedLink),
            _ => return Err(InvalidTaskOutputArchive::Malformed),
        }

        let padded = size
            .checked_add(TAR_BLOCK_BYTES - 1)
            .ok_or(InvalidTaskOutputArchive::Malformed)?
            / TAR_BLOCK_BYTES
            * TAR_BLOCK_BYTES;
        offset = data_offset
            .checked_add(padded)
            .filter(|next| *next >= data_end)
            .ok_or(InvalidTaskOutputArchive::Malformed)?;
    }
    Err(InvalidTaskOutputArchive::Malformed)
}

/// Bounded failures while delivering a runner-facing archive with its execution transcript.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskOutputTranscriptError {
    /// The stored archive is not a valid `out/`-only `steward.task-output/v1` archive.
    Archive(InvalidTaskOutputArchive),
    /// A stream exceeds 4 MiB.
    TranscriptTooLarge,
}

/// Build the runner-facing archive of a Task that requested `executionLog: full`.
///
/// steward-run 0.8.1, which generated callers pin, requires
/// `.steward/diagnostics/stdout.log` and `.steward/diagnostics/stderr.log` inside the output
/// archive whenever the authenticated Task status reports `executionLog: full`. No byte of an
/// agent-authored tar header reaches the runner: the stored archive is validated strictly as
/// `out/`-only, and every validated directory and regular file is re-emitted in archive order
/// with a header written by [`append_tar_entry`]. Steward then appends the two transcript files
/// and an end-of-archive marker. Two tar parsers can therefore never disagree about where an
/// agent entry ends, and agent output can never create or replace the reserved namespace.
///
/// Each stream is bounded to 4 MiB (8 MiB combined); an oversized stream fails closed rather
/// than being truncated. The delivered archive is at most the stored archive's size plus the
/// transcript plus Steward's fixed per-entry headers.
pub fn task_output_archive_with_execution_transcript(
    archive: Vec<u8>,
    stdout: &[u8],
    stderr: &[u8],
) -> Result<Vec<u8>, TaskOutputTranscriptError> {
    let within_bound = |stream: &[u8]| {
        u64::try_from(stream.len()).is_ok_and(|len| len <= MAX_EXECUTION_STREAM_BYTES)
    };
    if !within_bound(stdout) || !within_bound(stderr) {
        return Err(TaskOutputTranscriptError::TranscriptTooLarge);
    }
    let walked = walk_task_output_archive(&archive, TaskOutputArchiveCompatibility::Strict)
        .map_err(TaskOutputTranscriptError::Archive)?;
    let mut delivered = Vec::with_capacity(
        archive.len()
            + 4 * TAR_BLOCK_BYTES
            + padded_tar_bytes(stdout.len())
            + padded_tar_bytes(stderr.len()),
    );
    let mut directories = BTreeSet::new();
    for entry in walked {
        match entry {
            WalkedEntry::Directory(relative) => {
                if directories.insert(relative.clone()) {
                    let path = if relative.is_empty() {
                        "out/".to_owned()
                    } else {
                        format!("out/{relative}/")
                    };
                    append_tar_entry(&mut delivered, &path, TarEntryKind::Directory, &[]);
                }
            }
            WalkedEntry::File { entry, executable } => {
                let kind = if executable {
                    TarEntryKind::ExecutableFile
                } else {
                    TarEntryKind::File
                };
                append_tar_entry(
                    &mut delivered,
                    &format!("out/{}", entry.path),
                    kind,
                    &archive[entry.offset..entry.offset + entry.size],
                );
            }
        }
    }
    drop(archive);
    append_tar_entry(
        &mut delivered,
        EXECUTION_STDOUT_ARCHIVE_PATH,
        TarEntryKind::File,
        stdout,
    );
    append_tar_entry(
        &mut delivered,
        EXECUTION_STDERR_ARCHIVE_PATH,
        TarEntryKind::File,
        stderr,
    );
    finish_tar_archive(&mut delivered);
    Ok(delivered)
}

/// The entry types Steward writes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TarEntryKind {
    /// A regular file, mode 0644.
    File,
    /// A regular file, mode 0755.
    ExecutableFile,
    /// A directory, mode 0755; its path should end in `/`.
    Directory,
}

/// Append one Steward-written POSIX ustar entry with fixed metadata (uid/gid 0, mtime 0, no
/// user or group names, no prefix). A path longer than the 100-byte name field is carried by a
/// preceding PAX `path` record that Steward writes, with a truncated name in the ustar header.
pub fn append_tar_entry(archive: &mut Vec<u8>, path: &str, kind: TarEntryKind, content: &[u8]) {
    let name = if path.len() <= 100 {
        path
    } else {
        let record = pax_record("path", path);
        append_tar_header(archive, "PaxHeader", b'x', 0o644, record.len());
        archive.extend_from_slice(record.as_bytes());
        pad_tar_data(archive, record.len());
        let mut end = 100;
        while !path.is_char_boundary(end) {
            end -= 1;
        }
        &path[..end]
    };
    let (typeflag, mode, content) = match kind {
        TarEntryKind::File => (b'0', 0o644, content),
        TarEntryKind::ExecutableFile => (b'0', 0o755, content),
        TarEntryKind::Directory => (b'5', 0o755, &[][..]),
    };
    append_tar_header(archive, name, typeflag, mode, content.len());
    archive.extend_from_slice(content);
    pad_tar_data(archive, content.len());
}

/// Append the two zero blocks that end a tar archive.
pub fn finish_tar_archive(archive: &mut Vec<u8>) {
    archive.resize(archive.len() + 2 * TAR_BLOCK_BYTES, 0);
}

fn append_tar_header(archive: &mut Vec<u8>, name: &str, typeflag: u8, mode: u32, size: usize) {
    let header = archive.len();
    archive.resize(header + TAR_BLOCK_BYTES, 0);
    let block = &mut archive[header..header + TAR_BLOCK_BYTES];
    block[..name.len()].copy_from_slice(name.as_bytes());
    block[100..108].copy_from_slice(format!("{mode:07o}\0").as_bytes());
    block[108..116].copy_from_slice(b"0000000\0");
    block[116..124].copy_from_slice(b"0000000\0");
    block[124..136].copy_from_slice(format!("{size:011o}\0").as_bytes());
    block[136..148].copy_from_slice(b"00000000000\0");
    block[148..156].fill(b' ');
    block[156] = typeflag;
    block[257..263].copy_from_slice(b"ustar\0");
    block[263..265].copy_from_slice(b"00");
    block[329..337].copy_from_slice(b"0000000\0");
    block[337..345].copy_from_slice(b"0000000\0");
    let checksum = block.iter().map(|byte| usize::from(*byte)).sum::<usize>();
    block[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
}

fn pad_tar_data(archive: &mut Vec<u8>, size: usize) {
    archive.resize(archive.len() + padded_tar_bytes(size) - size, 0);
}

fn padded_tar_bytes(size: usize) -> usize {
    size.div_ceil(TAR_BLOCK_BYTES) * TAR_BLOCK_BYTES
}

/// One PAX extended-header record: `<length> <key>=<value>\n`, where the length counts itself.
fn pax_record(key: &str, value: &str) -> String {
    let payload = format!(" {key}={value}\n");
    let mut digits = 1;
    loop {
        let length = digits + payload.len();
        let text = length.to_string();
        if text.len() == digits {
            return format!("{text}{payload}");
        }
        digits = text.len();
    }
}

fn gnu_long_name(data: &[u8]) -> Result<String, InvalidTaskOutputArchive> {
    let end = data
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(data.len());
    if end == 0 || data[end..].iter().any(|byte| *byte != 0) {
        return Err(InvalidTaskOutputArchive::Malformed);
    }
    std::str::from_utf8(&data[..end])
        .map(str::to_owned)
        .map_err(|_| InvalidTaskOutputArchive::Malformed)
}

#[derive(Default)]
struct PaxAttributes {
    path: Option<String>,
    link_path: bool,
}

fn pax_attributes(data: &[u8]) -> Result<PaxAttributes, InvalidTaskOutputArchive> {
    let mut attributes = PaxAttributes::default();
    let mut offset = 0_usize;
    while offset < data.len() {
        let remaining = &data[offset..];
        let space = remaining
            .iter()
            .position(|byte| *byte == b' ')
            .ok_or(InvalidTaskOutputArchive::Malformed)?;
        let length = std::str::from_utf8(&remaining[..space])
            .map_err(|_| InvalidTaskOutputArchive::Malformed)?
            .parse::<usize>()
            .map_err(|_| InvalidTaskOutputArchive::Malformed)?;
        if length <= space + 1 {
            return Err(InvalidTaskOutputArchive::Malformed);
        }
        let end = offset
            .checked_add(length)
            .filter(|end| *end <= data.len())
            .ok_or(InvalidTaskOutputArchive::Malformed)?;
        let record = &data[offset + space + 1..end];
        let record = record
            .strip_suffix(b"\n")
            .ok_or(InvalidTaskOutputArchive::Malformed)?;
        let equals = record
            .iter()
            .position(|byte| *byte == b'=')
            .ok_or(InvalidTaskOutputArchive::Malformed)?;
        let key = std::str::from_utf8(&record[..equals])
            .map_err(|_| InvalidTaskOutputArchive::Malformed)?;
        match key {
            "path" => {
                let value = std::str::from_utf8(&record[equals + 1..])
                    .map_err(|_| InvalidTaskOutputArchive::Malformed)?;
                if value.is_empty() || attributes.path.replace(value.to_owned()).is_some() {
                    return Err(InvalidTaskOutputArchive::Malformed);
                }
            }
            "linkpath" => attributes.link_path = true,
            _ => {}
        }
        offset = end;
    }
    Ok(attributes)
}

fn is_historical_diagnostic(path: &str) -> bool {
    matches!(
        path,
        EXECUTION_STDOUT_ARCHIVE_PATH | EXECUTION_STDERR_ARCHIVE_PATH
    )
}

fn validate_directory(path: &str, size: usize) -> Result<String, InvalidTaskOutputArchive> {
    if size != 0 {
        return Err(InvalidTaskOutputArchive::Malformed);
    }
    let path = path.strip_suffix('/').unwrap_or(path);
    if path == "out" {
        return Ok(String::new());
    }
    let relative = path
        .strip_prefix("out/")
        .ok_or(InvalidTaskOutputArchive::Malformed)?;
    RelativePath::parse(relative.to_owned())
        .map(|relative| relative.as_str().to_owned())
        .map_err(|_| InvalidTaskOutputArchive::Malformed)
}

fn tar_path(header: &[u8]) -> Result<String, InvalidTaskOutputArchive> {
    fn field(bytes: &[u8]) -> Result<&str, InvalidTaskOutputArchive> {
        let end = bytes
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(bytes.len());
        if bytes[end..].iter().any(|byte| *byte != 0) {
            return Err(InvalidTaskOutputArchive::Malformed);
        }
        std::str::from_utf8(&bytes[..end]).map_err(|_| InvalidTaskOutputArchive::Malformed)
    }

    let name = field(&header[..100])?;
    let prefix = field(&header[345..500])?;
    if name.is_empty() {
        return Err(InvalidTaskOutputArchive::Malformed);
    }
    Ok(if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}/{name}")
    })
}

fn tar_octal(bytes: &[u8]) -> Result<usize, InvalidTaskOutputArchive> {
    let text = std::str::from_utf8(bytes).map_err(|_| InvalidTaskOutputArchive::Malformed)?;
    let text = text.trim_matches(['\0', ' ']);
    if text.is_empty() {
        return Ok(0);
    }
    usize::from_str_radix(text, 8).map_err(|_| InvalidTaskOutputArchive::Malformed)
}

fn validate_checksum(header: &[u8]) -> Result<(), InvalidTaskOutputArchive> {
    let expected = tar_octal(&header[148..156])?;
    let actual = header
        .iter()
        .enumerate()
        .map(|(index, byte)| {
            usize::from(if (148..156).contains(&index) {
                b' '
            } else {
                *byte
            })
        })
        .sum::<usize>();
    (expected == actual)
        .then_some(())
        .ok_or(InvalidTaskOutputArchive::Malformed)
}

#[cfg(test)]
mod tests {
    use super::{
        InvalidTaskOutputArchive, TarEntryKind, TaskOutputArchiveCompatibility,
        TaskOutputTranscriptError, append_tar_entry, finish_tar_archive,
        task_output_archive_entries, task_output_archive_with_execution_transcript,
    };

    fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut archive = Vec::new();
        for (path, content) in entries {
            append_tar_entry(&mut archive, path, TarEntryKind::File, content);
        }
        finish_tar_archive(&mut archive);
        archive
    }

    fn archive_with_kinds(entries: &[(&str, &[u8], u8)]) -> Vec<u8> {
        let mut archive = Vec::new();
        for (path, content, kind) in entries {
            append_entry(&mut archive, path, content, *kind);
        }
        archive.resize(archive.len() + 1024, 0);
        archive
    }

    fn append_entry(archive: &mut Vec<u8>, path: &str, content: &[u8], kind: u8) {
        let header_offset = archive.len();
        archive.resize(header_offset + 512, 0);
        archive[header_offset..header_offset + path.len()].copy_from_slice(path.as_bytes());
        archive[header_offset + 100..header_offset + 108].copy_from_slice(b"0000644\0");
        archive[header_offset + 108..header_offset + 116].copy_from_slice(b"0000000\0");
        archive[header_offset + 116..header_offset + 124].copy_from_slice(b"0000000\0");
        let size = format!("{:011o}\0", content.len());
        archive[header_offset + 124..header_offset + 136].copy_from_slice(size.as_bytes());
        archive[header_offset + 136..header_offset + 148].copy_from_slice(b"00000000000\0");
        archive[header_offset + 148..header_offset + 156].fill(b' ');
        archive[header_offset + 156] = kind;
        archive[header_offset + 257..header_offset + 263].copy_from_slice(b"ustar\0");
        archive[header_offset + 263..header_offset + 265].copy_from_slice(b"00");
        let checksum = archive[header_offset..header_offset + 512]
            .iter()
            .map(|byte| usize::from(*byte))
            .sum::<usize>();
        let checksum = format!("{:06o}\0 ", checksum);
        archive[header_offset + 148..header_offset + 156].copy_from_slice(checksum.as_bytes());
        archive.extend_from_slice(content);
        archive.resize(header_offset + 512 + content.len().div_ceil(512) * 512, 0);
    }

    fn pax_record(key: &str, value: &str) -> String {
        let payload = format!("{key}={value}\n");
        let mut length = payload.len() + 2;
        loop {
            let candidate = format!("{length} {payload}");
            if candidate.len() == length {
                return candidate;
            }
            length = candidate.len();
        }
    }

    /// Rewrite one header in place and refresh its checksum.
    fn patch_header(archive: &mut [u8], header_offset: usize, patch: impl Fn(&mut [u8])) {
        let header = &mut archive[header_offset..header_offset + 512];
        patch(header);
        header[148..156].fill(b' ');
        let checksum = header.iter().map(|byte| usize::from(*byte)).sum::<usize>();
        header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
    }

    /// Agent-style headers: GNU magic, a user name, a timestamp, and an executable mode.
    fn agent_archive() -> Vec<u8> {
        let mut stored = archive_with_kinds(&[
            ("out/", b"", b'5'),
            ("out/report.md", b"complete\n", b'0'),
            ("out/run.sh", b"#!/bin/sh\n", b'0'),
        ]);
        for (offset, mode) in [(0, b"0000775\0"), (512, b"0000664\0"), (1536, b"0000775\0")] {
            patch_header(&mut stored, offset, |header| {
                header[100..108].copy_from_slice(mode);
                header[108..116].copy_from_slice(b"0001750\0");
                header[136..148].copy_from_slice(b"15073551234\0");
                header[257..265].copy_from_slice(b"ustar  \0");
                header[265..270].copy_from_slice(b"agent");
            });
        }
        stored
    }

    #[test]
    fn runner_delivery_rewrites_every_header_and_appends_the_transcript() -> Result<(), String> {
        let stdout = b"stdout line\n".repeat(100);
        let delivered =
            task_output_archive_with_execution_transcript(agent_archive(), &stdout, b"")
                .map_err(|error| format!("runner archive was not produced: {error:?}"))?;

        let mut expected = Vec::new();
        append_tar_entry(&mut expected, "out/", TarEntryKind::Directory, b"");
        append_tar_entry(
            &mut expected,
            "out/report.md",
            TarEntryKind::File,
            b"complete\n",
        );
        append_tar_entry(
            &mut expected,
            "out/run.sh",
            TarEntryKind::ExecutableFile,
            b"#!/bin/sh\n",
        );
        append_tar_entry(
            &mut expected,
            ".steward/diagnostics/stdout.log",
            TarEntryKind::File,
            &stdout,
        );
        append_tar_entry(
            &mut expected,
            ".steward/diagnostics/stderr.log",
            TarEntryKind::File,
            b"",
        );
        finish_tar_archive(&mut expected);
        assert!(
            delivered == expected,
            "every delivered header is Steward-written; no agent header byte is forwarded"
        );
        assert!(!delivered.windows(5).any(|window| window == b"agent"));

        let listed = task_output_archive_entries(
            &delivered,
            TaskOutputArchiveCompatibility::HistoricalMixedDiagnostics,
        )
        .map_err(|error| format!("delivered archive must stay listable: {error:?}"))?;
        assert_eq!(
            listed
                .iter()
                .map(|entry| entry.path.as_str())
                .collect::<Vec<_>>(),
            ["report.md", "run.sh"],
            "transcripts are never Task output files"
        );
        assert!(
            task_output_archive_entries(&delivered, TaskOutputArchiveCompatibility::Strict)
                .is_err(),
            "a delivered archive must never be accepted back as a stored out/-only archive"
        );
        Ok(())
    }

    #[test]
    fn long_output_paths_are_delivered_through_a_steward_written_pax_record() -> Result<(), String>
    {
        let relative = format!("reports/{}.md", "a".repeat(120));
        let mut stored = Vec::new();
        append_tar_entry(
            &mut stored,
            &format!("out/{relative}"),
            TarEntryKind::File,
            b"long\n",
        );
        finish_tar_archive(&mut stored);
        assert_eq!(&stored[156..157], b"x", "the long path needs a PAX record");

        let delivered = task_output_archive_with_execution_transcript(stored, b"o", b"e")
            .map_err(|error| format!("runner archive was not produced: {error:?}"))?;
        let listed = task_output_archive_entries(
            &delivered,
            TaskOutputArchiveCompatibility::HistoricalMixedDiagnostics,
        )
        .map_err(|error| format!("delivered long path was rejected: {error:?}"))?;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].path, relative);
        Ok(())
    }

    #[test]
    fn archives_stored_by_earlier_releases_still_list_and_deliver() -> Result<(), String> {
        // Shapes 0.3.11 and 0.3.12 accepted: a pre-POSIX header without magic, a ustar prefix
        // under GNU magic, and a size field padded with leading NULs.
        let mut stored = archive_with_kinds(&[
            ("legacy.txt", b"legacy\n", b'0'),
            ("prefixed.txt", b"prefixed\n", b'0'),
            ("out/padded.txt", b"padded\n", b'0'),
        ]);
        patch_header(&mut stored, 0, |header| {
            header[..14].copy_from_slice(b"out/legacy.txt");
            header[257..265].fill(0);
        });
        patch_header(&mut stored, 1024, |header| {
            header[345..348].copy_from_slice(b"out");
            header[257..265].copy_from_slice(b"ustar  \0");
        });
        patch_header(&mut stored, 2048, |header| {
            header[124..136].copy_from_slice(b"\0\0\0\0 000007\0");
        });

        let listed = task_output_archive_entries(&stored, TaskOutputArchiveCompatibility::Strict)
            .map_err(|error| {
            format!("an archive stored by 0.3.11 must still list: {error:?}")
        })?;
        let paths = ["legacy.txt", "prefixed.txt", "padded.txt"];
        assert_eq!(
            listed
                .iter()
                .map(|entry| entry.path.as_str())
                .collect::<Vec<_>>(),
            paths
        );

        let delivered = task_output_archive_with_execution_transcript(stored, b"o", b"e")
            .map_err(|error| format!("an archive stored by 0.3.11 must deliver: {error:?}"))?;
        let mut expected = Vec::new();
        for (path, content) in [
            ("out/legacy.txt", &b"legacy\n"[..]),
            ("out/prefixed.txt", b"prefixed\n"),
            ("out/padded.txt", b"padded\n"),
            (".steward/diagnostics/stdout.log", b"o"),
            (".steward/diagnostics/stderr.log", b"e"),
        ] {
            append_tar_entry(&mut expected, path, TarEntryKind::File, content);
        }
        finish_tar_archive(&mut expected);
        assert!(
            delivered == expected,
            "earlier archives are re-emitted canonically"
        );
        Ok(())
    }

    #[test]
    fn runner_delivery_requires_a_strict_stored_archive_and_bounded_streams() {
        for stored in [
            archive(&[
                ("out/report.md", b"complete\n"),
                (".steward/diagnostics/stdout.log", b"forged"),
                (".steward/diagnostics/stderr.log", b"forged"),
            ]),
            archive_with_kinds(&[(".steward/diagnostics/", b"", b'5')]),
            archive_with_kinds(&[("out/link", b"", b'2')]),
        ] {
            assert!(matches!(
                task_output_archive_with_execution_transcript(stored, b"stdout", b"stderr"),
                Err(TaskOutputTranscriptError::Archive(_))
            ));
        }
        let oversized = vec![b'x'; 4 * 1024 * 1024 + 1];
        assert_eq!(
            task_output_archive_with_execution_transcript(
                archive(&[("out/report.md", b"complete\n")]),
                &oversized,
                b"",
            ),
            Err(TaskOutputTranscriptError::TranscriptTooLarge)
        );
    }

    #[test]
    fn strict_archive_contains_only_declared_outputs() -> Result<(), String> {
        let fixture = archive(&[("out/report.md", b"complete\n")]);
        let entries = task_output_archive_entries(&fixture, TaskOutputArchiveCompatibility::Strict)
            .map_err(|_| "declared output archive must validate".to_owned())?;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "report.md");
        assert_eq!(
            &fixture[entries[0].offset..entries[0].offset + entries[0].size],
            b"complete\n"
        );

        for path in [
            "secret.txt",
            "out/../secret.txt",
            ".steward/diagnostics/stdout.log",
        ] {
            assert!(
                task_output_archive_entries(
                    &archive(&[(path, b"no")]),
                    TaskOutputArchiveCompatibility::Strict,
                )
                .is_err(),
                "strict output archive must reject {path}"
            );
        }
        Ok(())
    }

    #[test]
    fn historical_mode_skips_only_the_two_legacy_transcripts() -> Result<(), String> {
        let fixture = archive(&[
            ("out/report.md", b"complete\n"),
            (".steward/diagnostics/stdout.log", b"stdout\n"),
            (".steward/diagnostics/stderr.log", b"stderr\n"),
        ]);
        let entries = task_output_archive_entries(
            &fixture,
            TaskOutputArchiveCompatibility::HistoricalMixedDiagnostics,
        )
        .map_err(|_| "historical mixed archive must remain readable".to_owned())?;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "report.md");

        assert!(
            task_output_archive_entries(
                &archive(&[(".steward/diagnostics/trace.log", b"no")]),
                TaskOutputArchiveCompatibility::HistoricalMixedDiagnostics,
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn long_name_extensions_resolve_before_the_output_boundary() -> Result<(), String> {
        let relative = format!("reports/{}.md", "a".repeat(120));
        let path = format!("out/{relative}");
        let gnu = archive_with_kinds(&[
            ("././@LongLink", format!("{path}\0").as_bytes(), b'L'),
            ("out/truncated", b"complete\n", b'0'),
        ]);
        let pax_path = pax_record("path", &path);
        let pax = archive_with_kinds(&[
            ("PaxHeader", pax_path.as_bytes(), b'x'),
            ("out/truncated", b"complete\n", b'0'),
        ]);

        for fixture in [&gnu, &pax] {
            let entries =
                task_output_archive_entries(fixture, TaskOutputArchiveCompatibility::Strict)
                    .map_err(|error| format!("long output path was rejected: {error:?}"))?;
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].path, relative);
        }
        Ok(())
    }

    #[test]
    fn hard_and_symbolic_links_have_an_explicit_rejection() {
        for kind in [b'1', b'2'] {
            assert_eq!(
                task_output_archive_entries(
                    &archive_with_kinds(&[("out/link", b"", kind)]),
                    TaskOutputArchiveCompatibility::Strict,
                ),
                Err(InvalidTaskOutputArchive::UnsupportedLink)
            );
        }
    }
}
