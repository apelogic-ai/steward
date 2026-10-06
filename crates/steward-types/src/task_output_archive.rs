//! Canonical validation for the durable Task output archive.
//!
//! New archives contain only the declared `out/` tree. A bounded compatibility mode keeps
//! historical archives readable when they also contain the two formerly embedded execution-log
//! transcripts. Transcript bytes are never returned as Task outputs.

use std::collections::BTreeSet;

use crate::direct_package::{
    EXECUTION_STDERR_ARCHIVE_PATH, EXECUTION_STDOUT_ARCHIVE_PATH, RelativePath,
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
                if let Some(path) = attributes.path {
                    if next_path.replace(path).is_some() {
                        return Err(InvalidTaskOutputArchive::Malformed);
                    }
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
        InvalidTaskOutputArchive, TaskOutputArchiveCompatibility, task_output_archive_entries,
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
