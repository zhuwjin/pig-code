use std::path::PathBuf;

use rusqlite::{Connection, params};

use super::Store;
use crate::agent::runtime::{
    AgentInitiator, ExecutionAttempt, MailboxDeliveryState, MailboxMessageKind,
    RUNTIME_SCHEMA_VERSION, RunWatchdogAction, RunWatchdogKind, RuntimeAgent,
    RuntimeConnectionState, RuntimeControlEvent, RuntimeError, RuntimeMailboxMessage, RuntimeRun,
    RuntimeRunStatus, RuntimeSnapshot,
};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS runtime_agents (
    session_id TEXT NOT NULL,
    agent_id TEXT NOT NULL,
    parent_agent_id TEXT,
    depth INTEGER NOT NULL,
    profile TEXT NOT NULL,
    conversation_path TEXT,
    created_at_ms INTEGER NOT NULL,
    closed_at_ms INTEGER,
    PRIMARY KEY (session_id, agent_id)
);
CREATE INDEX IF NOT EXISTS idx_runtime_agents_parent
    ON runtime_agents(session_id, parent_agent_id);
CREATE TABLE IF NOT EXISTS runtime_runs (
    session_id TEXT NOT NULL,
    run_id TEXT NOT NULL,
    agent_id TEXT NOT NULL,
    backend_id TEXT NOT NULL,
    initiator TEXT NOT NULL,
    idempotency_key TEXT,
    causation_id TEXT,
    correlation_id TEXT,
    status TEXT NOT NULL,
    stop_reason TEXT,
    input_tokens INTEGER NOT NULL,
    cache_read_tokens INTEGER NOT NULL,
    output_tokens INTEGER NOT NULL,
    turns INTEGER NOT NULL,
    started_at_ms INTEGER NOT NULL,
    ended_at_ms INTEGER,
    deadline_at_ms INTEGER,
    idle_timeout_ms INTEGER,
    last_activity_at_ms INTEGER NOT NULL,
    watchdog_action TEXT NOT NULL,
    watchdog_alerts TEXT NOT NULL,
    PRIMARY KEY (session_id, run_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_runtime_runs_idempotency
    ON runtime_runs(session_id, idempotency_key)
    WHERE idempotency_key IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_runtime_runs_agent
    ON runtime_runs(session_id, agent_id, started_at_ms);
CREATE TABLE IF NOT EXISTS runtime_attempts (
    session_id TEXT NOT NULL,
    run_id TEXT NOT NULL,
    attempt INTEGER NOT NULL,
    backend_epoch TEXT NOT NULL,
    connection_state TEXT NOT NULL,
    last_event_seq INTEGER NOT NULL,
    released_at_ms INTEGER,
    PRIMARY KEY (session_id, run_id)
);
CREATE TABLE IF NOT EXISTS runtime_mailbox (
    session_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    target_agent_id TEXT NOT NULL,
    target_run_id TEXT,
    kind TEXT NOT NULL,
    payload TEXT NOT NULL,
    notification_id TEXT,
    state TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    claimed_at_ms INTEGER,
    consumed_at_ms INTEGER,
    PRIMARY KEY (session_id, message_id)
);
CREATE INDEX IF NOT EXISTS idx_runtime_mailbox_pending
    ON runtime_mailbox(session_id, target_agent_id, state, created_at_ms);
CREATE UNIQUE INDEX IF NOT EXISTS idx_runtime_mailbox_notification
    ON runtime_mailbox(session_id, notification_id)
    WHERE notification_id IS NOT NULL;
CREATE TABLE IF NOT EXISTS runtime_events (
    session_id TEXT NOT NULL,
    sequence INTEGER NOT NULL,
    event_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    agent_id TEXT,
    run_id TEXT,
    attempt INTEGER,
    backend_epoch TEXT,
    payload TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    PRIMARY KEY (session_id, sequence),
    UNIQUE (session_id, event_id)
);
CREATE TABLE IF NOT EXISTS runtime_meta (
    session_id TEXT PRIMARY KEY,
    schema_version INTEGER NOT NULL
);
";

pub(super) fn create_schema(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(SCHEMA)
        .map_err(|error| format!("runtime store create-table failed: {error}"))
}

fn store_error(error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Store(error.to_string())
}

fn json_error(
    column: usize,
    error: impl std::error::Error + Send + Sync + 'static,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(column, rusqlite::types::Type::Text, Box::new(error))
}

fn text_error(column: usize, detail: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(column, rusqlite::types::Type::Text, detail.into())
}

fn as_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn as_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

fn write_event(conn: &Connection, event: &RuntimeControlEvent) -> Result<(), RuntimeError> {
    conn.execute(
        "INSERT INTO runtime_events (
            session_id, sequence, event_id, kind, agent_id, run_id, attempt,
            backend_epoch, payload, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            event.session_id,
            as_i64(event.sequence),
            event.event_id,
            event.kind.as_str(),
            event.agent_id,
            event.run_id,
            event.attempt.map(as_i64),
            event.backend_epoch,
            serde_json::to_string(&event.payload).map_err(store_error)?,
            as_i64(event.created_at_ms),
        ],
    )
    .map_err(store_error)?;
    Ok(())
}

fn write_agent(conn: &Connection, agent: &RuntimeAgent) -> Result<(), RuntimeError> {
    conn.execute(
        "INSERT INTO runtime_agents (
            session_id, agent_id, parent_agent_id, depth, profile,
            conversation_path, created_at_ms, closed_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(session_id, agent_id) DO UPDATE SET
            parent_agent_id = excluded.parent_agent_id,
            depth = excluded.depth,
            profile = excluded.profile,
            conversation_path = excluded.conversation_path,
            closed_at_ms = excluded.closed_at_ms",
        params![
            agent.session_id,
            agent.agent_id,
            agent.parent_agent_id,
            i64::from(agent.depth),
            agent.profile,
            agent
                .conversation_path
                .as_ref()
                .map(|path| path.display().to_string()),
            as_i64(agent.created_at_ms),
            agent.closed_at_ms.map(as_i64),
        ],
    )
    .map_err(store_error)?;
    Ok(())
}

fn write_run(conn: &Connection, run: &RuntimeRun) -> Result<(), RuntimeError> {
    conn.execute(
        "INSERT INTO runtime_runs (
            session_id, run_id, agent_id, backend_id, initiator, idempotency_key,
            causation_id, correlation_id, status, stop_reason, input_tokens,
            cache_read_tokens, output_tokens, turns, started_at_ms, ended_at_ms,
            deadline_at_ms, idle_timeout_ms, last_activity_at_ms, watchdog_action,
            watchdog_alerts
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                   ?17, ?18, ?19, ?20, ?21)",
        params![
            run.session_id,
            run.run_id,
            run.agent_id,
            run.backend_id,
            serde_json::to_string(&run.initiator).map_err(store_error)?,
            run.idempotency_key,
            run.causation_id,
            run.correlation_id,
            run.status.as_str(),
            run.stop_reason,
            as_i64(run.usage.0),
            as_i64(run.usage.1),
            as_i64(run.usage.2),
            as_i64(run.turns as u64),
            as_i64(run.started_at_ms),
            run.ended_at_ms.map(as_i64),
            run.deadline_at_ms.map(as_i64),
            run.idle_timeout_ms.map(as_i64),
            as_i64(run.last_activity_at_ms),
            run.watchdog_action.as_str(),
            serde_json::to_string(&run.watchdog_alerts).map_err(store_error)?,
        ],
    )
    .map_err(store_error)?;
    Ok(())
}

fn update_run(conn: &Connection, run: &RuntimeRun) -> Result<(), RuntimeError> {
    conn.execute(
        "UPDATE runtime_runs SET status = ?3, stop_reason = ?4, input_tokens = ?5,
            cache_read_tokens = ?6, output_tokens = ?7, turns = ?8, ended_at_ms = ?9,
            deadline_at_ms = ?10, idle_timeout_ms = ?11, last_activity_at_ms = ?12,
            watchdog_action = ?13, watchdog_alerts = ?14
         WHERE session_id = ?1 AND run_id = ?2",
        params![
            run.session_id,
            run.run_id,
            run.status.as_str(),
            run.stop_reason,
            as_i64(run.usage.0),
            as_i64(run.usage.1),
            as_i64(run.usage.2),
            as_i64(run.turns as u64),
            run.ended_at_ms.map(as_i64),
            run.deadline_at_ms.map(as_i64),
            run.idle_timeout_ms.map(as_i64),
            as_i64(run.last_activity_at_ms),
            run.watchdog_action.as_str(),
            serde_json::to_string(&run.watchdog_alerts).map_err(store_error)?,
        ],
    )
    .map_err(store_error)?;
    Ok(())
}

fn write_attempt(conn: &Connection, attempt: &ExecutionAttempt) -> Result<(), RuntimeError> {
    conn.execute(
        "INSERT INTO runtime_attempts (
            session_id, run_id, attempt, backend_epoch, connection_state, last_event_seq,
            released_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            attempt.session_id,
            attempt.run_id,
            as_i64(attempt.attempt),
            attempt.backend_epoch,
            attempt.connection_state.as_str(),
            as_i64(attempt.last_event_seq),
            attempt.released_at_ms.map(as_i64),
        ],
    )
    .map_err(store_error)?;
    Ok(())
}

fn update_attempt(conn: &Connection, attempt: &ExecutionAttempt) -> Result<(), RuntimeError> {
    conn.execute(
        "UPDATE runtime_attempts SET attempt = ?3, backend_epoch = ?4,
            connection_state = ?5, last_event_seq = ?6, released_at_ms = ?7
         WHERE session_id = ?1 AND run_id = ?2",
        params![
            attempt.session_id,
            attempt.run_id,
            as_i64(attempt.attempt),
            attempt.backend_epoch,
            attempt.connection_state.as_str(),
            as_i64(attempt.last_event_seq),
            attempt.released_at_ms.map(as_i64),
        ],
    )
    .map_err(store_error)?;
    Ok(())
}

fn write_mailbox(conn: &Connection, message: &RuntimeMailboxMessage) -> Result<(), RuntimeError> {
    conn.execute(
        "INSERT INTO runtime_mailbox (
            session_id, message_id, target_agent_id, target_run_id, kind, payload,
            notification_id, state, created_at_ms, claimed_at_ms, consumed_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT(session_id, message_id) DO UPDATE SET
            target_run_id = excluded.target_run_id,
            kind = excluded.kind,
            payload = excluded.payload,
            notification_id = excluded.notification_id,
            state = excluded.state,
            claimed_at_ms = excluded.claimed_at_ms,
            consumed_at_ms = excluded.consumed_at_ms",
        params![
            message.session_id,
            message.message_id,
            message.target_agent_id,
            message.target_run_id,
            message.kind.as_str(),
            serde_json::to_string(&message.payload).map_err(store_error)?,
            message.notification_id,
            message.state.as_str(),
            as_i64(message.created_at_ms),
            message.claimed_at_ms.map(as_i64),
            message.consumed_at_ms.map(as_i64),
        ],
    )
    .map_err(store_error)?;
    Ok(())
}

impl Store {
    pub(crate) fn commit_runtime_agent(
        &mut self,
        agent: &RuntimeAgent,
        event: &RuntimeControlEvent,
    ) -> Result<(), RuntimeError> {
        let transaction = self.conn.transaction().map_err(store_error)?;
        write_agent(&transaction, agent)?;
        write_event(&transaction, event)?;
        transaction
            .execute(
                "INSERT INTO runtime_meta (session_id, schema_version) VALUES (?1, ?2)
             ON CONFLICT(session_id) DO UPDATE SET schema_version = excluded.schema_version",
                params![agent.session_id, i64::from(RUNTIME_SCHEMA_VERSION)],
            )
            .map_err(store_error)?;
        transaction.commit().map_err(store_error)
    }

    pub(crate) fn commit_runtime_run(
        &mut self,
        run: &RuntimeRun,
        attempt: &ExecutionAttempt,
        event: &RuntimeControlEvent,
    ) -> Result<(), RuntimeError> {
        let transaction = self.conn.transaction().map_err(store_error)?;
        write_run(&transaction, run)?;
        write_attempt(&transaction, attempt)?;
        write_event(&transaction, event)?;
        transaction.commit().map_err(store_error)
    }

    pub(crate) fn commit_runtime_run_update(
        &mut self,
        run: &RuntimeRun,
        event: &RuntimeControlEvent,
    ) -> Result<(), RuntimeError> {
        let transaction = self.conn.transaction().map_err(store_error)?;
        update_run(&transaction, run)?;
        write_event(&transaction, event)?;
        transaction.commit().map_err(store_error)
    }

    pub(crate) fn commit_runtime_mailbox(
        &mut self,
        message: &RuntimeMailboxMessage,
        event: &RuntimeControlEvent,
    ) -> Result<(), RuntimeError> {
        let transaction = self.conn.transaction().map_err(store_error)?;
        write_mailbox(&transaction, message)?;
        write_event(&transaction, event)?;
        transaction.commit().map_err(store_error)
    }

    pub(crate) fn commit_runtime_attempt(
        &mut self,
        attempt: &ExecutionAttempt,
        event: &RuntimeControlEvent,
    ) -> Result<(), RuntimeError> {
        let transaction = self.conn.transaction().map_err(store_error)?;
        update_attempt(&transaction, attempt)?;
        write_event(&transaction, event)?;
        transaction.commit().map_err(store_error)
    }

    pub(crate) fn commit_runtime_task_mailbox(
        &mut self,
        session_id: &str,
        task: &super::tasks::BackgroundTask,
        message: &RuntimeMailboxMessage,
        event: &RuntimeControlEvent,
    ) -> Result<(), RuntimeError> {
        let transaction = self.conn.transaction().map_err(store_error)?;
        super::tasks::write_background_task(&transaction, session_id, task)
            .map_err(|error| RuntimeError::Store(pig_provider::core_error_en(&error)))?;
        write_mailbox(&transaction, message)?;
        write_event(&transaction, event)?;
        transaction.commit().map_err(store_error)
    }

    pub(crate) fn commit_runtime_root_notification(
        &mut self,
        session_id: &str,
        task: &super::tasks::BackgroundTask,
        notification: &pig_protocol::TaskNotification,
        message: Option<&RuntimeMailboxMessage>,
        event: Option<&RuntimeControlEvent>,
    ) -> Result<(), RuntimeError> {
        let transaction = self.conn.transaction().map_err(store_error)?;
        super::tasks::write_background_task(&transaction, session_id, task)
            .map_err(|error| RuntimeError::Store(pig_provider::core_error_en(&error)))?;
        if task.detached && !task.terminal_notification_suppressed {
            super::tasks::schedule_notification_on(&transaction, session_id, notification)
                .map_err(|error| RuntimeError::Store(pig_provider::core_error_en(&error)))?;
        }
        if let Some(message) = message {
            write_mailbox(&transaction, message)?;
        }
        if let Some(event) = event {
            write_event(&transaction, event)?;
        }
        transaction.commit().map_err(store_error)
    }

    pub(crate) fn update_runtime_activity(
        &self,
        session_id: &str,
        run_id: &str,
        activity_at_ms: u64,
    ) -> Result<(), RuntimeError> {
        self.conn
            .execute(
                "UPDATE runtime_runs SET last_activity_at_ms = ?3
                 WHERE session_id = ?1 AND run_id = ?2",
                params![session_id, run_id, as_i64(activity_at_ms)],
            )
            .map_err(store_error)?;
        Ok(())
    }

    pub(crate) fn runtime_snapshot(
        &self,
        session_id: &str,
    ) -> Result<RuntimeSnapshot, RuntimeError> {
        let mut snapshot = RuntimeSnapshot::default();
        let mut agents = self
            .conn
            .prepare(
                "SELECT agent_id, parent_agent_id, depth, profile, conversation_path,
                    created_at_ms, closed_at_ms
             FROM runtime_agents WHERE session_id = ?1 ORDER BY created_at_ms, agent_id",
            )
            .map_err(store_error)?;
        let agent_rows = agents
            .query_map(params![session_id], |row| {
                Ok(RuntimeAgent {
                    session_id: session_id.to_string(),
                    agent_id: row.get(0)?,
                    parent_agent_id: row.get(1)?,
                    depth: u32::try_from(row.get::<_, i64>(2)?).unwrap_or(0),
                    profile: row.get(3)?,
                    conversation_path: row.get::<_, Option<String>>(4)?.map(PathBuf::from),
                    created_at_ms: as_u64(row.get(5)?),
                    closed_at_ms: row.get::<_, Option<i64>>(6)?.map(as_u64),
                })
            })
            .map_err(store_error)?;
        for agent in agent_rows {
            snapshot.agents.push(agent.map_err(store_error)?);
        }
        drop(agents);

        let mut runs = self
            .conn
            .prepare(
                "SELECT run_id, agent_id, backend_id, initiator, idempotency_key,
                    causation_id, correlation_id, status, stop_reason, input_tokens,
                    cache_read_tokens, output_tokens, turns, started_at_ms, ended_at_ms,
                    deadline_at_ms, idle_timeout_ms, last_activity_at_ms, watchdog_action,
                    watchdog_alerts
             FROM runtime_runs WHERE session_id = ?1 ORDER BY started_at_ms, run_id",
            )
            .map_err(store_error)?;
        let run_rows = runs
            .query_map(params![session_id], |row| {
                let status_text = row.get::<_, String>(7)?;
                let status = RuntimeRunStatus::from_str(&status_text).ok_or_else(|| {
                    text_error(7, format!("unknown runtime run status: {status_text}"))
                })?;
                let initiator_text = row.get::<_, String>(3)?;
                let initiator: AgentInitiator =
                    serde_json::from_str(&initiator_text).map_err(|error| json_error(3, error))?;
                let watchdog_action_text = row.get::<_, String>(18)?;
                let watchdog_action = RunWatchdogAction::from_str(&watchdog_action_text)
                    .ok_or_else(|| {
                        text_error(
                            18,
                            format!("unknown watchdog action: {watchdog_action_text}"),
                        )
                    })?;
                let watchdog_alerts_text = row.get::<_, String>(19)?;
                let watchdog_alerts: std::collections::BTreeSet<RunWatchdogKind> =
                    serde_json::from_str(&watchdog_alerts_text)
                        .map_err(|error| json_error(19, error))?;
                Ok(RuntimeRun {
                    session_id: session_id.to_string(),
                    run_id: row.get(0)?,
                    agent_id: row.get(1)?,
                    backend_id: row.get(2)?,
                    initiator,
                    idempotency_key: row.get(4)?,
                    causation_id: row.get(5)?,
                    correlation_id: row.get(6)?,
                    status,
                    stop_reason: row.get(8)?,
                    usage: (
                        as_u64(row.get(9)?),
                        as_u64(row.get(10)?),
                        as_u64(row.get(11)?),
                    ),
                    turns: as_u64(row.get(12)?) as usize,
                    started_at_ms: as_u64(row.get(13)?),
                    ended_at_ms: row.get::<_, Option<i64>>(14)?.map(as_u64),
                    deadline_at_ms: row.get::<_, Option<i64>>(15)?.map(as_u64),
                    idle_timeout_ms: row.get::<_, Option<i64>>(16)?.map(as_u64),
                    last_activity_at_ms: as_u64(row.get(17)?),
                    watchdog_action,
                    watchdog_alerts,
                })
            })
            .map_err(store_error)?;
        for run in run_rows {
            snapshot.runs.push(run.map_err(store_error)?);
        }
        drop(runs);

        let mut attempts = self
            .conn
            .prepare(
                "SELECT run_id, attempt, backend_epoch, connection_state, last_event_seq,
                    released_at_ms
             FROM runtime_attempts WHERE session_id = ?1",
            )
            .map_err(store_error)?;
        let attempt_rows = attempts
            .query_map(params![session_id], |row| {
                let state_text = row.get::<_, String>(3)?;
                let connection_state =
                    RuntimeConnectionState::from_str(&state_text).ok_or_else(|| {
                        text_error(3, format!("unknown connection state: {state_text}"))
                    })?;
                Ok(ExecutionAttempt {
                    session_id: session_id.to_string(),
                    run_id: row.get(0)?,
                    attempt: as_u64(row.get(1)?),
                    backend_epoch: row.get(2)?,
                    connection_state,
                    last_event_seq: as_u64(row.get(4)?),
                    released_at_ms: row.get::<_, Option<i64>>(5)?.map(as_u64),
                })
            })
            .map_err(store_error)?;
        for attempt in attempt_rows {
            snapshot.attempts.push(attempt.map_err(store_error)?);
        }
        drop(attempts);

        let mut mailbox = self
            .conn
            .prepare(
                "SELECT message_id, target_agent_id, target_run_id, kind, payload,
                    notification_id, state, created_at_ms, claimed_at_ms, consumed_at_ms
             FROM runtime_mailbox WHERE session_id = ?1 ORDER BY created_at_ms, message_id",
            )
            .map_err(store_error)?;
        let mailbox_rows = mailbox
            .query_map(params![session_id], |row| {
                let kind_text = row.get::<_, String>(3)?;
                let kind = MailboxMessageKind::from_str(&kind_text)
                    .ok_or_else(|| text_error(3, format!("unknown mailbox kind: {kind_text}")))?;
                let payload_text = row.get::<_, String>(4)?;
                let payload =
                    serde_json::from_str(&payload_text).map_err(|error| json_error(4, error))?;
                let state_text = row.get::<_, String>(6)?;
                let state = MailboxDeliveryState::from_str(&state_text)
                    .ok_or_else(|| text_error(6, format!("unknown mailbox state: {state_text}")))?;
                Ok(RuntimeMailboxMessage {
                    session_id: session_id.to_string(),
                    message_id: row.get(0)?,
                    target_agent_id: row.get(1)?,
                    target_run_id: row.get(2)?,
                    kind,
                    payload,
                    notification_id: row.get(5)?,
                    state,
                    created_at_ms: as_u64(row.get(7)?),
                    claimed_at_ms: row.get::<_, Option<i64>>(8)?.map(as_u64),
                    consumed_at_ms: row.get::<_, Option<i64>>(9)?.map(as_u64),
                })
            })
            .map_err(store_error)?;
        for message in mailbox_rows {
            snapshot.mailbox.push(message.map_err(store_error)?);
        }
        drop(mailbox);

        snapshot.latest_sequence = self
            .conn
            .query_row(
                "SELECT COALESCE(MAX(sequence), 0) FROM runtime_events WHERE session_id = ?1",
                params![session_id],
                |row| row.get::<_, i64>(0),
            )
            .map(as_u64)
            .map_err(store_error)?;
        Ok(snapshot)
    }
}
