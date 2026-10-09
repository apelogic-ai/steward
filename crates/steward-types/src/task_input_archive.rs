//! Internal framing for caller-authored Task inputs plus Steward-authored workspace material.
//!
//! The caller archive remains byte-for-byte available for idempotency checks. Workspace bytes
//! travel in a separate member so an untrusted tar entry cannot replace Steward's materializer
//! inputs or manifest during extraction.

const MAGIC: &[u8; 32] = b"steward.workspace-input/v1\0\0\0\0\0\0";
const HEADER_BYTES: usize = MAGIC.len() + 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskInputArchiveError {
    LengthOverflow,
    InvalidFrame,
}

pub struct TaskInputArchiveParts<'a> {
    pub caller_archive: &'a [u8],
    pub workspace_archive: Option<&'a [u8]>,
}

pub fn frame_task_input_archive(
    caller_archive: &[u8],
    workspace_archive: &[u8],
) -> Result<Vec<u8>, TaskInputArchiveError> {
    let caller_len =
        u64::try_from(caller_archive.len()).map_err(|_| TaskInputArchiveError::LengthOverflow)?;
    let workspace_len = u64::try_from(workspace_archive.len())
        .map_err(|_| TaskInputArchiveError::LengthOverflow)?;
    let capacity = HEADER_BYTES
        .checked_add(caller_archive.len())
        .and_then(|value| value.checked_add(workspace_archive.len()))
        .ok_or(TaskInputArchiveError::LengthOverflow)?;
    let mut framed = Vec::with_capacity(capacity);
    framed.extend_from_slice(MAGIC);
    framed.extend_from_slice(&caller_len.to_be_bytes());
    framed.extend_from_slice(&workspace_len.to_be_bytes());
    framed.extend_from_slice(caller_archive);
    framed.extend_from_slice(workspace_archive);
    Ok(framed)
}

pub fn split_task_input_archive(
    archive: &[u8],
) -> Result<TaskInputArchiveParts<'_>, TaskInputArchiveError> {
    if !archive.starts_with(MAGIC) {
        return Ok(TaskInputArchiveParts {
            caller_archive: archive,
            workspace_archive: None,
        });
    }
    let header = archive
        .get(..HEADER_BYTES)
        .ok_or(TaskInputArchiveError::InvalidFrame)?;
    let caller_len = u64::from_be_bytes(
        header[MAGIC.len()..MAGIC.len() + 8]
            .try_into()
            .map_err(|_| TaskInputArchiveError::InvalidFrame)?,
    );
    let workspace_len = u64::from_be_bytes(
        header[MAGIC.len() + 8..HEADER_BYTES]
            .try_into()
            .map_err(|_| TaskInputArchiveError::InvalidFrame)?,
    );
    let caller_len =
        usize::try_from(caller_len).map_err(|_| TaskInputArchiveError::InvalidFrame)?;
    let workspace_len =
        usize::try_from(workspace_len).map_err(|_| TaskInputArchiveError::InvalidFrame)?;
    let caller_end = HEADER_BYTES
        .checked_add(caller_len)
        .ok_or(TaskInputArchiveError::InvalidFrame)?;
    let workspace_end = caller_end
        .checked_add(workspace_len)
        .ok_or(TaskInputArchiveError::InvalidFrame)?;
    if workspace_end != archive.len() || workspace_len == 0 {
        return Err(TaskInputArchiveError::InvalidFrame);
    }
    Ok(TaskInputArchiveParts {
        caller_archive: archive
            .get(HEADER_BYTES..caller_end)
            .ok_or(TaskInputArchiveError::InvalidFrame)?,
        workspace_archive: Some(
            archive
                .get(caller_end..workspace_end)
                .ok_or(TaskInputArchiveError::InvalidFrame)?,
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::{TaskInputArchiveError, frame_task_input_archive, split_task_input_archive};

    #[test]
    fn frame_keeps_caller_and_workspace_bytes_separate() -> Result<(), String> {
        let framed = frame_task_input_archive(b"caller-tar", b"workspace-tar")
            .map_err(|error| format!("frame failed: {error:?}"))?;
        let parts = split_task_input_archive(&framed)
            .map_err(|error| format!("split failed: {error:?}"))?;
        assert_eq!(parts.caller_archive, b"caller-tar");
        assert_eq!(parts.workspace_archive, Some(b"workspace-tar".as_slice()));
        Ok(())
    }

    #[test]
    fn ordinary_archive_remains_unchanged() -> Result<(), String> {
        let parts = split_task_input_archive(b"ordinary-tar")
            .map_err(|error| format!("split failed: {error:?}"))?;
        assert_eq!(parts.caller_archive, b"ordinary-tar");
        assert_eq!(parts.workspace_archive, None);
        Ok(())
    }

    #[test]
    fn malformed_frame_fails_closed() -> Result<(), String> {
        let mut framed = frame_task_input_archive(b"caller", b"workspace")
            .map_err(|error| format!("frame failed: {error:?}"))?;
        framed.pop();
        assert_eq!(
            split_task_input_archive(&framed).err(),
            Some(TaskInputArchiveError::InvalidFrame)
        );
        Ok(())
    }
}
