use futures_util::stream::FuturesUnordered;
use pig_protocol::{CoreError, Event};

use super::entry::{SessionEntry, TurnFuture, resolve_entry};
use super::queue::{next_queue_id, queue_len, start_queued_head};
use super::{Manager, emit};

pub(super) fn sync_notification_mailbox(
    entry: &SessionEntry,
    store: &std::sync::Arc<std::sync::Mutex<crate::store::Store>>,
    session_id: &str,
) -> Result<Vec<pig_protocol::TaskNotification>, CoreError> {
    let mut notifications = store
        .lock()
        .expect("store lock")
        .pending_notifications(session_id)?;
    let receipts = entry
        .state
        .delivery_receipts
        .lock()
        .expect("notification receipt lock");
    notifications.retain(|notification| !receipts.contains_key(&notification.id));
    drop(receipts);
    *entry.notifications.lock().expect("notification lock") = notifications
        .iter()
        .map(|notification| notification.id.clone())
        .collect();
    Ok(notifications)
}

impl Manager {
    pub(super) fn retry_terminal_notifications(&mut self, event_tx: &async_channel::Sender<Event>) {
        for (session_id, task, notification) in std::mem::take(&mut self.pending_terminal) {
            if self.deleted_sessions.contains(&session_id) {
                continue;
            }
            let result = self.store.lock().expect("store lock").record_terminal_task(
                &session_id,
                &task,
                &notification,
            );
            if let Err(error) = result {
                tracing::warn!(
                    session_id = %session_id,
                    error = ?error,
                    "terminal task persistence failed; retrying in the background"
                );
                self.pending_terminal.push((session_id, task, notification));
            } else {
                self.emit_task_list(event_tx, &session_id);
            }
        }
    }

    pub(super) fn task_changed(
        &mut self,
        event_tx: async_channel::Sender<Event>,
        change: crate::task::TaskChange,
        turns: &mut FuturesUnordered<TurnFuture>,
    ) {
        match change {
            crate::task::TaskChange::Snapshot { session_id, task } => {
                if self.deleted_sessions.contains(&session_id) {
                    return;
                }
                if let Some(task) = task {
                    let result = self
                        .store
                        .lock()
                        .expect("store lock")
                        .upsert_background_task(&session_id, &task);
                    if let Err(error) = result {
                        emit(
                            &event_tx,
                            Event::Error {
                                session_id: Some(session_id.clone()),
                                error,
                                fatal: false,
                            },
                        );
                    }
                }
                self.emit_task_list(&event_tx, &session_id);
            }
            crate::task::TaskChange::Terminal {
                session_id,
                task,
                notification,
            } => {
                if self.deleted_sessions.contains(&session_id) {
                    return;
                }
                let detached = task.detached;
                let suppressed = task.terminal_notification_suppressed;
                let result = self.store.lock().expect("store lock").record_terminal_task(
                    &session_id,
                    &task,
                    &notification,
                );
                if let Err(error) = result {
                    emit(
                        &event_tx,
                        Event::Error {
                            session_id: Some(session_id.clone()),
                            error,
                            fatal: false,
                        },
                    );
                    self.pending_terminal
                        .push((session_id, *task, notification));
                    return;
                }
                self.emit_task_list(&event_tx, &session_id);
                if !detached || suppressed {
                    return;
                }
                self.refresh_notification_queue(&event_tx, &session_id);
                self.start_notification_turn_if_ready(&event_tx, &session_id, turns);
            }
        }
    }

    fn emit_task_list(&self, event_tx: &async_channel::Sender<Event>, session_id: &str) {
        let Some(entry) = self.sessions.get(session_id) else {
            return;
        };
        let tasks = crate::task::snapshot(&entry.state.tasks);
        emit(
            event_tx,
            Event::TaskListChanged {
                session_id: session_id.to_string(),
                tasks,
            },
        );
    }

    pub(super) fn refresh_notification_queue(
        &mut self,
        event_tx: &async_channel::Sender<Event>,
        session_id: &str,
    ) {
        self.retry_terminal_notifications(event_tx);
        let Some(entry) = self.sessions.get(session_id) else {
            return;
        };
        let notifications = match sync_notification_mailbox(entry, &self.store, session_id) {
            Ok(notifications) => notifications,
            Err(error) => {
                emit(
                    event_tx,
                    Event::Error {
                        session_id: Some(session_id.to_string()),
                        error,
                        fatal: false,
                    },
                );
                return;
            }
        };
        emit(
            event_tx,
            Event::TaskNotificationQueueChanged {
                session_id: session_id.to_string(),
                notifications,
            },
        );
    }

    pub(super) fn start_notification_turn_if_ready(
        &mut self,
        event_tx: &async_channel::Sender<Event>,
        session_id: &str,
        turns: &mut FuturesUnordered<TurnFuture>,
    ) -> bool {
        self.retry_terminal_notifications(event_tx);
        let Some(entry) = self.sessions.get_mut(session_id) else {
            return false;
        };
        start_notification_turn(entry, session_id, event_tx, turns, &self.store)
    }
}

pub(super) fn start_notification_turn(
    entry: &mut SessionEntry,
    session_id: &str,
    event_tx: &async_channel::Sender<Event>,
    turns: &mut FuturesUnordered<TurnFuture>,
    store: &std::sync::Arc<std::sync::Mutex<crate::store::Store>>,
) -> bool {
    if entry.session.is_none() || !entry.auto_drain {
        return false;
    }
    if queue_len(entry) > 0 {
        return start_queued_head(entry, session_id, event_tx, turns, store);
    }
    let notifications = match sync_notification_mailbox(entry, store, session_id) {
        Ok(notifications) => notifications,
        Err(error) => {
            emit(
                event_tx,
                Event::Error {
                    session_id: Some(session_id.to_string()),
                    error,
                    fatal: false,
                },
            );
            return false;
        }
    };
    emit(
        event_tx,
        Event::TaskNotificationQueueChanged {
            session_id: session_id.to_string(),
            notifications: notifications.clone(),
        },
    );
    if notifications.is_empty() {
        return false;
    }
    let resolved = match resolve_entry(entry) {
        Ok(resolved) => resolved,
        Err(error) => {
            emit(
                event_tx,
                Event::Error {
                    session_id: Some(session_id.to_string()),
                    error,
                    fatal: false,
                },
            );
            return false;
        }
    };
    let message = crate::session::PendingMessage {
        id: next_queue_id(&mut entry.queue_seq),
        text: String::new(),
        files: vec![],
        images: vec![],
        permissions: entry.last_permissions,
        steer: false,
        held: false,
    };
    if super::queue::start_turn_inner(
        entry,
        session_id.to_string(),
        message,
        &resolved,
        true,
        event_tx,
        turns,
    )
    .is_err()
    {
        emit(
            event_tx,
            Event::Error {
                session_id: Some(session_id.to_string()),
                error: CoreError::Internal {
                    detail: "The session was busy; the notification stays queued.".to_string(),
                },
                fatal: false,
            },
        );
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use pig_protocol::{NotificationSeverity, TaskKind, TaskStatus};

    fn terminal_task() -> (crate::store::BackgroundTask, pig_protocol::TaskNotification) {
        let summary = pig_protocol::TaskSummary {
            id: "b1".into(),
            kind: TaskKind::Shell,
            command: "done".into(),
            status: TaskStatus::Completed,
            exit_code: Some(0),
            reason: None,
            started_at_ms: 1,
            ended_at_ms: Some(2),
            output_tail: "done".into(),
            agent_id: None,
            spill_path: None,
        };
        let task = crate::store::BackgroundTask {
            summary: summary.clone(),
            detached: true,
            record_path: None,
            result_path: None,
            turns: None,
            model: None,
            terminal_notification_suppressed: false,
        };
        let notification = pig_protocol::TaskNotification {
            id: "task:s1:b1:completed".into(),
            task_id: "b1".into(),
            kind: TaskKind::Shell,
            status: TaskStatus::Completed,
            exit_code: Some(0),
            severity: NotificationSeverity::Info,
            description: "done".into(),
            title: "Background shell completed".into(),
            body: "done".into(),
            duration_ms: Some(1),
            turns: None,
            model: None,
            agent_id: None,
            record_path: None,
            output: None,
        };
        (task, notification)
    }

    #[test]
    fn failed_terminal_writes_roll_back_and_retry_owned_facts() {
        let dir = std::env::temp_dir().join(format!("pig-terminal-retry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create test data dir");
        let config_path = dir.join("config.toml");
        std::fs::write(&config_path, "").expect("empty test config");
        let (event_tx, _event_rx) = async_channel::unbounded();
        let (_op_tx, op_rx) = async_channel::unbounded();
        let mut manager = Manager::new(
            &event_tx,
            op_rx,
            Some(config_path),
            dir.clone(),
            dir.clone(),
            std::sync::Arc::new(crate::credentials::CredentialManager::mock()),
        );
        rusqlite::Connection::open(dir.join("store.sqlite"))
            .expect("open store")
            .execute("DROP TABLE task_notifications", [])
            .expect("break the outbox");
        let (task, notification) = terminal_task();
        manager.task_changed(
            event_tx.clone(),
            crate::task::TaskChange::Terminal {
                session_id: "s1".into(),
                task: Box::new(task),
                notification,
            },
            &mut FuturesUnordered::new(),
        );
        assert_eq!(manager.pending_terminal.len(), 1);
        assert!(
            manager
                .store
                .lock()
                .expect("store lock")
                .background_tasks("s1")
                .expect("task rows")
                .is_empty(),
            "the terminal fact and its outbox entry roll back together"
        );

        crate::store::Store::open(&dir).expect("recreate the outbox schema");
        manager.retry_terminal_notifications(&event_tx);
        assert!(manager.pending_terminal.is_empty());
        assert_eq!(
            manager
                .store
                .lock()
                .expect("store lock")
                .background_tasks("s1")
                .expect("task rows")
                .len(),
            1
        );
        assert_eq!(
            manager
                .store
                .lock()
                .expect("store lock")
                .pending_notifications("s1")
                .expect("pending notifications")
                .len(),
            1
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
