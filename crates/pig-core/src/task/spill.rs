//! Spill-file writer for task output.
//!
//! The spill lives under
//! `{data_dir}/sessions/{session_id}/tool-results/{task_id}.log`. That subtree
//! is already an approval-free read root for the model (subagent results and
//! context files live below it), so the model can page through the full output
//! with Read, while the workspace stays free of pig's own task artifacts and
//! two sessions in the same workspace cannot overwrite each other's files
//! (task ids are per-session sequences).
//!
//! The file is created lazily: output up to [`SPILL_THRESHOLD_BYTES`] stays in
//! memory only (the registry's rolling window plus the TaskSummary tail), and
//! the first larger chunk creates the file with the buffered prefix. The
//! opened handle is then held for later chunks instead of reopening the file
//! per chunk. Failures are retained and surfaced through the task; a
//! command must still return its output if the spill cannot be persisted.

use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Spill file cap (full output persisted to disk) of 10MB; writing stops beyond it.
const MAX_SPILL_BYTES: u64 = 10 * 1024 * 1024;

/// Output at or below this size never creates a file. It matches ExecCommand's
/// 30KB inline-output limit: only output that needed the head/tail preview has
/// a full copy worth reading afterwards.
pub(super) const SPILL_THRESHOLD_BYTES: usize = 30 * 1024;

const TRUNCATED_NOTICE: &str = "[Output exceeded 10MB; the rest was discarded]";

/// The task's spill path:
/// `{data_dir}/sessions/{session_id}/tool-results/{task_id}.log`.
pub(super) fn path_for(data_dir: &Path, session_id: &str, task_id: &str) -> PathBuf {
    data_dir
        .join("sessions")
        .join(session_id)
        .join("tool-results")
        .join(format!("{task_id}.log"))
}

/// Lazily opened append sink for one task's full output.
pub(crate) struct Spill {
    path: PathBuf,
    file: Option<std::fs::File>,
    /// Raw output kept back until the threshold is crossed.
    pending: Vec<u8>,
    written: u64,
    /// A failed open is final: do not buffer or retry for this task.
    failed: bool,
    failure: Option<String>,
}

impl Spill {
    pub(crate) fn new(path: PathBuf) -> Self {
        // The registry is not persisted, so a file under this session/task id
        // belongs to an earlier run and must not be shown or appended to. The
        // new task owns the id; removing the path also drops a planted link
        // instead of following it.
        let failure = match std::fs::remove_file(&path) {
            Ok(()) => None,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => Some(format!(
                "Failed to persist task spill {}: failed to replace previous output: {error}",
                path.display()
            )),
        };
        let failed = failure.is_some();
        Self {
            path,
            file: None,
            pending: Vec::new(),
            written: 0,
            failed,
            failure,
        }
    }

    pub(crate) fn restored(path: PathBuf) -> Self {
        Self {
            path,
            file: None,
            pending: Vec::new(),
            written: 0,
            failed: true,
            failure: None,
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Append raw output bytes; the first chunk that pushes the total past the
    /// threshold creates the file and writes the buffered prefix too.
    pub(crate) fn append(&mut self, chunk: &[u8]) -> Result<(), String> {
        if chunk.is_empty() {
            return Ok(());
        }
        if self.failed {
            return self.failure.clone().map_or(Ok(()), Err);
        }
        let result = if self.file.is_none() {
            self.pending.extend_from_slice(chunk);
            if self.pending.len() <= SPILL_THRESHOLD_BYTES {
                return Ok(());
            }
            self.open().and_then(|()| {
                let pending = std::mem::take(&mut self.pending);
                self.write(&pending)
            })
        } else {
            self.write(chunk)
        };
        if let Err(error) = result {
            let error = format!(
                "Failed to persist task spill {}: {error}",
                self.path.display()
            );
            self.failed = true;
            self.failure = Some(error.clone());
            return Err(error);
        }
        Ok(())
    }

    fn open(&mut self) -> Result<(), String> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| "spill path has no parent".to_string())?;
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|error| error.to_string())?;
        self.file = Some(file);
        self.written = 0;
        Ok(())
    }

    fn write(&mut self, chunk: &[u8]) -> Result<(), String> {
        let Some(file) = self.file.as_mut() else {
            return Err("spill file is not open".to_string());
        };
        if self.written >= MAX_SPILL_BYTES {
            return Ok(());
        }
        let remaining = (MAX_SPILL_BYTES - self.written) as usize;
        if chunk.len() <= remaining {
            file.write_all(chunk).map_err(|error| error.to_string())?;
            self.written += chunk.len() as u64;
        } else {
            file.write_all(&chunk[..remaining])
                .map_err(|error| error.to_string())?;
            file.write_all(TRUNCATED_NOTICE.as_bytes())
                .map_err(|error| error.to_string())?;
            self.written = MAX_SPILL_BYTES;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static SEQ: AtomicUsize = AtomicUsize::new(0);

    fn fixture(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pig-spill-{name}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("fixture");
        dir.canonicalize().expect("canonical fixture")
    }

    #[test]
    fn the_spill_path_is_scoped_to_the_session_data_dir() {
        let data = fixture("path");
        let path = path_for(&data, "s1", "b1");
        assert_eq!(
            path,
            data.join("sessions")
                .join("s1")
                .join("tool-results")
                .join("b1.log"),
            "path shape is data-dir/sessions/<session>/tool-results/<task>.log"
        );
        assert!(
            path.starts_with(&data),
            "the spill stays inside the data dir"
        );
        assert!(
            !path.to_string_lossy().contains(".pigcode"),
            "the workspace is not part of the spill path"
        );
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn the_file_appears_only_after_the_threshold_and_keeps_appending() {
        let data = fixture("threshold");
        let path = path_for(&data, "s1", "b1");
        let mut spill = Spill::new(path.clone());

        let below = vec![b'a'; SPILL_THRESHOLD_BYTES];
        spill.append(&below).expect("buffered output");
        assert!(
            !path.exists(),
            "output up to the threshold must not create a spill file"
        );

        spill.append(b"b").expect("cross spill threshold");
        let text = std::fs::read_to_string(&path).expect("the crossing chunk creates the file");
        assert_eq!(text.len(), SPILL_THRESHOLD_BYTES + 1);
        assert!(text.starts_with("aaaa"), "the buffered prefix is written");
        assert!(text.ends_with('b'));

        spill.append(b"tail").expect("append spill tail");
        assert!(
            std::fs::read_to_string(&path)
                .expect("later chunks use the held handle")
                .ends_with("btail")
        );
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn writes_stop_at_the_cap_with_a_notice() {
        let data = fixture("cap");
        let path = path_for(&data, "s1", "b1");
        let mut spill = Spill::new(path.clone());

        let start = vec![b'a'; SPILL_THRESHOLD_BYTES + 1];
        spill.append(&start).expect("start capped spill");
        let fill = vec![b'b'; MAX_SPILL_BYTES as usize];
        spill.append(&fill).expect("fill capped spill");
        spill.append(b"after-the-cap").expect("cap remains stable");

        let text = std::fs::read_to_string(&path).expect("spill exists");
        assert!(text.len() as u64 >= MAX_SPILL_BYTES);
        assert!(text.ends_with(TRUNCATED_NOTICE));
        assert!(!text.contains("after-the-cap"));
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn a_failed_spill_is_visible_and_never_retries() {
        let root = fixture("failed");
        let blocker = root.join("blocked");
        std::fs::write(&blocker, "file").expect("blocking file");
        let mut spill = Spill::new(blocker.join("tool-results").join("b1.log"));

        let chunk = vec![b'a'; SPILL_THRESHOLD_BYTES + 1];
        let first = spill.append(&chunk).expect_err("spill error is visible");
        let second = spill.append(b"more").expect_err("same retained failure");
        assert_eq!(first, second);
        assert!(first.contains("Failed to persist task spill"));
        assert!(
            blocker.is_file(),
            "a path that cannot become a directory leaves no trace"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A link planted at the spill path itself is replaced, never followed:
    /// the dummy target must not receive output.
    #[cfg(unix)]
    #[test]
    fn a_planted_spill_link_is_replaced_not_followed() {
        let data = fixture("link");
        let dummy = fixture("link-dummy");
        let target = dummy.join("metadata.txt");
        std::fs::write(&target, "do-not-touch").expect("dummy target");
        let path = path_for(&data, "s1", "b1");
        std::fs::create_dir_all(path.parent().expect("spill parent")).expect("spill dir");
        std::os::unix::fs::symlink(&target, &path).expect("planted link");

        let mut spill = Spill::new(path.clone());
        let chunk = vec![b'a'; SPILL_THRESHOLD_BYTES + 1];
        spill.append(&chunk);

        assert!(
            std::fs::symlink_metadata(&path)
                .expect("spill exists")
                .file_type()
                .is_file(),
            "the spill is a regular file again, not the planted link"
        );
        assert_eq!(
            std::fs::read_to_string(&target).expect("dummy target"),
            "do-not-touch",
            "the link target must not receive spill output"
        );
        let _ = std::fs::remove_dir_all(&data);
        let _ = std::fs::remove_dir_all(&dummy);
    }
}
