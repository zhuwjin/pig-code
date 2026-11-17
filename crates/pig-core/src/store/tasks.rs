use std::path::PathBuf;

use pig_protocol::{
    CoreError, DeliveryChannel, DeliveryState, TaskNotification, TaskStatus, TaskSummary,
};
use rusqlite::params;

use super::Store;

#[derive(Clone, Debug)]
pub struct BackgroundTask {
    pub summary: TaskSummary,
    pub detached: bool,
    pub record_path: Option<PathBuf>,
    pub result_path: Option<PathBuf>,
    pub turns: Option<u32>,
    pub model: Option<String>,
    pub run: Option<crate::task::AgentRunMetadata>,
    pub terminal_notification_suppressed: bool,
}

fn task_error(error: impl std::fmt::Display) -> CoreError {
    CoreError::TaskStore {
        detail: error.to_string(),
    }
}

fn json<T: serde::Serialize>(value: &T) -> Result<String, CoreError> {
    serde_json::to_string(value).map_err(task_error)
}

pub(super) fn schedule_notification_on(
    conn: &rusqlite::Connection,
    session_id: &str,
    notification: &TaskNotification,
) -> Result<(), CoreError> {
    conn.execute(
        "INSERT OR IGNORE INTO task_notifications (
            notification_id, session_id, task_id, status, payload,
            delivery_state, created_at_ms
        ) VALUES (?1, ?2, ?3, ?4, ?5, 'scheduled', ?6)",
        params![
            notification.id,
            session_id,
            notification.task_id,
            notification.status.as_str(),
            json(notification)?,
            crate::model_io::now_ms() as i64,
        ],
    )
    .map_err(task_error)?;
    Ok(())
}

pub(super) fn write_background_task(
    conn: &rusqlite::Connection,
    session_id: &str,
    task: &BackgroundTask,
) -> Result<(), CoreError> {
    let summary = &task.summary;
    conn.execute(
        "INSERT INTO background_tasks (
            session_id, task_id, kind, agent_id, command, state, exit_code, reason,
            detached, output_path, output_tail, record_path, result_path,
            started_at_ms, ended_at_ms, turns, model, run_id, parent_agent_id,
            delegation_depth, profile, run_input_tokens, run_cache_read_tokens,
            run_output_tokens, terminal_notification_suppressed
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25)
        ON CONFLICT(session_id, task_id) DO UPDATE SET
            kind = excluded.kind, agent_id = excluded.agent_id,
            command = excluded.command, state = excluded.state,
            exit_code = excluded.exit_code, reason = excluded.reason,
            detached = excluded.detached, output_path = excluded.output_path,
            output_tail = excluded.output_tail, record_path = excluded.record_path,
            result_path = excluded.result_path, started_at_ms = excluded.started_at_ms,
            ended_at_ms = excluded.ended_at_ms, turns = excluded.turns, model = excluded.model,
            run_id = excluded.run_id, parent_agent_id = excluded.parent_agent_id,
            delegation_depth = excluded.delegation_depth, profile = excluded.profile,
            run_input_tokens = excluded.run_input_tokens,
            run_cache_read_tokens = excluded.run_cache_read_tokens,
            run_output_tokens = excluded.run_output_tokens,
            terminal_notification_suppressed = excluded.terminal_notification_suppressed
        WHERE background_tasks.state NOT IN ('completed', 'failed', 'timed_out', 'killed', 'lost')
            OR excluded.state NOT IN ('queued', 'running')",
        params![
            session_id,
            summary.id,
            summary.kind.as_str(),
            summary.agent_id,
            summary.command,
            summary.status.as_str(),
            summary.exit_code,
            summary.reason,
            task.detached,
            summary
                .spill_path
                .as_ref()
                .map(|path| path.display().to_string()),
            summary.output_tail,
            task.record_path
                .as_ref()
                .map(|path| path.display().to_string()),
            task.result_path
                .as_ref()
                .map(|path| path.display().to_string()),
            summary.started_at_ms as i64,
            summary.ended_at_ms.map(|value| value as i64),
            task.turns,
            task.model,
            task.run.as_ref().map(|run| run.run_id.as_str()),
            task.run
                .as_ref()
                .and_then(|run| run.parent_agent_id.as_deref()),
            task.run.as_ref().map(|run| run.delegation_depth),
            task.run.as_ref().map(|run| run.profile.as_str()),
            task.run.as_ref().map(|run| run.usage.0 as i64),
            task.run.as_ref().map(|run| run.usage.1 as i64),
            task.run.as_ref().map(|run| run.usage.2 as i64),
            task.terminal_notification_suppressed,
        ],
    )
    .map_err(task_error)?;
    Ok(())
}

impl Store {
    pub fn upsert_background_task(
        &self,
        session_id: &str,
        task: &BackgroundTask,
    ) -> Result<(), CoreError> {
        write_background_task(&self.conn, session_id, task)
    }

    pub fn record_terminal_task(
        &mut self,
        session_id: &str,
        task: &BackgroundTask,
        notification: &TaskNotification,
    ) -> Result<(), CoreError> {
        let transaction = self.conn.transaction().map_err(task_error)?;
        write_background_task(&transaction, session_id, task)?;
        if task.detached && !task.terminal_notification_suppressed {
            schedule_notification_on(&transaction, session_id, notification)?;
        }
        transaction.commit().map_err(task_error)
    }

    pub fn background_tasks(&self, session_id: &str) -> Result<Vec<BackgroundTask>, CoreError> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT task_id, kind, agent_id, command, state, exit_code, reason, detached,
                        output_path, output_tail, record_path, result_path, started_at_ms,
                        ended_at_ms, turns, model, run_id, parent_agent_id, delegation_depth,
                        profile, run_input_tokens, run_cache_read_tokens, run_output_tokens,
                        terminal_notification_suppressed
                 FROM background_tasks WHERE session_id = ?1 ORDER BY started_at_ms, task_id",
            )
            .map_err(task_error)?;
        let rows = statement
            .query_map(params![session_id], |row| {
                let kind = match row.get::<_, String>(1)?.as_str() {
                    "shell" => pig_protocol::TaskKind::Shell,
                    "subagent" => pig_protocol::TaskKind::Subagent,
                    other => {
                        return Err(rusqlite::Error::FromSqlConversionFailure(
                            1,
                            rusqlite::types::Type::Text,
                            format!("unknown task kind: {other}").into(),
                        ));
                    }
                };
                let status = match row.get::<_, String>(4)?.as_str() {
                    "queued" => TaskStatus::Queued,
                    "running" => TaskStatus::Running,
                    "completed" => TaskStatus::Completed,
                    "failed" => TaskStatus::Failed,
                    "timed_out" => TaskStatus::TimedOut,
                    "killed" => TaskStatus::Killed,
                    "lost" => TaskStatus::Lost,
                    other => {
                        return Err(rusqlite::Error::FromSqlConversionFailure(
                            4,
                            rusqlite::types::Type::Text,
                            format!("unknown task state: {other}").into(),
                        ));
                    }
                };
                let agent_id: Option<String> = row.get(2)?;
                let run = match (agent_id.clone(), row.get::<_, Option<String>>(16)?) {
                    (Some(agent_id), Some(run_id)) => Some(crate::task::AgentRunMetadata {
                        agent_id,
                        run_id,
                        parent_agent_id: row.get::<_, Option<String>>(17)?,
                        delegation_depth: row
                            .get::<_, Option<i64>>(18)?
                            .unwrap_or(0)
                            .clamp(0, u32::MAX as i64)
                            as u32,
                        profile: row.get::<_, Option<String>>(19)?.unwrap_or_default(),
                        usage: (
                            row.get::<_, Option<i64>>(20)?.unwrap_or(0).max(0) as u64,
                            row.get::<_, Option<i64>>(21)?.unwrap_or(0).max(0) as u64,
                            row.get::<_, Option<i64>>(22)?.unwrap_or(0).max(0) as u64,
                        ),
                    }),
                    _ => None,
                };
                Ok(BackgroundTask {
                    summary: TaskSummary {
                        id: row.get(0)?,
                        kind,
                        command: row.get(3)?,
                        status,
                        exit_code: row.get(5)?,
                        reason: row.get(6)?,
                        started_at_ms: row.get::<_, i64>(12)? as u64,
                        ended_at_ms: row.get::<_, Option<i64>>(13)?.map(|value| value as u64),
                        output_tail: row.get(9)?,
                        agent_id,
                        spill_path: row.get::<_, Option<String>>(8)?.map(PathBuf::from),
                    },
                    detached: row.get(7)?,
                    record_path: row.get::<_, Option<String>>(10)?.map(PathBuf::from),
                    result_path: row.get::<_, Option<String>>(11)?.map(PathBuf::from),
                    turns: row.get(14)?,
                    model: row.get(15)?,
                    run,
                    terminal_notification_suppressed: row.get(23)?,
                })
            })
            .map_err(task_error)?;
        rows.map(|row| row.map_err(task_error)).collect()
    }

    pub fn schedule_notification(
        &self,
        session_id: &str,
        notification: &TaskNotification,
    ) -> Result<(), CoreError> {
        schedule_notification_on(&self.conn, session_id, notification)
    }

    pub fn task_notification_exists(&self, notification_id: &str) -> Result<bool, CoreError> {
        self.conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM task_notifications WHERE notification_id = ?1)",
                params![notification_id],
                |row| row.get::<_, i64>(0),
            )
            .map(|exists| exists != 0)
            .map_err(task_error)
    }

    pub fn pending_notifications(
        &self,
        session_id: &str,
    ) -> Result<Vec<TaskNotification>, CoreError> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT payload FROM task_notifications
                 WHERE session_id = ?1 AND delivery_state = 'scheduled'
                 ORDER BY created_at_ms, notification_id",
            )
            .map_err(task_error)?;
        let rows = statement
            .query_map(params![session_id], |row| row.get::<_, String>(0))
            .map_err(task_error)?;
        rows.map(|row| {
            let payload = row.map_err(task_error)?;
            serde_json::from_str(&payload).map_err(task_error)
        })
        .collect()
    }

    pub fn mark_delivered(
        &mut self,
        ids: &[String],
        channel: DeliveryChannel,
        timestamp_ms: u64,
    ) -> Result<(), CoreError> {
        if ids.is_empty() {
            return Ok(());
        }
        let transaction = self.conn.transaction().map_err(task_error)?;
        for id in ids {
            transaction
                .execute(
                    "UPDATE task_notifications
                     SET delivery_state = 'delivered', delivery_channel = ?2,
                         delivered_at_ms = ?3, last_error = NULL
                     WHERE notification_id = ?1 AND delivery_state = 'scheduled'",
                    params![id, channel.as_str(), timestamp_ms as i64],
                )
                .map_err(task_error)?;
        }
        transaction.commit().map_err(task_error)
    }

    pub fn reconcile_delivered(
        &mut self,
        receipts: &[(String, DeliveryChannel)],
    ) -> Result<(), CoreError> {
        let transaction = self.conn.transaction().map_err(task_error)?;
        for (id, channel) in receipts {
            transaction
                .execute(
                    "UPDATE task_notifications
                     SET delivery_state = 'delivered', delivery_channel = ?2,
                         delivered_at_ms = ?3, last_error = NULL
                     WHERE notification_id = ?1 AND delivery_state = 'scheduled'",
                    params![id, channel.as_str(), crate::model_io::now_ms() as i64],
                )
                .map_err(task_error)?;
        }
        transaction.commit().map_err(task_error)
    }

    pub fn reconcile_with_history(
        &mut self,
        session_id: &str,
        receipts: &[(String, DeliveryChannel)],
    ) -> Result<(), CoreError> {
        let transaction = self.conn.transaction().map_err(task_error)?;
        for (id, channel) in receipts {
            transaction
                .execute(
                    "UPDATE task_notifications
                     SET delivery_state = 'delivered', delivery_channel = ?2,
                         delivered_at_ms = ?3, last_error = NULL
                     WHERE notification_id = ?1 AND delivery_state = 'scheduled'",
                    params![id, channel.as_str(), crate::model_io::now_ms() as i64],
                )
                .map_err(task_error)?;
        }
        let delivered = {
            let mut statement = transaction
                .prepare(
                    "SELECT notification_id FROM task_notifications
                     WHERE session_id = ?1 AND delivery_state = 'delivered'",
                )
                .map_err(task_error)?;
            let rows = statement
                .query_map(params![session_id], |row| row.get::<_, String>(0))
                .map_err(task_error)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(task_error)?
        };
        for id in delivered {
            if !receipts.iter().any(|(receipt_id, _)| receipt_id == &id) {
                transaction
                    .execute(
                        "UPDATE task_notifications
                         SET delivery_state = 'scheduled', delivery_channel = NULL,
                             delivered_at_ms = NULL
                         WHERE notification_id = ?1",
                        params![id],
                    )
                    .map_err(task_error)?;
            }
        }
        transaction.commit().map_err(task_error)
    }

    pub fn reset_delivered_for_recall(&mut self, ids: &[String]) -> Result<(), CoreError> {
        let transaction = self.conn.transaction().map_err(task_error)?;
        for id in ids {
            transaction
                .execute(
                    "UPDATE task_notifications
                     SET delivery_state = 'scheduled', delivery_channel = NULL,
                         delivered_at_ms = NULL
                     WHERE notification_id = ?1 AND delivery_state = 'delivered'",
                    params![id],
                )
                .map_err(task_error)?;
        }
        transaction.commit().map_err(task_error)
    }

    pub fn notification_delivery(
        &self,
        notification_id: &str,
    ) -> Result<Option<(DeliveryState, Option<DeliveryChannel>)>, CoreError> {
        self.conn
            .query_row(
                "SELECT delivery_state, delivery_channel FROM task_notifications
                 WHERE notification_id = ?1",
                params![notification_id],
                |row| {
                    let state = match row.get::<_, String>(0)?.as_str() {
                        "scheduled" => DeliveryState::Scheduled,
                        "delivered" => DeliveryState::Delivered,
                        other => {
                            return Err(rusqlite::Error::FromSqlConversionFailure(
                                0,
                                rusqlite::types::Type::Text,
                                format!("unknown delivery state: {other}").into(),
                            ));
                        }
                    };
                    let channel = match row.get::<_, Option<String>>(1)?.as_deref() {
                        None => None,
                        Some("automatic") => Some(DeliveryChannel::Automatic),
                        Some("task_wait") => Some(DeliveryChannel::TaskWait),
                        Some(other) => {
                            return Err(rusqlite::Error::FromSqlConversionFailure(
                                1,
                                rusqlite::types::Type::Text,
                                format!("unknown delivery channel: {other}").into(),
                            ));
                        }
                    };
                    Ok((state, channel))
                },
            )
            .map(Some)
            .or_else(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(task_error(other)),
            })
    }

    pub fn recover_interrupted_tasks(
        &mut self,
        session_id: &str,
    ) -> Result<Vec<TaskNotification>, CoreError> {
        let tasks = self.background_tasks(session_id)?;
        let transaction = self.conn.transaction().map_err(task_error)?;
        let mut recovered = Vec::new();
        for mut task in tasks {
            if !task.detached
                || !matches!(
                    task.summary.status,
                    TaskStatus::Queued | TaskStatus::Running
                )
            {
                continue;
            }
            let ended_at_ms = crate::model_io::now_ms();
            let notification =
                crate::task::notification::lost_notification(session_id, &task, ended_at_ms);
            task.summary.status = TaskStatus::Lost;
            task.summary.exit_code = None;
            task.summary.ended_at_ms = Some(ended_at_ms);
            task.summary.reason =
                Some("The previous process ended before a terminal state was observed".to_string());
            write_background_task(&transaction, session_id, &task)?;
            if !task.terminal_notification_suppressed {
                schedule_notification_on(&transaction, session_id, &notification)?;
            }
            recovered.push(notification);
        }
        transaction.commit().map_err(task_error)?;
        Ok(recovered)
    }

    pub fn recover_terminal_notifications(
        &mut self,
        session_id: &str,
    ) -> Result<Vec<TaskNotification>, CoreError> {
        let mut recovered = Vec::new();
        for task in self.background_tasks(session_id)? {
            if !task.detached
                || task.terminal_notification_suppressed
                || !task.summary.status.is_terminal()
            {
                continue;
            }
            let notification =
                crate::task::notification::background_notification(session_id, &task);
            if self.notification_delivery(&notification.id)?.is_some() {
                continue;
            }
            self.schedule_notification(session_id, &notification)?;
            recovered.push(notification);
        }
        Ok(recovered)
    }

    pub fn record_notification_error(
        &mut self,
        ids: &[String],
        detail: &str,
    ) -> Result<(), CoreError> {
        if ids.is_empty() {
            return Ok(());
        }
        let detail = detail.chars().take(4_000).collect::<String>();
        let transaction = self.conn.transaction().map_err(task_error)?;
        for id in ids {
            transaction
                .execute(
                    "UPDATE task_notifications SET last_error = ?2
                     WHERE notification_id = ?1 AND delivery_state = 'scheduled'",
                    params![id, detail],
                )
                .map_err(task_error)?;
        }
        transaction.commit().map_err(task_error)
    }

    pub fn delete_session_tasks(&self, session_id: &str) -> Result<(), CoreError> {
        self.conn
            .execute(
                "DELETE FROM task_notifications WHERE session_id = ?1",
                params![session_id],
            )
            .map_err(task_error)?;
        self.conn
            .execute(
                "DELETE FROM background_tasks WHERE session_id = ?1",
                params![session_id],
            )
            .map_err(task_error)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pig_protocol::{NotificationSeverity, TaskKind};

    fn notification() -> TaskNotification {
        TaskNotification {
            id: "task:s1:b1:completed".into(),
            task_id: "b1".into(),
            kind: TaskKind::Shell,
            status: TaskStatus::Completed,
            exit_code: Some(0),
            severity: NotificationSeverity::Info,
            description: "done".into(),
            title: "Background shell completed".into(),
            body: "done".into(),
            duration_ms: Some(10),
            turns: None,
            model: None,
            agent_id: None,
            record_path: None,
            output: None,
        }
    }

    #[test]
    fn notification_delivery_is_idempotent_and_recallable() {
        let dir = std::env::temp_dir().join(format!("pig-task-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut store = Store::open(&dir).expect("open store");
        let notification = notification();
        store
            .schedule_notification("s1", &notification)
            .expect("schedule");
        store
            .schedule_notification("s1", &notification)
            .expect("idempotent");
        assert_eq!(store.pending_notifications("s1").expect("pending").len(), 1);
        store
            .mark_delivered(
                std::slice::from_ref(&notification.id),
                DeliveryChannel::Automatic,
                20,
            )
            .expect("deliver");
        assert!(
            store
                .pending_notifications("s1")
                .expect("pending")
                .is_empty()
        );
        store
            .schedule_notification("s1", &notification)
            .expect("no reschedule");
        assert!(
            store
                .pending_notifications("s1")
                .expect("pending")
                .is_empty()
        );
        store
            .reset_delivered_for_recall(std::slice::from_ref(&notification.id))
            .expect("reset");
        assert_eq!(store.pending_notifications("s1").expect("pending").len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn terminal_task_and_outbox_commit_together() {
        let dir = std::env::temp_dir().join(format!("pig-terminal-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut store = Store::open(&dir).expect("open store");
        let notification = notification();
        let task = BackgroundTask {
            summary: TaskSummary {
                id: "b1".into(),
                kind: TaskKind::Shell,
                command: "cargo test".into(),
                status: TaskStatus::Completed,
                exit_code: Some(0),
                reason: None,
                started_at_ms: 10,
                ended_at_ms: Some(20),
                output_tail: "tail".into(),
                agent_id: None,
                spill_path: Some("/tmp/task.log".into()),
            },
            detached: true,
            record_path: Some("/tmp/context.jsonl".into()),
            result_path: Some("/tmp/result.md".into()),
            turns: Some(3),
            model: Some("mock-model".into()),
            run: None,
            terminal_notification_suppressed: false,
        };
        store
            .record_terminal_task("s1", &task, &notification)
            .expect("atomic terminal");
        store
            .record_terminal_task("s1", &task, &notification)
            .expect("idempotent terminal");
        let restored = store.background_tasks("s1").expect("task rows");
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].summary.status, TaskStatus::Completed);
        assert_eq!(restored[0].summary.output_tail, "tail");
        assert_eq!(restored[0].turns, Some(3));
        assert_eq!(restored[0].model.as_deref(), Some("mock-model"));
        assert_eq!(
            restored[0].result_path.as_deref(),
            Some(std::path::Path::new("/tmp/result.md"))
        );
        assert_eq!(store.pending_notifications("s1").expect("pending").len(), 1);
        let mut stale = task.clone();
        stale.summary.status = TaskStatus::Running;
        stale.summary.ended_at_ms = None;
        store
            .upsert_background_task("s1", &stale)
            .expect("stale snapshot");
        assert_eq!(
            store.background_tasks("s1").expect("task rows")[0]
                .summary
                .status,
            TaskStatus::Completed,
            "an active snapshot cannot overwrite a saved terminal state"
        );

        store
            .mark_delivered(
                std::slice::from_ref(&notification.id),
                DeliveryChannel::Automatic,
                30,
            )
            .expect("deliver");
        store
            .mark_delivered(
                std::slice::from_ref(&notification.id),
                DeliveryChannel::TaskWait,
                40,
            )
            .expect("idempotent delivery");
        assert_eq!(
            store
                .notification_delivery(&notification.id)
                .expect("delivery"),
            Some((DeliveryState::Delivered, Some(DeliveryChannel::Automatic))),
            "a later channel cannot overwrite the first durable delivery"
        );
        store
            .reconcile_with_history("s1", &[])
            .expect("revoke missing receipt");
        assert_eq!(store.pending_notifications("s1").expect("pending").len(), 1);
        store
            .reconcile_with_history(
                "s1",
                &[(notification.id.clone(), DeliveryChannel::Automatic)],
            )
            .expect("restore receipt");
        assert!(
            store
                .pending_notifications("s1")
                .expect("pending")
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn agent_run_metadata_survives_reopen_and_marks_interrupted_work_lost() {
        let dir = std::env::temp_dir().join(format!("pig-agent-run-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).expect("open store");
        let run = crate::task::AgentRunMetadata {
            agent_id: "a1-1".into(),
            run_id: "run-1".into(),
            parent_agent_id: Some("a-parent".into()),
            delegation_depth: 2,
            profile: "explore".into(),
            usage: (11, 12, 13),
        };
        let task = BackgroundTask {
            summary: TaskSummary {
                id: "b1".into(),
                kind: TaskKind::Subagent,
                command: "Subagent explore: catalog".into(),
                status: TaskStatus::Running,
                exit_code: None,
                reason: None,
                started_at_ms: 10,
                ended_at_ms: None,
                output_tail: String::new(),
                agent_id: Some(run.agent_id.clone()),
                spill_path: None,
            },
            detached: true,
            record_path: Some("agent.jsonl".into()),
            result_path: Some("agent.result.md".into()),
            turns: Some(3),
            model: Some("mock-model".into()),
            run: Some(run.clone()),
            terminal_notification_suppressed: false,
        };
        store
            .upsert_background_task("s1", &task)
            .expect("persist run");
        drop(store);

        let mut store = Store::open(&dir).expect("reopen store");
        store
            .recover_interrupted_tasks("s1")
            .expect("recover interrupted run");
        let restored = store.background_tasks("s1").expect("task rows");
        assert_eq!(restored[0].summary.status, TaskStatus::Lost);
        assert_eq!(restored[0].run.as_ref(), Some(&run));
        assert_eq!(restored[0].turns, Some(3));
        assert_eq!(restored[0].model.as_deref(), Some("mock-model"));
        assert_eq!(
            restored[0].record_path.as_deref(),
            Some(std::path::Path::new("agent.jsonl"))
        );
        assert_eq!(
            restored[0].result_path.as_deref(),
            Some(std::path::Path::new("agent.result.md"))
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
