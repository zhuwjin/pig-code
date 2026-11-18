use pig_protocol::CoreError;
use rusqlite::params;

use super::Store;

fn deletion_error(action: &str, session_id: &str, error: rusqlite::Error) -> CoreError {
    CoreError::TaskStore {
        detail: format!("Failed to {action} for session {session_id}: {error}"),
    }
}

impl Store {
    pub fn request_session_deletion(&self, session_id: &str) -> Result<(), CoreError> {
        self.conn
            .execute(
                "INSERT INTO session_deletions (session_id, requested_at_ms)
                 VALUES (?1, ?2) ON CONFLICT(session_id) DO NOTHING",
                params![session_id, crate::model_io::now_ms() as i64],
            )
            .map(|_| ())
            .map_err(|error| deletion_error("request session deletion", session_id, error))
    }

    pub fn pending_session_deletions(&self) -> Result<Vec<String>, CoreError> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT session_id FROM session_deletions ORDER BY requested_at_ms, session_id",
            )
            .map_err(|error| CoreError::TaskStore {
                detail: format!("Failed to read pending session deletions: {error}"),
            })?;
        let rows =
            statement
                .query_map([], |row| row.get(0))
                .map_err(|error| CoreError::TaskStore {
                    detail: format!("Failed to read pending session deletions: {error}"),
                })?;
        rows.collect::<Result<Vec<String>, _>>()
            .map_err(|error| CoreError::TaskStore {
                detail: format!("Failed to decode pending session deletions: {error}"),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deletion_request_survives_reopen_and_final_commit_clears_it() {
        let root =
            std::env::temp_dir().join(crate::agent::runtime::new_runtime_id("deletion-store"));
        let mut store = Store::open(&root).expect("open store");
        store
            .request_session_deletion("s1")
            .expect("request deletion");
        store
            .request_session_deletion("s1")
            .expect("idempotent request");
        drop(store);
        let mut store = Store::open(&root).expect("reopen store");
        assert_eq!(store.pending_session_deletions().expect("pending"), ["s1"]);
        store.delete_session_checked("s1").expect("commit deletion");
        assert!(
            store
                .pending_session_deletions()
                .expect("cleared")
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn final_delete_requires_a_durable_request() {
        let root =
            std::env::temp_dir().join(crate::agent::runtime::new_runtime_id("deletion-guard"));
        let mut store = Store::open(&root).expect("open store");
        assert!(store.delete_session_checked("s1").is_err());
        let _ = std::fs::remove_dir_all(root);
    }
}
