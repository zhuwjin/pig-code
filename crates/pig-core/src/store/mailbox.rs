use pig_protocol::CoreError;
use rusqlite::params;

use super::Store;

#[derive(Clone, Debug)]
pub(crate) struct StoredAgentSteer {
    pub(crate) agent_id: String,
    pub(crate) steer: crate::agent::AgentSteer,
}

fn mailbox_error(error: impl std::fmt::Display) -> CoreError {
    CoreError::TaskStore {
        detail: error.to_string(),
    }
}

impl Store {
    pub(crate) fn enqueue_agent_steer(
        &self,
        session_id: &str,
        agent_id: &str,
        run_id: &str,
        steer: &crate::agent::AgentSteer,
    ) -> Result<u64, CoreError> {
        let notification = steer
            .notification
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(mailbox_error)?;
        let created_at_ms = crate::model_io::now_ms();
        self.conn
            .execute(
                "INSERT INTO agent_mailbox (
                    session_id, agent_id, run_id, message_id, text, notification,
                    state, created_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7)",
                params![
                    session_id,
                    agent_id,
                    run_id,
                    steer.id,
                    steer.text,
                    notification,
                    created_at_ms as i64
                ],
            )
            .map_err(mailbox_error)?;
        Ok(created_at_ms)
    }

    pub(crate) fn mark_agent_steer_consumed(
        &self,
        session_id: &str,
        message_id: &str,
    ) -> Result<(), CoreError> {
        self.conn
            .execute(
                "UPDATE agent_mailbox SET state = 'consumed', consumed_at_ms = ?3
                 WHERE session_id = ?1 AND message_id = ?2 AND state = 'pending'",
                params![session_id, message_id, crate::model_io::now_ms() as i64],
            )
            .map_err(mailbox_error)?;
        Ok(())
    }

    pub(crate) fn pending_agent_steers(
        &self,
        session_id: &str,
        agent_id: &str,
    ) -> Result<Vec<StoredAgentSteer>, CoreError> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT agent_id, run_id, message_id, text, notification, created_at_ms
                 FROM agent_mailbox
                 WHERE session_id = ?1 AND agent_id = ?2 AND state = 'pending'
                 ORDER BY created_at_ms, message_id",
            )
            .map_err(mailbox_error)?;
        let rows = statement
            .query_map(params![session_id, agent_id], |row| {
                let notification = row
                    .get::<_, Option<String>>(4)?
                    .map(|raw| serde_json::from_str(&raw))
                    .transpose()
                    .map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            4,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?;
                let id = row.get::<_, String>(2)?;
                Ok(StoredAgentSteer {
                    agent_id: row.get(0)?,
                    steer: crate::agent::AgentSteer::restored(id, row.get(3)?, notification),
                })
            })
            .map_err(mailbox_error)?;
        rows.map(|row| row.map_err(mailbox_error)).collect()
    }

    pub(crate) fn delete_agent_mailbox(&self, session_id: &str) -> Result<(), CoreError> {
        self.conn
            .execute(
                "DELETE FROM agent_mailbox WHERE session_id = ?1",
                params![session_id],
            )
            .map_err(mailbox_error)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_steer_survives_reopen_and_consumption_is_idempotent() {
        let dir = std::env::temp_dir().join(format!(
            "pig-mailbox-store-{}-{}",
            std::process::id(),
            crate::model_io::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).expect("open store");
        let steer = crate::agent::AgentSteer::message("continue after restart".into());
        store
            .enqueue_agent_steer("s1", "a1", "run-1", &steer)
            .expect("enqueue");
        store
            .enqueue_agent_steer("s1", "a1", "run-1", &steer)
            .expect_err("message ids are unique");
        drop(store);

        let store = Store::open(&dir).expect("reopen store");
        let pending = store.pending_agent_steers("s1", "a1").expect("pending");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].steer.id, steer.id);
        assert_eq!(pending[0].steer.text, "continue after restart");
        store
            .mark_agent_steer_consumed("s1", &steer.id)
            .expect("consume");
        store
            .mark_agent_steer_consumed("s1", &steer.id)
            .expect("idempotent consume");
        assert!(
            store
                .pending_agent_steers("s1", "a1")
                .expect("empty")
                .is_empty()
        );
        store.delete_agent_mailbox("s1").expect("delete");
        assert!(
            store
                .pending_agent_steers("s1", "a1")
                .expect("deleted")
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
