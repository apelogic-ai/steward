//! Canonical validation for the durable Task output archive.
//!
//! New archives contain only the declared `out/` tree. A bounded compatibility mode keeps
//! historical archives readable when they also contain the two formerly embedded execution-log
//! transcripts. Transcript bytes are never returned as Task outputs.
//!
//! The stored archive stays `out/`-only. Only the authenticated runner delivery of a Task whose
//! snapshotted diagnostics requested `executionLog: full` appends the server-owned transcript,
//! see [`task_output_archive_with_execution_transcript`].

use std::collections::BTreeSet;

use crate::direct_package::{
    EXECUTION_STDERR_ARCHIVE_PATH, EXECUTION_STDOUT_ARCHIVE_PATH, MAX_EXECUTION_STREAM_BYTES,
    MAX_EXECUTION_TRANSCRIPT_BYTES, RelativePath,
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
    walk_task_output_archive(archive, compatibility).map(|(entries, _)| entries)
}

/// Validate the archive and also return the offset of its end-of-archive marker.
fn walk_task_output_archive(
    archive: &[u8],
    compatibility: TaskOutputArchiveCompatibility,
) -> Result<(Vec<TaskOutputArchiveEntry>, usize), InvalidTaskOutputArchive> {
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
                .then_some((entries, offset))
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
                    entries.push(TaskOutputArchiveEntry {
                        path: relative.as_str().to_owned(),
                        offset: data_offset,
                        size,
                    });
                }
            }
            b'5' => {
                let path = next_path.take().unwrap_or(header_path);
                validate_directory(&path, size)?;
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
    /// A stream exceeds 4 MiB or the transcript exceeds 8 MiB.
    TranscriptTooLarge,
}

/// Deliver a stored `steward.task-output/v1` archive with the reserved execution transcript.
///
/// steward-run 0.8.1, which generated callers pin, requires
/// `.steward/diagnostics/stdout.log` and `.steward/diagnostics/stderr.log` inside the output
/// archive whenever the authenticated Task status reports `executionLog: full`. The stored
/// archive must validate strictly as `out/`-only, so agent output can never create or replace
/// the reserved namespace; Steward then appends exactly two regular-file entries, in the layout
/// the pre-0.3.11 adapter produced with `tar -rf`, before a fresh end-of-archive marker. Each
/// stream is bounded to 4 MiB and the transcript to 8 MiB; an oversized stream fails closed
/// rather than being truncated.
pub fn task_output_archive_with_execution_transcript(
    archive: &[u8],
    stdout: &[u8],
    stderr: &[u8],
) -> Result<Vec<u8>, TaskOutputTranscriptError> {
    let stream_bytes = |stream: &[u8]| u64::try_from(stream.len()).unwrap_or(u64::MAX);
    let (stdout_bytes, stderr_bytes) = (stream_bytes(stdout), stream_bytes(stderr));
    if stdout_bytes > MAX_EXECUTION_STREAM_BYTES
        || stderr_bytes > MAX_EXECUTION_STREAM_BYTES
        || stdout_bytes.saturating_add(stderr_bytes) > MAX_EXECUTION_TRANSCRIPT_BYTES
    {
        return Err(TaskOutputTranscriptError::TranscriptTooLarge);
    }
    let (_, end) = walk_task_output_archive(archive, TaskOutputArchiveCompatibility::Strict)
        .map_err(TaskOutputTranscriptError::Archive)?;
    let mut delivered = Vec::with_capacity(
        end + 4 * TAR_BLOCK_BYTES + padded_tar_bytes(stdout.len()) + padded_tar_bytes(stderr.len()),
    );
    delivered.extend_from_slice(&archive[..end]);
    append_transcript_entry(&mut delivered, EXECUTION_STDOUT_ARCHIVE_PATH, stdout);
    append_transcript_entry(&mut delivered, EXECUTION_STDERR_ARCHIVE_PATH, stderr);
    delivered.resize(delivered.len() + 2 * TAR_BLOCK_BYTES, 0);
    Ok(delivered)
}

fn padded_tar_bytes(size: usize) -> usize {
    size.div_ceil(TAR_BLOCK_BYTES) * TAR_BLOCK_BYTES
}

/// Append one POSIX ustar regular file with a fixed, metadata-free header.
fn append_transcript_entry(archive: &mut Vec<u8>, path: &str, content: &[u8]) {
    let header = archive.len();
    archive.resize(header + TAR_BLOCK_BYTES, 0);
    let block = &mut archive[header..header + TAR_BLOCK_BYTES];
    block[..path.len()].copy_from_slice(path.as_bytes());
    block[100..108].copy_from_slice(b"0000644\0");
    block[108..116].copy_from_slice(b"0000000\0");
    block[116..124].copy_from_slice(b"0000000\0");
    block[124..136].copy_from_slice(format!("{:011o}\0", content.len()).as_bytes());
    block[136..148].copy_from_slice(b"00000000000\0");
    block[148..156].fill(b' ');
    block[156] = b'0';
    block[257..263].copy_from_slice(b"ustar\0");
    block[263..265].copy_from_slice(b"00");
    block[329..337].copy_from_slice(b"0000000\0");
    block[337..345].copy_from_slice(b"0000000\0");
    let checksum = block.iter().map(|byte| usize::from(*byte)).sum::<usize>();
    block[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
    archive.extend_from_slice(content);
    archive.resize(
        header + TAR_BLOCK_BYTES + padded_tar_bytes(content.len()),
        0,
    );
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
            // tar-stream applies a PAX size to the entry; rejecting it keeps both parsers on
            // the header's own size field.
            "size" => return Err(InvalidTaskOutputArchive::Malformed),
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

fn validate_directory(path: &str, size: usize) -> Result<(), InvalidTaskOutputArchive> {
    if size != 0 {
        return Err(InvalidTaskOutputArchive::Malformed);
    }
    let path = path.strip_suffix('/').unwrap_or(path);
    if path == "out" {
        return Ok(());
    }
    let relative = path
        .strip_prefix("out/")
        .ok_or(InvalidTaskOutputArchive::Malformed)?;
    RelativePath::parse(relative.to_owned())
        .map(|_| ())
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
    // Match tar-stream 3.1.7: only POSIX ustar magic carries a path prefix, GNU magic ignores
    // that region, and any other format is rejected.
    let prefix = if &header[257..263] == b"ustar\0" {
        field(&header[345..500])?
    } else if &header[257..263] == b"ustar " && &header[263..265] == b" \0" {
        ""
    } else {
        return Err(InvalidTaskOutputArchive::Malformed);
    };
    if name.is_empty() {
        return Err(InvalidTaskOutputArchive::Malformed);
    }
    Ok(if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}/{name}")
    })
}

/// Parse a numeric header field exactly as tar-stream 3.1.7 would, or reject it.
///
/// Only `spaces* octal-digits (NUL|space)*` is accepted; tar-stream's lenient decoding of any
/// other spelling (for example a leading NUL before the digits) could disagree with the value
/// used here and desynchronize the runner's entry boundaries from Steward's.
fn tar_octal(bytes: &[u8]) -> Result<usize, InvalidTaskOutputArchive> {
    let start = bytes
        .iter()
        .position(|byte| *byte != b' ')
        .unwrap_or(bytes.len());
    let digits = bytes[start..]
        .iter()
        .take_while(|byte| (b'0'..=b'7').contains(*byte))
        .count();
    if bytes[start + digits..]
        .iter()
        .any(|byte| *byte != 0 && *byte != b' ')
    {
        return Err(InvalidTaskOutputArchive::Malformed);
    }
    if digits == 0 {
        return Ok(0);
    }
    let text = std::str::from_utf8(&bytes[start..start + digits])
        .map_err(|_| InvalidTaskOutputArchive::Malformed)?;
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
        InvalidTaskOutputArchive, TaskOutputArchiveCompatibility, TaskOutputTranscriptError,
        task_output_archive_entries, task_output_archive_with_execution_transcript,
        walk_task_output_archive,
    };

    fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut archive = Vec::new();
        for (path, content) in entries {
            append_entry(&mut archive, path, content, b'0');
        }
        archive.resize(archive.len() + 1024, 0);
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

    /// Rewrite one header of a single-entry fixture and refresh its checksum.
    fn patch_header(archive: &mut [u8], header_offset: usize, patch: impl Fn(&mut [u8])) {
        let header = &mut archive[header_offset..header_offset + 512];
        patch(header);
        header[148..156].fill(b' ');
        let checksum = header.iter().map(|byte| usize::from(*byte)).sum::<usize>();
        header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
    }

    #[test]
    fn headers_the_runner_reads_differently_are_rejected() {
        let regular = || archive(&[("out/result.txt", b"complete\n")]);
        let mut cases: Vec<(&str, Vec<u8>)> = Vec::new();

        // tar-stream reads "\0 11" as size 0 and would parse the data blocks as headers.
        let mut fixture = regular();
        patch_header(&mut fixture, 0, |header| {
            header[124..136].copy_from_slice(b"\0\0\0\0\0\0\0\0 11\0");
        });
        cases.push(("size with a leading NUL", fixture));

        // tar-stream ignores the ustar prefix under GNU magic.
        let mut fixture = archive(&[(".steward/diagnostics/stdout.log", b"forged")]);
        patch_header(&mut fixture, 0, |header| {
            header[345..348].copy_from_slice(b"out");
            header[257..265].copy_from_slice(b"ustar  \0");
        });
        cases.push(("prefix under GNU magic", fixture));

        // tar-stream rejects headers with neither ustar nor GNU magic.
        let mut fixture = regular();
        patch_header(&mut fixture, 0, |header| header[257..265].fill(0));
        cases.push(("missing magic", fixture));

        // tar-stream honours a PAX size, which would desynchronize the entry boundary.
        for kind in [b'x', b'g'] {
            let size = pax_record("size", "0");
            cases.push((
                "PAX size",
                archive_with_kinds(&[
                    ("PaxHeader", size.as_bytes(), kind),
                    ("out/result.txt", b"complete\n", b'0'),
                ]),
            ));
        }

        for (name, fixture) in cases {
            // Under GNU magic the forged path resolves, for both parsers, to the legacy
            // transcript, which only historical mode may skip.
            let modes: &[TaskOutputArchiveCompatibility] = if name == "prefix under GNU magic" {
                &[TaskOutputArchiveCompatibility::Strict]
            } else {
                &[
                    TaskOutputArchiveCompatibility::Strict,
                    TaskOutputArchiveCompatibility::HistoricalMixedDiagnostics,
                ]
            };
            for compatibility in modes.iter().copied() {
                assert!(
                    task_output_archive_entries(&fixture, compatibility).is_err(),
                    "{name} must be rejected in {compatibility:?}"
                );
            }
            assert!(
                task_output_archive_with_execution_transcript(&fixture, b"stdout", b"stderr")
                    .is_err(),
                "{name} must never be delivered with a transcript"
            );
        }
    }

    #[test]
    fn gnu_and_ustar_headers_without_ambiguity_remain_valid() -> Result<(), String> {
        let mut fixture = archive(&[("out/result.txt", b"complete\n")]);
        patch_header(&mut fixture, 0, |header| {
            header[257..265].copy_from_slice(b"ustar  \0");
            header[124..136].copy_from_slice(b" 0000000011 ");
        });
        let entries = task_output_archive_entries(&fixture, TaskOutputArchiveCompatibility::Strict)
            .map_err(|error| format!("GNU header was rejected: {error:?}"))?;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].size, 9);
        Ok(())
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
    fn runner_delivery_appends_exactly_the_two_reserved_transcript_files() -> Result<(), String> {
        let stored =
            archive_with_kinds(&[("out/", b"", b'5'), ("out/report.md", b"complete\n", b'0')]);
        let (_, end) = walk_task_output_archive(&stored, TaskOutputArchiveCompatibility::Strict)
            .map_err(|error| format!("stored archive must validate: {error:?}"))?;
        let stdout = b"stdout line\n".repeat(100);
        let delivered = task_output_archive_with_execution_transcript(&stored, &stdout, b"")
            .map_err(|error| format!("runner archive was not produced: {error:?}"))?;

        assert_eq!(
            &delivered[..end],
            &stored[..end],
            "stored entries stay byte-identical"
        );
        let stdout_header = &delivered[end..end + 512];
        let stderr_offset = end + 512 + stdout.len().div_ceil(512) * 512;
        let stderr_header = &delivered[stderr_offset..stderr_offset + 512];
        for (header, path, size) in [
            (
                stdout_header,
                ".steward/diagnostics/stdout.log",
                stdout.len(),
            ),
            (stderr_header, ".steward/diagnostics/stderr.log", 0),
        ] {
            assert_eq!(&header[..path.len()], path.as_bytes());
            assert!(header[path.len()..100].iter().all(|byte| *byte == 0));
            assert_eq!(header[156], b'0', "transcripts are regular files");
            assert_eq!(&header[257..265], b"ustar\x0000");
            assert_eq!(&header[124..136], format!("{size:011o}\0").as_bytes());
        }
        assert_eq!(&delivered[end + 512..end + 512 + stdout.len()], &stdout[..]);
        assert_eq!(delivered.len(), stderr_offset + 512 + 1024);
        assert!(
            delivered[stderr_offset + 512..]
                .iter()
                .all(|byte| *byte == 0)
        );

        let listed = task_output_archive_entries(
            &delivered,
            TaskOutputArchiveCompatibility::HistoricalMixedDiagnostics,
        )
        .map_err(|error| format!("delivered archive must stay listable: {error:?}"))?;
        assert_eq!(listed.len(), 1, "transcripts are never Task output files");
        assert_eq!(listed[0].path, "report.md");
        assert!(
            task_output_archive_entries(&delivered, TaskOutputArchiveCompatibility::Strict)
                .is_err(),
            "a delivered archive must never be accepted back as a stored out/-only archive"
        );
        Ok(())
    }

    #[test]
    fn runner_delivery_requires_a_strict_stored_archive() {
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
                task_output_archive_with_execution_transcript(&stored, b"stdout", b"stderr"),
                Err(TaskOutputTranscriptError::Archive(_))
            ));
        }
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
