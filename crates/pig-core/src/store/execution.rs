use pig_protocol::CoreError;
use rusqlite::params;
use serde::{Deserialize, Serialize};

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

    /// A stale completion may clear only the exact claim it owns.
    pub fn clear_execution_claim(
        &self,
        session_id: &str,
        request_id: &str,
    ) -> Result<(), CoreError> {
        self.conn
            .execute(
                "DELETE FROM execution_claims WHERE session_id = ?1 AND request_id = ?2",
                params![session_id, request_id],
            )
            .map(|_| ())
            .map_err(|error| CoreError::Internal {
                detail: format!("Failed to clear execution claim for {session_id}: {error}"),
            })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolExecutionResult {
    pub output: String,
    pub is_error: bool,
    pub edit: Option<pig_protocol::EditDiff>,
    pub images: Vec<pig_provider::ChatImage>,
    pub documents: Vec<pig_provider::ChatDocument>,
}

#[derive(Clone, Debug)]
pub enum ToolExecutionStart {
    Started,
    Completed(ToolExecutionResult),
    Blocked(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownToolExecution {
    pub execution_id: String,
    pub tool: String,
}

struct ToolExecutionRow {
    tool: String,
    arguments: String,
    state: String,
    payload: Option<String>,
}

fn tool_error(action: &str, error: impl std::fmt::Display) -> CoreError {
    CoreError::Internal {
        detail: format!("Tool execution store {action}: {error}"),
    }
}

impl Store {
    fn tool_execution_row(
        &self,
        session_id: &str,
        execution_id: &str,
    ) -> Result<Option<ToolExecutionRow>, CoreError> {
        self.conn
            .query_row(
                "SELECT tool, arguments, state, payload FROM tool_executions
                 WHERE session_id = ?1 AND execution_id = ?2",
                params![session_id, execution_id],
                |row| {
                    Ok(ToolExecutionRow {
                        tool: row.get(0)?,
                        arguments: row.get(1)?,
                        state: row.get(2)?,
                        payload: row.get(3)?,
                    })
                },
            )
            .map(Some)
            .or_else(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(tool_error("read", other)),
            })
    }

    pub fn begin_tool_execution(
        &self,
        session_id: &str,
        execution_id: &str,
        tool: &str,
        arguments: &str,
    ) -> Result<ToolExecutionStart, CoreError> {
        let existing = self.tool_execution_row(session_id, execution_id)?;
        let Some(existing) = existing else {
            self.conn
                .execute(
                    "INSERT INTO tool_executions (
                        session_id, execution_id, tool, arguments, state, created_at_ms
                     ) VALUES (?1, ?2, ?3, ?4, 'running', ?5)",
                    params![
                        session_id,
                        execution_id,
                        tool,
                        arguments,
                        crate::model_io::now_ms() as i64
                    ],
                )
                .map_err(|error| tool_error("start", error))?;
            return Ok(ToolExecutionStart::Started);
        };
        if existing.tool != tool || existing.arguments != arguments {
            return Ok(ToolExecutionStart::Blocked(
                "This execution id was already used for a different tool call".to_string(),
            ));
        }
        match existing.state.as_str() {
            "completed" => {
                let payload = existing.payload.ok_or_else(|| {
                    tool_error("read", "completed execution has no result payload")
                })?;
                let result = serde_json::from_str(&payload).map_err(|error| tool_error("decode", error))?;
                Ok(ToolExecutionStart::Completed(result))
            }
            "running" => Ok(ToolExecutionStart::Blocked(
                "This tool execution is already running; wait for its existing result".to_string(),
            )),
            "unknown" => Ok(ToolExecutionStart::Blocked(
                "This tool execution has an unknown outcome after restart; verify its side effects before retrying with a new call".to_string(),
            )),
            other => Err(tool_error("read", format!("unknown state {other}"))),
        }
    }

    pub fn finish_tool_execution(
        &self,
        session_id: &str,
        execution_id: &str,
        result: &ToolExecutionResult,
    ) -> Result<(), CoreError> {
        let payload = serde_json::to_string(result).map_err(|error| tool_error("encode", error))?;
        self.conn
            .execute(
                "UPDATE tool_executions SET state = 'completed', payload = ?3, ended_at_ms = ?4
                 WHERE session_id = ?1 AND execution_id = ?2 AND state = 'running'",
                params![
                    session_id,
                    execution_id,
                    payload,
                    crate::model_io::now_ms() as i64
                ],
            )
            .map(|_| ())
            .map_err(|error| tool_error("finish", error))
    }

    pub fn recover_tool_executions(&self, session_id: &str) -> Result<(), CoreError> {
        self.conn
            .execute(
                "UPDATE tool_executions SET state = 'unknown', ended_at_ms = ?2
                 WHERE session_id = ?1 AND state = 'running'",
                params![session_id, crate::model_io::now_ms() as i64],
            )
            .map(|_| ())
            .map_err(|error| tool_error("recover", error))
    }

    pub fn unknown_tool_executions(
        &self,
        session_id: &str,
    ) -> Result<Vec<UnknownToolExecution>, CoreError> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT execution_id, tool FROM tool_executions
                 WHERE session_id = ?1 AND state = 'unknown' ORDER BY created_at_ms, execution_id",
            )
            .map_err(|error| tool_error("list unknown", error))?;
        let rows = statement
            .query_map(params![session_id], |row| {
                Ok(UnknownToolExecution {
                    execution_id: row.get(0)?,
                    tool: row.get(1)?,
                })
            })
            .map_err(|error| tool_error("list unknown", error))?;
        rows.map(|row| row.map_err(|error| tool_error("list unknown", error)))
            .collect()
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
        store.clear_execution_claim("s1", "q1").expect("clear");
        assert!(store.execution_claim("s1").expect("read").is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn tool_results_replay_only_after_completion_and_crash_becomes_unknown() {
        let dir = std::env::temp_dir().join(format!("pig-tool-journal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).expect("store");
        let result = ToolExecutionResult {
            output: "saved".into(),
            is_error: false,
            edit: None,
            images: vec![],
            documents: vec![],
        };
        assert!(matches!(
            store.begin_tool_execution("s1", "call-1", "Read", "{}"),
            Ok(ToolExecutionStart::Started)
        ));
        store
            .finish_tool_execution("s1", "call-1", &result)
            .expect("finish");
        assert!(matches!(
            store.begin_tool_execution("s1", "call-1", "Read", "{}"),
            Ok(ToolExecutionStart::Completed(saved)) if saved.output == "saved"
        ));

        store
            .begin_tool_execution("s1", "call-2", "ExecCommand", "{}")
            .expect("second call");
        store.recover_tool_executions("s1").expect("recovery");
        let blocked = store
            .begin_tool_execution("s1", "call-2", "ExecCommand", "{}")
            .expect("unknown is not replayed");
        assert!(
            matches!(blocked, ToolExecutionStart::Blocked(ref note) if note.contains("unknown")),
            "{blocked:?}"
        );
        assert_eq!(
            store.unknown_tool_executions("s1").expect("unknown").len(),
            1
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
