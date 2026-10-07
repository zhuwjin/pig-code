use super::*;

/// Records turn_originals; at turn end take_turn_changes computes the "before the first write of this turn → current" net delta and clears it.
#[derive(Default)]
pub struct ChangeTracker {
    originals: HashMap<PathBuf, Option<Vec<u8>>>,
    stats: HashMap<PathBuf, (u32, u32)>,
    /// Snapshot paths newly added in this process, not yet persisted (the session side drains them into the store)
    dirty: Vec<PathBuf>,
    /// Original bytes before each file's first write within this turn (None = created this turn); taken (cleared) at turn end
    turn_originals: HashMap<PathBuf, Option<Vec<u8>>>,
}

/// Read a file's raw bytes (None = file does not exist)
fn read_original_bytes(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("Failed to read {}: {e}", path.display())),
    }
}

/// Bytes → LF model view: text::decode first, falling back to lossy UTF-8 on failure (diff fallback)
pub(crate) fn decoded_view(bytes: &[u8]) -> String {
    match crate::text::decode(bytes) {
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
    /// Snapshot raw bytes before modification (None = the file did not exist); paths already snapshotted are not read from disk again.
    pub fn snapshot(&mut self, path: &Path) -> Result<(), String> {
        if let std::collections::hash_map::Entry::Vacant(entry) =
            self.originals.entry(path.to_path_buf())
        {
            entry.insert(read_original_bytes(path)?);
            self.dirty.push(path.to_path_buf());
        }
        // Per-turn accounting: also record before this turn's first write (independent of the session-level baseline)
        if let std::collections::hash_map::Entry::Vacant(entry) =
            self.turn_originals.entry(path.to_path_buf())
        {
            entry.insert(read_original_bytes(path)?);
        }
        Ok(())
    }

    /// Take out the newly added snapshot paths (cleared after persisting)
    pub fn take_dirty(&mut self) -> Vec<PathBuf> {
        std::mem::take(&mut self.dirty)
    }

    /// Read a path's original snapshot (a None value = the file did not originally exist; a None return = untracked).
    /// Internal bytes go through snapshot_to_store into a persisted String; the session-side 4MB check logic is unchanged.
    pub fn original(&self, path: &Path) -> Option<Option<String>> {
        self.originals
            .get(path)
            .map(|original| original.as_deref().map(snapshot_to_store))
    }

    /// Restore the baseline after restart (from the file_originals table; restored entries are not marked dirty, avoiding write-back).
    /// The persisted String is turned back into bytes via snapshot_from_store.
    pub fn restore(&mut self, entries: Vec<(PathBuf, Option<String>)>) {
        for (path, original) in entries {
            self.originals
                .insert(path, original.map(|s| snapshot_from_store(&s)));
        }
    }

    /// Produce the "original → current" unified diff and added/removed line counts (both sides decoded to the LF view first).
    pub fn diff(&mut self, cwd: &Path, path: &Path) -> Result<FileChange, String> {
        let original_bytes = self
            .originals
            .get(path)
            .ok_or_else(|| "file not tracked".to_string())?
            .clone()
            .unwrap_or_default();
        let original = decoded_view(&original_bytes);
        let current_bytes =
            std::fs::read(path).map_err(|e| format!("failed to read {}: {e}", path.display()))?;
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
        self.stats
            .insert(path.to_path_buf(), (additions, deletions));
        let cwd_canonical = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
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

    /// Compute and clear "this turn's changes": per file, the net diff of "before the first write of this turn → current disk" (LF view).
    /// A zero net delta (reverted to the original within the turn) produces nothing; deleted files produce nothing for now.
    pub fn take_turn_changes(&mut self, cwd: &Path) -> Vec<FileChange> {
        let entries = std::mem::take(&mut self.turn_originals);
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

    /// Revert: write the original bytes back verbatim (byte-exact for GBK/UTF-16/CRLF); newly created files are deleted.
    pub fn revert(&mut self, path: &Path) -> Result<(), CoreError> {
        let Some(original) = self.originals.remove(path) else {
            return Err(CoreError::RevertNotModified);
        };
        self.stats.remove(path);
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

    #[allow(dead_code)] // will be used by M4 session statistics
    pub fn totals(&self) -> (u32, u32) {
        self.stats
            .values()
            .fold((0, 0), |(a, d), (ta, td)| (a + ta, d + td))
    }

    pub fn tracked_paths(&self) -> Vec<String> {
        self.originals
            .keys()
            .map(|p| p.to_string_lossy().to_string())
            .collect()
    }
}
