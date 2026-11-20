use std::path::{Path, PathBuf};

use pig_protocol::CoreError;

use crate::rollout::{ConversationStore, Rollout, RolloutRecord};

/// Raw JSONL store for one child context. Message/media projection remains in
/// the child record functions; this type owns only durable record operations.
pub(crate) struct AgentConversationStore {
    path: PathBuf,
    limit_bytes: u64,
}

impl AgentConversationStore {
    pub(crate) fn new(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            limit_bytes: crate::rollout::DEFAULT_HISTORY_LIMIT_BYTES,
        }
    }

    #[cfg(test)]
    fn with_limit(path: &Path, limit_bytes: u64) -> Self {
        Self {
            path: path.to_path_buf(),
            limit_bytes,
        }
    }
}

impl ConversationStore for AgentConversationStore {
    fn path(&self) -> &Path {
        &self.path
    }

    fn load(&self) -> Result<Vec<RolloutRecord>, CoreError> {
        Rollout::load_snapshot(&self.path)
    }

    fn append(&mut self, record: &RolloutRecord) -> Result<(), CoreError> {
        crate::rollout::ensure_history_capacity(&self.path, record, self.limit_bytes)?;
        super::jsonl::append_agent_record(
            &self.path,
            &serde_json::to_value(record).map_err(|error| CoreError::RolloutWrite {
                path: self.path.display().to_string(),
                detail: error.to_string(),
            })?,
        )
        .map_err(|detail| CoreError::RolloutWrite {
            path: self.path.display().to_string(),
            detail,
        })
    }

    fn sync(&mut self) -> Result<(), CoreError> {
        std::fs::OpenOptions::new()
            .write(true)
            .open(&self.path)
            .and_then(|file| file.sync_all())
            .map_err(|error| CoreError::RolloutWrite {
                path: self.path.display().to_string(),
                detail: error.to_string(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_history_quota_preserves_all_prior_records() {
        let dir = std::env::temp_dir().join(format!(
            "pig-core-child-history-quota-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("a1.jsonl");
        let mut store = AgentConversationStore::with_limit(&path, 1024);
        let record = RolloutRecord::Text {
            text: "first".into(),
        };
        store.append(&record).expect("first record");
        store.sync().expect("sync");
        let original = std::fs::read(&path).expect("original history");
        let error = store
            .append(&RolloutRecord::Text {
                text: "x".repeat(2048),
            })
            .expect_err("quota rejects a child record");
        assert!(matches!(error, CoreError::HistoryLimitExceeded { .. }));
        assert_eq!(std::fs::read(&path).expect("preserved history"), original);
        assert!(
            matches!(store.load().expect("replay").as_slice(), [RolloutRecord::Text { text }] if text == "first")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
