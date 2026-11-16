//! One JSONL line scanner and append-tail repair shared by the root rollout
//! and subagent context files.
//!
//! The policies deliberately stay with the callers: the root rollout backs up
//! and atomically replaces a torn tail on load, a read-only snapshot ignores
//! an in-flight final line, and a child context read ignores a torn final line
//! without touching the file. The child append path repairs its exclusive
//! single-writer file before writing.

use std::path::Path;

/// One line in a byte stream, split on `\n` and preserving offsets.
#[derive(Clone, Copy, Debug)]
pub(crate) struct JsonlLine<'a> {
    /// 1-based line number.
    pub number: usize,
    /// Byte offset of the first byte of the line.
    pub start: usize,
    /// The line including its trailing `\n` when present.
    pub chunk: &'a [u8],
    /// The line content without the trailing `\n`.
    pub body: &'a [u8],
}

/// Split a JSONL byte stream into lines. `split_inclusive` is used so a
/// partial final line is still yielded with `terminated == false`; callers
/// decide whether that is a torn record to ignore, repair or reject.
pub(crate) fn lines(bytes: &[u8]) -> impl Iterator<Item = JsonlLine<'_>> {
    let mut offset = 0usize;
    bytes
        .split_inclusive(|byte| *byte == b'\n')
        .enumerate()
        .map(move |(index, chunk)| {
            let start = offset;
            offset += chunk.len();
            let body = if chunk.ends_with(b"\n") {
                &chunk[..chunk.len() - 1]
            } else {
                chunk
            };
            JsonlLine {
                number: index + 1,
                start,
                chunk,
                body,
            }
        })
}

/// The final chunk of a file, inspected without rewriting it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct JsonlTail<'a> {
    /// Byte offset where the final chunk starts (after the last `\n`).
    pub start: usize,
    /// The final chunk without its newline (empty for a clean file).
    pub body: &'a [u8],
    /// True when the final chunk is newline-terminated (or the file is empty).
    pub terminated: bool,
}

/// Inspect the final chunk of a JSONL byte stream. A clean file ends with a
/// newline, so the tail is empty and `terminated` is true.
pub(crate) fn inspect_tail(bytes: &[u8]) -> JsonlTail<'_> {
    let start = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    let chunk = &bytes[start..];
    let terminated = chunk.is_empty() || chunk.ends_with(b"\n");
    let body = if chunk.ends_with(b"\n") {
        &chunk[..chunk.len() - 1]
    } else {
        chunk
    };
    JsonlTail {
        start,
        body,
        terminated,
    }
}

/// Whether a final line body is a complete JSON record.
pub(crate) fn is_complete_json(body: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(body).is_ok()
}

/// Repair a torn final line before appending. A crash during a previous write
/// can leave bytes without the terminating newline; appending directly would
/// glue the next record onto them and turn a recoverable prefix into a hard
/// parse error. A valid final record without its newline is preserved (the
/// newline is added); a torn partial line is truncated. The caller must hold
/// the file's exclusive writer claim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TailRepair {
    Clean,
    NewlineAdded,
    Truncated { removed_bytes: u64 },
}

pub(crate) fn repair_tail_before_append(path: &Path, label: &str) -> Result<TailRepair, String> {
    use std::io::{Read as _, Seek as _, SeekFrom, Write as _};

    let Ok(metadata) = std::fs::metadata(path) else {
        return Ok(TailRepair::Clean);
    };
    if metadata.len() == 0 {
        return Ok(TailRepair::Clean);
    }
    let mut probe = std::fs::File::open(path)
        .map_err(|e| format!("Failed to inspect {label} {}: {e}", path.display()))?;
    probe
        .seek(SeekFrom::End(-1))
        .map_err(|e| format!("Failed to inspect {label} {}: {e}", path.display()))?;
    let mut last = [0u8; 1];
    probe
        .read_exact(&mut last)
        .map_err(|e| format!("Failed to inspect {label} {}: {e}", path.display()))?;
    if last[0] == b'\n' {
        return Ok(TailRepair::Clean);
    }
    drop(probe);
    let bytes = std::fs::read(path)
        .map_err(|e| format!("Failed to read {label} {}: {e}", path.display()))?;
    let tail = inspect_tail(&bytes);
    if is_complete_json(tail.body) {
        // A complete final record just lacks its separator (a legacy or
        // manually written file): keep it and add the newline.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .map_err(|e| format!("Failed to open {label} {}: {e}", path.display()))?;
        file.write_all(b"\n")
            .map_err(|e| format!("Failed to write {label} {}: {e}", path.display()))?;
        file.sync_all()
            .map_err(|e| format!("Failed to sync {label} {}: {e}", path.display()))?;
        return Ok(TailRepair::NewlineAdded);
    }
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|e| format!("Failed to open {label} {}: {e}", path.display()))?;
    file.set_len(tail.start as u64)
        .map_err(|e| format!("Failed to repair {label} {}: {e}", path.display()))?;
    file.sync_all()
        .map_err(|e| format!("Failed to repair {label} {}: {e}", path.display()))?;
    let removed_bytes = (bytes.len() - tail.start) as u64;
    tracing::warn!(
        path = %path.display(),
        removed_bytes,
        "repaired a torn final line before appending"
    );
    Ok(TailRepair::Truncated { removed_bytes })
}
