use super::*;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

type BaselineMap = HashMap<PathBuf, Option<Vec<u8>>>;

/// Session change tracking with one ownership model:
///
/// - `originals` is the session-wide first-seen baseline, shared by the parent
///   session and every foreground/background child driver. The first
///   `snapshot` of a path wins (whatever driver makes it), so the persisted
///   `file_originals` row is always the true earliest value even when a
///   background child and the parent race on the same path.
/// - `dirty` is the shared set of newly inserted baselines awaiting the store.
///   Any driver's gate may drain it; a turn-end drain is the backstop.
/// - `turn_originals` is the turn-summary scope. Foreground children share the
///   parent's scope (their writes belong to the parent turn summary); detached
///   background children have no turn scope at all (their writes are reported
///   via FileChanged, not the parent's current-turn summary), so they never
///   allocate or record an unused map.
pub struct ChangeTracker {
    originals: Arc<Mutex<BaselineMap>>,
    dirty: Arc<Mutex<HashSet<PathBuf>>>,
    turn_originals: Option<Arc<Mutex<BaselineMap>>>,
    /// Per-turn pre-write snapshot stack aligned 1:1 with the root session's
    /// recallable `user_ixs` stack. Enables coordinated conversation + file
    /// undo on `recall_last_message` (Kimi-style turn checkpoints).
    turn_checkpoints: Vec<BaselineMap>,
}

/// Outcome for one file restored by [`ChangeTracker::pop_and_restore_last_turn`].
#[derive(Debug, Clone)]
pub enum TurnRevertOutcome {
    /// File returned to its pre-session baseline (or deleted when originally absent).
    RevertedToBaseline {
        full_path: PathBuf,
        rel_path: String,
    },
    /// File returned to an earlier turn's state, which still differs from the
    /// session baseline.
    UpdatedDiff(FileChange),
}

impl Default for ChangeTracker {
    fn default() -> Self {
        Self {
            originals: Arc::default(),
            dirty: Arc::default(),
            turn_originals: Some(Arc::default()),
            turn_checkpoints: Vec::new(),
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl ChangeTracker {
    /// A foreground child driver: shares the session baselines, the dirty
    /// queue and the parent's current-turn summary.
    pub fn foreground_child(&self) -> Self {
        Self {
            originals: Arc::clone(&self.originals),
            dirty: Arc::clone(&self.dirty),
            turn_originals: self.turn_originals.clone(),
            turn_checkpoints: Vec::new(),
        }
    }

    /// A detached background child driver: shares the session baselines and
    /// the dirty queue only.
    pub fn detached_child(&self) -> Self {
        Self {
            originals: Arc::clone(&self.originals),
            dirty: Arc::clone(&self.dirty),
            turn_originals: None,
            turn_checkpoints: Vec::new(),
        }
    }
}

/// Read a file's raw bytes through an `ExecutorFileSystem` (None = file does not exist).
fn read_original_bytes_with_fs(
    fs: &dyn crate::task::ExecutorFileSystem,
    path: &Path,
) -> Result<Option<Vec<u8>>, String> {
    match fs.read_all(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("Failed to read {}: {e}", path.display())),
    }
}

/// Bytes → LF model view: pig_utils text::decode first, falling back to lossy UTF-8 on failure (diff fallback)
pub(crate) fn decoded_view(bytes: &[u8]) -> String {
    match pig_utils::text::decode(bytes) {
        Ok(doc) => doc.text,
        Err(_) => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// Snapshot persistence encoding prefix: non-UTF-8 bytes cross the String boundary as hexadecimal
const SNAPSHOT_HEX_PREFIX: &str = "pigcode:hex:";

/// Raw bytes → persisted String: valid UTF-8 converts directly; otherwise hex (keeping the store/session signatures unchanged)
pub fn snapshot_to_store(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => {
            let mut out = String::with_capacity(SNAPSHOT_HEX_PREFIX.len() + bytes.len() * 2);
            out.push_str(SNAPSHOT_HEX_PREFIX);
            for byte in bytes {
                out.push(
                    char::from_digit((byte >> 4) as u32, 16).expect("0-15 is a valid hex digit"),
                );
                out.push(
                    char::from_digit((byte & 0x0f) as u32, 16).expect("0-15 is a valid hex digit"),
                );
            }
            out
        }
    }
}

/// Persisted String → raw bytes: decode when the hex prefix is present; no prefix = valid UTF-8 stored directly by the writer
/// (snapshot_to_store adds the hex prefix only for non-UTF-8 bytes; UTF-8 originals get no prefix)
pub fn snapshot_from_store(s: &str) -> Vec<u8> {
    let Some(hex) = s.strip_prefix(SNAPSHOT_HEX_PREFIX) else {
        return s.as_bytes().to_vec();
    };
    let digits = hex.as_bytes();
    let mut out = Vec::with_capacity(digits.len() / 2);
    let mut index = 0;
    while index + 1 < digits.len() {
        let high = (digits[index] as char).to_digit(16);
        let low = (digits[index + 1] as char).to_digit(16);
        match (high, low) {
            (Some(high), Some(low)) => out.push(((high << 4) | low) as u8),
            // Invalid hex (corrupted data): truncate as a safety net, never panic
            _ => break,
        }
        index += 2;
    }
    out
}

impl ChangeTracker {
    /// Snapshot raw bytes before modification (None = the file did not exist).
    /// The session baseline is inserted by whichever driver gets here first and
    /// is never re-read afterwards; the turn scope records the pre-write state
    /// only for its first writer.
    pub fn snapshot(&mut self, path: &Path) -> Result<(), String> {
        self.snapshot_with_fs(&crate::task::LocalFileSystem, path)
    }

    /// Snapshot raw bytes before modification through an [`ExecutorFileSystem`].
    pub fn snapshot_with_fs(
        &mut self,
        fs: &dyn crate::task::ExecutorFileSystem,
        path: &Path,
    ) -> Result<(), String> {
        let path_buf = path.to_path_buf();
        let inserted = {
            let mut originals = lock(&self.originals);
            match originals.entry(path_buf.clone()) {
                std::collections::hash_map::Entry::Occupied(_) => false,
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(read_original_bytes_with_fs(fs, path)?);
                    true
                }
            }
        };
        if inserted {
            lock(&self.dirty).insert(path_buf.clone());
        }
        // The first write in this tracker's turn scope records the disk state
        // before that write; concurrent foreground siblings share the scope,
        // so whichever snapshots first supplies the turn-start value. Detached
        // children have no turn scope and stop at the session baselines.
        if let Some(turn_scope) = &self.turn_originals {
            let mut turn = lock(turn_scope);
            if let std::collections::hash_map::Entry::Vacant(entry) = turn.entry(path_buf) {
                entry.insert(read_original_bytes_with_fs(fs, path)?);
            }
        }
        Ok(())
    }

    /// Take the newly inserted baselines awaiting persistence. Shared across
    /// the session and all child drivers: whichever gate drains first persists
    /// them; the turn-end drain is the backstop.
    pub fn take_dirty(&mut self) -> Vec<PathBuf> {
        lock(&self.dirty).drain().collect()
    }

    /// Read a path's original snapshot (a None value = the file did not originally exist; a None return = untracked).
    /// Internal bytes go through snapshot_to_store into a persisted String; the session-side 4MB check logic is unchanged.
    pub fn original(&self, path: &Path) -> Option<Option<String>> {
        lock(&self.originals)
            .get(path)
            .map(|original| original.as_deref().map(snapshot_to_store))
    }

    /// Restore the baseline after restart (from the file_originals table; restored entries are not marked dirty, avoiding write-back).
    /// The persisted String is turned back into bytes via snapshot_from_store.
    pub fn restore(&mut self, entries: Vec<(PathBuf, Option<String>)>) {
        let mut originals = lock(&self.originals);
        for (path, original) in entries {
            originals.insert(path, original.map(|s| snapshot_from_store(&s)));
        }
    }

    /// Produce the "original → current" unified diff and added/removed line counts (both sides decoded to the LF view first).
    pub fn diff(&mut self, cwd: &Path, path: &Path) -> Result<FileChange, String> {
        self.diff_with_fs(&crate::task::LocalFileSystem, cwd, path)
    }

    /// Produce the "original → current" unified diff through an [`ExecutorFileSystem`].
    pub fn diff_with_fs(
        &mut self,
        fs: &dyn crate::task::ExecutorFileSystem,
        cwd: &Path,
        path: &Path,
    ) -> Result<FileChange, String> {
        let original_bytes = lock(&self.originals)
            .get(path)
            .ok_or_else(|| "file not tracked".to_string())?
            .clone()
            .unwrap_or_default();
        let original = decoded_view(&original_bytes);
        let current_bytes = match fs.read_all(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(format!("failed to read {}: {error}", path.display())),
        };
        let current = decoded_view(&current_bytes);
        let diff = similar::TextDiff::from_lines(&original, &current);
        let mut additions = 0;
        let mut deletions = 0;
        for change in diff.iter_all_changes() {
            match change.tag() {
                similar::ChangeTag::Insert => additions += 1,
                similar::ChangeTag::Delete => deletions += 1,
                _ => {}
            }
        }
        let cwd_canonical = fs.canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
        let relative = path
            .strip_prefix(&cwd_canonical)
            .or_else(|_| path.strip_prefix(cwd))
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|_| path.to_path_buf());
        let relative = relative.to_string_lossy().replace('\\', "/");
        let unified = diff
            .unified_diff()
            .context_radius(3)
            .header(&format!("a/{relative}"), &format!("b/{relative}"))
            .to_string();
        Ok(FileChange {
            path: relative,
            unified_diff: unified,
            additions,
            deletions,
        })
    }

    /// Seed empty checkpoint slots to align with a restored `user_ixs` stack
    /// after session load (pre-restart turns have no per-turn undo snapshots).
    pub fn seed_empty_turn_checkpoints(&mut self, count: usize) {
        self.turn_checkpoints.clear();
        self.turn_checkpoints
            .resize_with(count, HashMap::new);
    }

    /// Open a new turn checkpoint slot aligned with a newly accepted recallable
    /// user message. Flushes any leftover `turn_originals` into the previous
    /// slot first so no pre-write baseline is silently dropped.
    pub fn begin_turn_checkpoint(&mut self) {
        self.drain_turn_originals_into_top();
        self.turn_checkpoints.push(HashMap::new());
    }

    /// Clear per-turn file snapshots for recall entries that compaction turned
    /// into non-recallable tombstones (Kimi's compaction-boundary stop).
    pub fn clear_compacted_checkpoints(&mut self, keep_mask: &[bool]) {
        self.drain_turn_originals_into_top();
        for (slot, &keep) in self.turn_checkpoints.iter_mut().zip(keep_mask.iter()) {
            if !keep {
                slot.clear();
            }
        }
    }

    fn drain_turn_originals_into_top(&mut self) -> BaselineMap {
        let Some(turn_scope) = &self.turn_originals else {
            return HashMap::new();
        };
        let entries = {
            let mut turn = lock(turn_scope);
            std::mem::take(&mut *turn)
        };
        if let Some(top) = self.turn_checkpoints.last_mut() {
            for (path, before) in &entries {
                top.entry(path.clone()).or_insert_with(|| before.clone());
            }
        }
        entries
    }

    /// Compute and clear "this turn's changes": per file, the net diff of "before the first write of this turn → current disk" (LF view).
    /// Also records the drained pre-turn baselines into the active turn checkpoint for coordinated undo.
    /// A zero net delta (reverted to the original within the turn) produces nothing; deleted files produce nothing for now.
    pub fn take_turn_changes(&mut self, cwd: &Path) -> Vec<FileChange> {
        let entries = self.drain_turn_originals_into_top();
        let mut changes = Vec::new();
        for (path, before) in entries {
            let before = decoded_view(&before.unwrap_or_default());
            let Ok(current_bytes) = std::fs::read(&path) else {
                continue;
            };
            let current = decoded_view(&current_bytes);
            let change = per_edit_diff(cwd, &path, &before, &current);
            if change.additions == 0 && change.deletions == 0 {
                continue;
            }
            changes.push(change);
        }
        changes.sort_by(|a, b| a.path.cmp(&b.path));
        changes
    }

    /// Pop the latest turn checkpoint and restore every file modified during
    /// that turn back to its pre-turn bytes (deleting files created in that
    /// turn). Reconciles `originals` against disk so callers can emit exact
    /// `FileReverted` or updated `FileChanged` events.
    pub fn pop_and_restore_last_turn(
        &mut self,
        cwd: &Path,
    ) -> Vec<Result<TurnRevertOutcome, CoreError>> {
        self.drain_turn_originals_into_top();
        let Some(snapshot) = self.turn_checkpoints.pop() else {
            return Vec::new();
        };
        let cwd_canonical = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        let mut paths: Vec<(PathBuf, Option<Vec<u8>>)> = snapshot.into_iter().collect();
        paths.sort_by(|a, b| a.0.cmp(&b.0));

        let mut outcomes = Vec::with_capacity(paths.len());
        for (path, before_turn) in paths {
            let write_result = match &before_turn {
                Some(bytes) => {
                    if let Some(parent) = path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    std::fs::write(&path, bytes).map_err(|e| CoreError::RevertWrite {
                        path: path.display().to_string(),
                        detail: e.to_string(),
                    })
                }
                None => match std::fs::remove_file(&path) {
                    Ok(()) => Ok(()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(e) => Err(CoreError::RevertDelete {
                        path: path.display().to_string(),
                        detail: e.to_string(),
                    }),
                },
            };
            if let Err(error) = write_result {
                outcomes.push(Err(error));
                continue;
            }
            let rel_path = path
                .strip_prefix(&cwd_canonical)
                .or_else(|_| path.strip_prefix(cwd))
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| path.to_string_lossy().replace('\\', "/"));
            let returned_to_baseline = {
                let originals = lock(&self.originals);
                match originals.get(&path) {
                    Some(session_baseline) => session_baseline == &before_turn,
                    None => true,
                }
            };
            if returned_to_baseline {
                lock(&self.originals).remove(&path);
                outcomes.push(Ok(TurnRevertOutcome::RevertedToBaseline {
                    full_path: path,
                    rel_path,
                }));
            } else {
                match self.diff(cwd, &path) {
                    Ok(change) if change.additions == 0 && change.deletions == 0 => {
                        lock(&self.originals).remove(&path);
                        outcomes.push(Ok(TurnRevertOutcome::RevertedToBaseline {
                            full_path: path,
                            rel_path,
                        }));
                    }
                    Ok(change) => outcomes.push(Ok(TurnRevertOutcome::UpdatedDiff(change))),
                    Err(_) => {
                        lock(&self.originals).remove(&path);
                        outcomes.push(Ok(TurnRevertOutcome::RevertedToBaseline {
                            full_path: path,
                            rel_path,
                        }));
                    }
                }
            }
        }
        outcomes
    }

    /// Revert: write the original bytes back verbatim (byte-exact for GBK/UTF-16/CRLF); newly created files are deleted.
    pub fn revert(&mut self, path: &Path) -> Result<(), CoreError> {
        let Some(original) = lock(&self.originals).remove(path) else {
            return Err(CoreError::RevertNotModified);
        };
        match original {
            Some(bytes) => std::fs::write(path, bytes).map_err(|e| CoreError::RevertWrite {
                path: path.display().to_string(),
                detail: e.to_string(),
            }),
            None => std::fs::remove_file(path).map_err(|e| CoreError::RevertDelete {
                path: path.display().to_string(),
                detail: e.to_string(),
            }),
        }
    }

    pub fn tracked_paths(&self) -> Vec<String> {
        lock(&self.originals)
            .keys()
            .map(|p| p.to_string_lossy().to_string())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The session baselines and dirty queue stay shared, foreground children
    /// share the parent's turn scope, and detached children allocate none.
    #[test]
    fn foreground_children_share_the_turn_scope_and_detached_children_skip_it() {
        let parent = ChangeTracker::default();
        let foreground = parent.foreground_child();
        assert!(
            foreground
                .turn_originals
                .as_ref()
                .is_some_and(|scope| Arc::ptr_eq(
                    scope,
                    parent.turn_originals.as_ref().expect("root turn scope")
                )),
            "a foreground child writes into the parent's turn summary"
        );

        let mut detached = parent.detached_child();
        assert!(
            detached.turn_originals.is_none(),
            "a detached child must not allocate an unused turn map"
        );
        assert!(
            Arc::ptr_eq(&detached.originals, &parent.originals)
                && Arc::ptr_eq(&detached.dirty, &parent.dirty),
            "session baselines and the dirty queue stay shared"
        );
        assert!(
            detached.take_turn_changes(Path::new("/tmp")).is_empty(),
            "a detached child has no current-turn summary"
        );
    }

    #[test]
    fn multi_turn_checkpoints_restore_step_by_step_and_delete_created_files() {
        let dir = std::env::temp_dir().join(format!(
            "pig-turn-undo-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let existing = dir.join("app.txt");
        let created = dir.join("new.txt");
        std::fs::write(&existing, "v0\n").unwrap();

        let mut tracker = ChangeTracker::default();

        // Turn 1: modify existing v0 -> v1
        tracker.begin_turn_checkpoint();
        tracker.snapshot(&existing).unwrap();
        std::fs::write(&existing, "v1\n").unwrap();
        let t1_changes = tracker.take_turn_changes(&dir);
        assert_eq!(t1_changes.len(), 1);

        // Turn 2: modify existing v1 -> v2, and create new.txt
        tracker.begin_turn_checkpoint();
        tracker.snapshot(&existing).unwrap();
        std::fs::write(&existing, "v2\n").unwrap();
        tracker.snapshot(&created).unwrap();
        std::fs::write(&created, "hello\n").unwrap();
        let t2_changes = tracker.take_turn_changes(&dir);
        assert_eq!(t2_changes.len(), 2);

        // Undo Turn 2: existing returns to v1 (UpdatedDiff), created is deleted (RevertedToBaseline)
        let undo2 = tracker.pop_and_restore_last_turn(&dir);
        assert_eq!(std::fs::read_to_string(&existing).unwrap(), "v1\n");
        assert!(!created.exists());
        assert!(undo2.iter().any(|r| matches!(
            r,
            Ok(TurnRevertOutcome::RevertedToBaseline { rel_path, .. }) if rel_path == "new.txt"
        )));
        assert!(undo2.iter().any(|r| matches!(
            r,
            Ok(TurnRevertOutcome::UpdatedDiff(change)) if change.path == "app.txt"
        )));

        // Undo Turn 1: existing returns to v0 (RevertedToBaseline)
        let undo1 = tracker.pop_and_restore_last_turn(&dir);
        assert_eq!(std::fs::read_to_string(&existing).unwrap(), "v0\n");
        assert!(undo1.iter().any(|r| matches!(
            r,
            Ok(TurnRevertOutcome::RevertedToBaseline { rel_path, .. }) if rel_path == "app.txt"
        )));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
