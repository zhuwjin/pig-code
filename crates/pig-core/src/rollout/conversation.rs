use std::path::{Path, PathBuf};

use pig_protocol::CoreError;

use super::{Rollout, RolloutRecord};

/// Shared raw-record persistence boundary for root rollouts and child JSONL
/// contexts. Compaction remains an append-only `Compact` record; replay and
/// model projection stay above this interface.
pub(crate) trait ConversationStore {
    fn path(&self) -> &Path;
    fn load(&self) -> Result<Vec<RolloutRecord>, CoreError>;
    fn append(&mut self, record: &RolloutRecord) -> Result<(), CoreError>;
    fn sync(&mut self) -> Result<(), CoreError>;

    fn media_root(&self) -> PathBuf {
        self.path().with_extension("media")
    }
}

impl ConversationStore for Rollout {
    fn path(&self) -> &Path {
        Rollout::path(self)
    }

    fn load(&self) -> Result<Vec<RolloutRecord>, CoreError> {
        Rollout::load(self.path())
    }

    fn append(&mut self, record: &RolloutRecord) -> Result<(), CoreError> {
        let path = self.path().to_path_buf();
        Rollout::append(self, record).map_err(|error| error.into_core(&path))
    }

    fn sync(&mut self) -> Result<(), CoreError> {
        let path = self.path().to_path_buf();
        Rollout::sync(self).map_err(|error| error.into_core(&path))
    }
}
