use std::path::{Path, PathBuf};

use pig_protocol::CoreError;

use crate::rollout::{ConversationStore, Rollout, RolloutRecord};

/// Raw JSONL store for one child context. Message/media projection remains in
/// the child record functions; this type owns only durable record operations.
pub(crate) struct AgentConversationStore {
    path: PathBuf,
}

impl AgentConversationStore {
    pub(crate) fn new(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
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
