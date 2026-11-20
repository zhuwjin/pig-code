use pig_protocol::CoreError;
use rusqlite::params;

use super::Store;

pub struct ExecutionClaim {
    pub request_id: String,
    pub payload: String,
}

impl Store {
    /// Record the current execution's complete input before a turn future can
    /// run. Starting a different turn after reopening replaces the stale claim;
    /// the queue remains the durable owner of any older unconsumed input.
    pub fn claim_execution(
        &self,
        session_id: &str,
        request_id: &str,
        payload: &str,
    ) -> Result<(), CoreError> {
        self.conn
            .execute(
                "INSERT INTO execution_claims (session_id, request_id, payload, created_at_ms)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(session_id) DO UPDATE SET
                    request_id = excluded.request_id,
                    payload = excluded.payload,
                    created_at_ms = excluded.created_at_ms",
                params![
                    session_id,
                    request_id,
                    payload,
                    crate::model_io::now_ms() as i64
                ],
            )
            .map(|_| ())
            .map_err(|error| CoreError::Internal {
                detail: format!("Failed to record execution claim for {session_id}: {error}"),
            })
    }

    pub fn execution_claim(&self, session_id: &str) -> Result<Option<ExecutionClaim>, CoreError> {
        self.conn
            .query_row(
                "SELECT request_id, payload FROM execution_claims WHERE session_id = ?1",
                params![session_id],
                |row| {
                    Ok(ExecutionClaim {
                        request_id: row.get(0)?,
                        payload: row.get(1)?,
                    })
                },
            )
            .map(Some)
            .or_else(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(CoreError::Internal {
                    detail: format!("Failed to read execution claim for {session_id}: {other}"),
                }),
            })
    }

    pub fn execution_receipt(&self, session_id: &str, request_id: &str) -> Result<bool, CoreError> {
        self.conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM execution_receipts WHERE session_id = ?1 AND request_id = ?2)",
                params![session_id, request_id],
                |row| row.get(0),
            )
            .map_err(|error| CoreError::Internal {
                detail: format!("Failed to read execution receipt for {session_id}: {error}"),
            })
    }

    /// A stale completion may clear only the exact claim it owns. The receipt
    /// commits in the same transaction so a consumed input cannot be replayed.
    pub fn clear_execution_claim(
        &self,
        session_id: &str,
        request_id: &str,
    ) -> Result<(), CoreError> {
        let transaction =
            self.conn
                .unchecked_transaction()
                .map_err(|error| CoreError::Internal {
                    detail: format!("Failed to settle execution claim for {session_id}: {error}"),
                })?;
        let cleared = transaction
            .execute(
                "DELETE FROM execution_claims WHERE session_id = ?1 AND request_id = ?2",
                params![session_id, request_id],
            )
            .map_err(|error| CoreError::Internal {
                detail: format!("Failed to clear execution claim for {session_id}: {error}"),
            })?;
        if cleared == 1 {
            transaction
                .execute(
                    "INSERT INTO execution_receipts (session_id, request_id, created_at_ms)
                     VALUES (?1, ?2, ?3) ON CONFLICT DO NOTHING",
                    params![session_id, request_id, crate::model_io::now_ms() as i64],
                )
                .map_err(|error| CoreError::Internal {
                    detail: format!("Failed to record execution receipt for {session_id}: {error}"),
                })?;
        }
        transaction.commit().map_err(|error| CoreError::Internal {
            detail: format!("Failed to commit execution receipt for {session_id}: {error}"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_roundtrips_and_clears_only_its_own_request() {
        let dir = std::env::temp_dir().join(format!("pig-execution-claim-{}", std::process::id()));
        let store = Store::open(&dir).expect("store");
        store
            .claim_execution("s1", "q1", "{\"text\":\"task\"}")
            .expect("claim");
        let claim = store
            .execution_claim("s1")
            .expect("read")
            .expect("claim exists");
        assert_eq!(claim.request_id, "q1");
        assert!(claim.payload.contains("task"));
        store.clear_execution_claim("s1", "q2").expect("other");
        assert!(store.execution_claim("s1").expect("read").is_some());
        assert!(
            !store
                .execution_receipt("s1", "q2")
                .expect("no stale receipt")
        );
        store.clear_execution_claim("s1", "q1").expect("clear");
        assert!(store.execution_claim("s1").expect("read").is_none());
        assert!(store.execution_receipt("s1", "q1").expect("receipt"));
        drop(store);
        let store = Store::open(&dir).expect("reopen store");
        assert!(
            store
                .execution_receipt("s1", "q1")
                .expect("persisted receipt")
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
