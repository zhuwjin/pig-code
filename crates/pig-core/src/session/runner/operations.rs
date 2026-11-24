use super::*;

use super::entry::{SessionEntry, model_label, resolve_entry};
use super::queue::{queue_snapshot, steer_snapshot, stop_session_tasks_until_idle};
use super::runtime::layered_runtime;

use crate::session::{estimate, meta_to_selection, restore_pending, restored_queue_seq};

impl Manager {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new_session(
        &mut self,
        event_tx: async_channel::Sender<Event>,
        cwd: PathBuf,
        provider_id: Option<String>,
        model_id: Option<String>,
        reasoning_level: Option<String>,
        permissions: Option<pig_protocol::PermissionSettings>,
        plan_enabled: Option<bool>,
    ) {
        let config_path = self.config_path.clone();
        let sessions_dir = self.sessions_dir.clone();
        let data_dir = self.data_dir.clone();
        let store = self.store.clone();
        let pending = self.pending.clone();
        let pending_questions = self.pending_questions.clone();
        let task_notify_tx = self.task_notify_tx.clone();
        let config = &mut self.config;
        let sessions = &mut self.sessions;
        let id_counter = &mut self.id_counter;
        let cwd = normalize_workspace_path(&cwd);
        *id_counter += 1;
        let id = format!("s{}-{}", now_secs(), id_counter);
        // New-session defaults = the workspace's most recently active session; fields passed explicitly by the UI take priority over the seed
        let seed = store
            .lock()
            .expect("store lock")
            .latest_active_in_workspace(&cwd);
        let mut meta = SessionMeta {
            id: id.clone(),
            // New-session seed title: empty-string sentinel (GUI renders the localized "New task");
            // persisted data holds no natural-language constants
            title: String::new(),
            title_custom: false,
            cwd,
            created_at: now_secs(),
            updated_at: now_secs(),
            pinned: false,
            archived: false,
            provider_id: provider_id.or_else(|| seed.as_ref().and_then(|m| m.provider_id.clone())),
            model_id: model_id.or_else(|| seed.as_ref().and_then(|m| m.model_id.clone())),
            // Reasoning level passes through from the UI as-is (hero defaults already seed the UI; None = off)
            reasoning_level,
            permissions: permissions
                .unwrap_or_else(|| seed.as_ref().map(|m| m.permissions).unwrap_or_default()),
            // Plan mode is transient: not inherited from the seed, only set explicitly by the UI
            plan_enabled: plan_enabled.unwrap_or(false),
        };
        // UI left the reasoning level unset and the model config has a default -> adopt the default tier
        // (written into meta; SessionConfigured syncs it back to the UI's level chip)
        let session_runtime = layered_runtime(
            config.as_ref(),
            &config_path,
            &meta.cwd,
            self.credentials.clone(),
            &self.plugins,
        );
        if meta.reasoning_level.is_none()
            && let Some(cfg) = session_runtime.as_ref().map(|runtime| runtime.config())
        {
            let pid = meta
                .provider_id
                .clone()
                .unwrap_or_else(|| cfg.default_provider.clone());
            let mid = meta
                .model_id
                .clone()
                .unwrap_or_else(|| cfg.default_model.clone());
            let model_cfg = cfg
                .providers
                .iter()
                .find(|p| p.id == pid)
                .and_then(|p| p.models.iter().find(|m| m.id == mid));
            if let Some(level) = model_cfg
                .and_then(|m| m.default_reasoning_level.clone())
                .filter(|lv| model_cfg.is_some_and(|m| m.reasoning_levels.contains(lv)))
            {
                meta.reasoning_level = Some(level);
            }
        }
        match Session::create(
            meta.clone(),
            pending.clone(),
            pending_questions.clone(),
            store.clone(),
            &sessions_dir,
            data_dir.clone(),
            task_notify_tx.clone(),
            &self.plugins,
            session_runtime.clone(),
        ) {
            Ok(mut session) => {
                session.state.agents.attach_event_journal(event_tx.clone());
                session.set_permissions(meta.permissions);
                session.set_plan_mode(meta.plan_enabled);
                // Untrusted project surfaces: banner once per session
                // (hooks, MCP, skills, profiles, permission allow rules and
                // config stay inactive until Op::TrustProjectHooks; the
                // SessionStart chain itself runs at the first turn)
                if let Some((path, count)) = session.pending_project_trust() {
                    emit(
                        &event_tx,
                        Event::ProjectHooksPending {
                            session_id: id.clone(),
                            path,
                            count,
                        },
                    );
                }
                let selection = meta_to_selection(&meta);
                let state = session.state.clone();
                let entry_runtime = session.runtime_config_cloned();
                let plugin_dispatcher = session.plugin_tools.dispatcher();
                sessions.insert(
                    id.clone(),
                    SessionEntry {
                        session: Some(session),
                        plugin_dispatcher,
                        state,
                        cwd: meta.cwd.clone(),
                        runtime: entry_runtime,
                        cancel: None,
                        active_claim: None,
                        active_run: None,
                        retry: None,
                        model_override: selection.clone(),
                        reasoning_level: meta.reasoning_level.clone(),
                        last_permissions: meta.permissions,
                        pending: Arc::new(Mutex::new(Default::default())),
                        steer_signal: tokio::sync::watch::channel(0).0,
                        notifications: Arc::new(Mutex::new(Default::default())),
                        queue_seq: 0,
                        auto_drain: true,
                        cancel_promote: false,
                        pending_plan: None,
                        steerable: false,
                        mcp_status: None,
                        plugin_surfaces_stale: false,
                    },
                );
                store.lock().expect("store lock").upsert_session(&meta);
                let (model, provider_name) = sessions.get(&id).map(model_label).unwrap_or_default();
                emit(
                    &event_tx,
                    Event::SessionConfigured {
                        session_id: id.clone(),
                        cwd: meta.cwd.clone(),
                        model,
                        provider_name,
                        provider_id: meta.provider_id.clone(),
                        model_id: meta.model_id.clone(),
                        reasoning_level: meta.reasoning_level.clone(),
                        permissions: meta.permissions,
                        plan_enabled: meta.plan_enabled,
                    },
                );
                // New session: panels initialize to empty snapshots
                emit(
                    &event_tx,
                    Event::TodoListChanged {
                        session_id: id.clone(),
                        items: vec![],
                    },
                );
                emit(
                    &event_tx,
                    Event::TaskListChanged {
                        session_id: id.clone(),
                        tasks: vec![],
                    },
                );
                emit(
                    &event_tx,
                    Event::SessionList {
                        sessions: store.lock().expect("store lock").sorted_sessions(),
                    },
                );
                // Creating a session under a removed (hidden) workspace: automatically unhide it
                if store
                    .lock()
                    .expect("store lock")
                    .unhide_workspace(&meta.cwd)
                {
                    emit(
                        &event_tx,
                        Event::WorkspaceList {
                            workspaces: store.lock().expect("store lock").workspaces(),
                        },
                    );
                }
            }
            Err(error) => emit(
                &event_tx,
                Event::Error {
                    session_id: None,
                    error,
                    fatal: false,
                },
            ),
        }
    }
    pub(super) fn replay_loaded_session(
        &mut self,
        event_tx: &async_channel::Sender<Event>,
        session_id: String,
    ) {
        self.pending_replays.retain(|id| id != &session_id);
        if self.deleted_sessions.contains(&session_id) {
            return;
        }
        let Some(entry) = self.sessions.get_mut(&session_id) else {
            return;
        };
        let Some(session) = entry.session.as_mut() else {
            if !self.pending_replays.contains(&session_id) {
                self.pending_replays.push(session_id);
            }
            return;
        };
        let path = self.sessions_dir.join(format!("{session_id}.jsonl"));
        match crate::rollout::Rollout::load(&path) {
            Ok(records) => session.replay(&records, event_tx),
            Err(error) => {
                emit(
                    event_tx,
                    Event::Error {
                        session_id: Some(session_id),
                        error,
                        fatal: false,
                    },
                );
                return;
            }
        }
        emit(event_tx, Event::SessionReplayComplete { session_id });
    }

    pub(super) fn open_session(
        &mut self,
        event_tx: async_channel::Sender<Event>,
        session_id: String,
    ) {
        if self.deleted_sessions.contains(&session_id) {
            emit(
                &event_tx,
                Event::Error {
                    session_id: Some(session_id),
                    error: CoreError::SessionNotFoundOpen,
                    fatal: false,
                },
            );
            return;
        }
        self.retry_terminal_notifications(&event_tx);
        let config_path = self.config_path.clone();
        let sessions_dir = self.sessions_dir.clone();
        let data_dir = self.data_dir.clone();
        let default_cwd = self.default_cwd.clone();
        let store = self.store.clone();
        let pending = self.pending.clone();
        let pending_questions = self.pending_questions.clone();
        let task_notify_tx = self.task_notify_tx.clone();
        let config = &self.config;
        let sessions = &mut self.sessions;
        if sessions.contains_key(&session_id) {
            let meta = store.lock().expect("store lock").get_session(&session_id);
            if let Some(meta) = meta {
                let (model, provider_name) = sessions
                    .get(&session_id)
                    .map(model_label)
                    .unwrap_or_default();
                emit(
                    &event_tx,
                    Event::SessionConfigured {
                        session_id: session_id.clone(),
                        cwd: meta.cwd,
                        model,
                        provider_name,
                        provider_id: meta.provider_id.clone(),
                        model_id: meta.model_id.clone(),
                        reasoning_level: meta.reasoning_level.clone(),
                        permissions: meta.permissions,
                        plan_enabled: meta.plan_enabled,
                    },
                );
                // Switching back to an already-open session: re-emit panel snapshots so the UI resets its panels
                if let Some(entry) = sessions.get(&session_id) {
                    let items = entry.state.todos.lock().expect("todos lock").clone();
                    let tasks = entry.state.task_snapshot();
                    emit(
                        &event_tx,
                        Event::TodoListChanged {
                            session_id: session_id.clone(),
                            items,
                        },
                    );
                    emit(
                        &event_tx,
                        Event::TaskListChanged {
                            session_id: session_id.clone(),
                            tasks,
                        },
                    );
                    match super::notifications::sync_notification_mailbox(
                        entry,
                        &store,
                        &session_id,
                    ) {
                        Ok(notifications) => emit(
                            &event_tx,
                            Event::TaskNotificationQueueChanged {
                                session_id: session_id.clone(),
                                notifications,
                            },
                        ),
                        Err(error) => emit(
                            &event_tx,
                            Event::Error {
                                session_id: Some(session_id.clone()),
                                error,
                                fatal: false,
                            },
                        ),
                    }
                    // Re-emit the usage watermark/totals (the composer's capacity chip would otherwise keep stale values after switching sessions)
                    if let Some(session) = &entry.session
                        && let Some(used) = session.run_state.last_total_tokens
                        && let Ok(resolved) = resolve_entry(entry)
                    {
                        emit(
                            &event_tx,
                            Event::ContextUsage {
                                session_id: session_id.clone(),
                                used,
                                total: resolved.context_window,
                                cache_read_total: session.run_state.cache_read_total,
                                input_total: session.run_state.input_total,
                            },
                        );
                        // The composition panel's rows belong to the
                        // same snapshot: recompute them from the loaded
                        // history instead of waiting for the next turn
                        let breakdown = estimate::context_breakdown(
                            &session.agent.history,
                            &session.root_schemas(),
                        );
                        emit(
                            &event_tx,
                            Event::ContextBreakdownChanged {
                                session_id: session_id.clone(),
                                breakdown,
                            },
                        );
                    }
                }
            }
            self.replay_loaded_session(&event_tx, session_id);
            return;
        }
        let resume_cwd = store
            .lock()
            .expect("store lock")
            .get_session(&session_id)
            .map(|meta| meta.cwd)
            .unwrap_or_else(|| default_cwd.clone());
        let session_runtime = layered_runtime(
            config.as_ref(),
            &config_path,
            &resume_cwd,
            self.credentials.clone(),
            &self.plugins,
        );
        match Session::load(
            &session_id,
            &sessions_dir,
            pending.clone(),
            pending_questions.clone(),
            store.clone(),
            data_dir.clone(),
            task_notify_tx.clone(),
            &self.plugins,
            session_runtime.clone(),
        ) {
            Ok((mut session, records)) => {
                session.state.agents.attach_event_journal(event_tx.clone());
                // Restore persisted mode/model overrides (meta stays current via Set* write-through)
                let meta = store.lock().expect("store lock").get_session(&session_id);
                let selection = meta.as_ref().and_then(meta_to_selection);
                if let Some(meta) = &meta {
                    session.set_permissions(meta.permissions);
                    session.set_plan_mode(meta.plan_enabled);
                }
                let cwd = session.agent.cwd.clone();
                let state = session.state.clone();
                let entry_runtime = session.runtime_config_cloned();
                let plugin_dispatcher = session.plugin_tools.dispatcher();
                let pending: super::PendingMailbox = Arc::new(Mutex::new(Default::default()));
                sessions.insert(
                    session_id.clone(),
                    SessionEntry {
                        session: Some(session),
                        plugin_dispatcher,
                        state,
                        cwd: cwd.clone(),
                        runtime: entry_runtime,
                        cancel: None,
                        active_claim: None,
                        active_run: None,
                        retry: None,
                        model_override: selection.clone(),
                        reasoning_level: meta.as_ref().and_then(|m| m.reasoning_level.clone()),
                        last_permissions: meta.as_ref().map(|m| m.permissions).unwrap_or_default(),
                        pending: pending.clone(),
                        steer_signal: tokio::sync::watch::channel(0).0,
                        notifications: Arc::new(Mutex::new(Default::default())),
                        queue_seq: 0,
                        auto_drain: true,
                        cancel_promote: false,
                        pending_plan: None,
                        steerable: false,
                        mcp_status: None,
                        plugin_surfaces_stale: false,
                    },
                );
                // Switching back to an already-open session: re-emit
                // both pending lanes so the UI rebuilds its queue view
                let queue_items = queue_snapshot(&pending);
                emit(
                    &event_tx,
                    Event::QueueChanged {
                        session_id: session_id.clone(),
                        items: queue_items,
                    },
                );
                let steer_items = steer_snapshot(&pending);
                emit(
                    &event_tx,
                    Event::SteerChanged {
                        session_id: session_id.clone(),
                        items: steer_items,
                    },
                );
                let (model, provider_name) = sessions
                    .get(&session_id)
                    .map(model_label)
                    .unwrap_or_default();
                emit(
                    &event_tx,
                    Event::SessionConfigured {
                        session_id: session_id.clone(),
                        cwd,
                        model,
                        provider_name,
                        provider_id: meta.as_ref().and_then(|m| m.provider_id.clone()),
                        model_id: meta.as_ref().and_then(|m| m.model_id.clone()),
                        reasoning_level: meta.as_ref().and_then(|m| m.reasoning_level.clone()),
                        permissions: meta.as_ref().map(|m| m.permissions).unwrap_or_default(),
                        plan_enabled: meta.as_ref().is_some_and(|m| m.plan_enabled),
                    },
                );
                emit(
                    &event_tx,
                    Event::TodoListChanged {
                        session_id: session_id.clone(),
                        items: vec![],
                    },
                );
                emit(
                    &event_tx,
                    Event::TaskListChanged {
                        session_id: session_id.clone(),
                        tasks: sessions
                            .get(&session_id)
                            .map(|entry| entry.state.task_snapshot())
                            .unwrap_or_default(),
                    },
                );
                if let Some(entry) = sessions.get_mut(&session_id) {
                    if let Some(session) = entry.session.as_mut() {
                        session.replay(&records, &event_tx);
                        // Untrusted project surfaces: banner once per
                        // session (the SessionStart chain runs at
                        // the first turn with source "resume")
                        if let Some((path, count)) = session.pending_project_trust() {
                            emit(
                                &event_tx,
                                Event::ProjectHooksPending {
                                    session_id: session_id.clone(),
                                    path,
                                    count,
                                },
                            );
                        }
                    }
                    // Persisted pending queue restores into the
                    // entry (steer flags flatten — no turn runs
                    // after a restart), PAUSED: a restart never
                    // auto-continues the head (the interrupted
                    // turn's context is gone; the user resumes
                    // via the banner). Held items flatten into the
                    // queue lane too, so nothing is lost.
                    let restored = restore_pending(&store, &session_id);
                    entry.queue_seq = entry.queue_seq.max(restored_queue_seq(&restored));
                    entry.pending.lock().expect("pending lock").extend(restored);
                    match crate::session::pending::recovered_pending_execution(&store, &session_id)
                    {
                        Ok(Some(message)) => {
                            entry.queue_seq = entry
                                .queue_seq
                                .max(restored_queue_seq(std::slice::from_ref(&message)));
                            let mut pending = entry.pending.lock().expect("pending lock");
                            if !pending.iter().any(|item| item.id == message.id) {
                                pending.push_front(message);
                            }
                        }
                        Ok(None) => {}
                        Err(error) => emit(
                            &event_tx,
                            Event::Error {
                                session_id: Some(session_id.clone()),
                                error,
                                fatal: false,
                            },
                        ),
                    }
                    let has_user_queue = !entry.pending.lock().expect("pending lock").is_empty();
                    if has_user_queue {
                        let items = queue_snapshot(&entry.pending);
                        emit(
                            &event_tx,
                            Event::QueueChanged {
                                session_id: session_id.clone(),
                                items,
                            },
                        );
                    }
                    let notifications = match super::notifications::sync_notification_mailbox(
                        entry,
                        &store,
                        &session_id,
                    ) {
                        Ok(notifications) => notifications,
                        Err(error) => {
                            emit(
                                &event_tx,
                                Event::Error {
                                    session_id: Some(session_id.clone()),
                                    error,
                                    fatal: false,
                                },
                            );
                            Vec::new()
                        }
                    };
                    let has_notifications = !notifications.is_empty();
                    emit(
                        &event_tx,
                        Event::TaskNotificationQueueChanged {
                            session_id: session_id.clone(),
                            notifications,
                        },
                    );
                    if has_user_queue || has_notifications {
                        entry.auto_drain = false;
                        emit(
                            &event_tx,
                            Event::QueueDrainChanged {
                                session_id: session_id.clone(),
                                auto_drain: false,
                                reason: Some(pig_protocol::QueuePauseReason::Restarted),
                            },
                        );
                    }
                }
                // Replay already restored the usage watermark and totals: re-emit context capacity so
                // reopening the app shows it without waiting for the next message (skipped when no model is configured; no window to report)
                if let Some(entry) = sessions.get(&session_id)
                    && let Some(session) = &entry.session
                    && let Some(used) = session.run_state.last_total_tokens
                    && let Ok(resolved) = resolve_entry(entry)
                {
                    emit(
                        &event_tx,
                        Event::ContextUsage {
                            session_id: session_id.clone(),
                            used,
                            total: resolved.context_window,
                            cache_read_total: session.run_state.cache_read_total,
                            input_total: session.run_state.input_total,
                        },
                    );
                    let breakdown = estimate::context_breakdown(
                        &session.agent.history,
                        &session.root_schemas(),
                    );
                    emit(
                        &event_tx,
                        Event::ContextBreakdownChanged {
                            session_id: session_id.clone(),
                            breakdown,
                        },
                    );
                }
                emit(
                    &event_tx,
                    Event::SessionReplayComplete {
                        session_id: session_id.clone(),
                    },
                );
            }
            Err(error) => emit(
                &event_tx,
                Event::Error {
                    session_id: None,
                    error,
                    fatal: false,
                },
            ),
        }
    }
    pub(super) fn list_workspaces(&mut self, event_tx: async_channel::Sender<Event>) {
        let store = self.store.clone();
        emit(
            &event_tx,
            Event::WorkspaceList {
                workspaces: store.lock().expect("store lock").workspaces(),
            },
        );
    }
    pub(super) fn add_workspace(&mut self, event_tx: async_channel::Sender<Event>, path: PathBuf) {
        let store = self.store.clone();
        let path = normalize_workspace_path(&path);
        store.lock().expect("store lock").add_workspace(&path);
        emit(
            &event_tx,
            Event::WorkspaceList {
                workspaces: store.lock().expect("store lock").workspaces(),
            },
        );
    }
    pub(super) fn remove_workspace(
        &mut self,
        event_tx: async_channel::Sender<Event>,
        path: PathBuf,
    ) {
        let store = self.store.clone();
        let path = normalize_workspace_path(&path);
        store.lock().expect("store lock").hide_workspace(&path);
        emit(
            &event_tx,
            Event::WorkspaceList {
                workspaces: store.lock().expect("store lock").workspaces(),
            },
        );
    }
    pub(super) fn rename_workspace(
        &mut self,
        event_tx: async_channel::Sender<Event>,
        path: PathBuf,
        alias: Option<String>,
    ) {
        let store = self.store.clone();
        let path = normalize_workspace_path(&path);
        store
            .lock()
            .expect("store lock")
            .rename_workspace(&path, alias);
        emit(
            &event_tx,
            Event::WorkspaceList {
                workspaces: store.lock().expect("store lock").workspaces(),
            },
        );
    }
    pub(super) fn list_sessions(&mut self, event_tx: async_channel::Sender<Event>) {
        let store = self.store.clone();
        emit(
            &event_tx,
            Event::SessionList {
                sessions: store.lock().expect("store lock").sorted_sessions(),
            },
        );
    }
    pub(super) fn update_session_meta(
        &mut self,
        event_tx: async_channel::Sender<Event>,
        session_id: String,
        pinned: Option<bool>,
        archived: Option<bool>,
        title: Option<String>,
    ) {
        let store = self.store.clone();
        store
            .lock()
            .expect("store lock")
            .update_session(&session_id, |meta| {
                if let Some(pinned) = pinned {
                    meta.pinned = pinned;
                }
                if let Some(archived) = archived {
                    meta.archived = archived;
                }
                if let Some(title) = &title {
                    meta.title = title.clone();
                    // Manual rename: auto-naming never overrides it afterwards
                    meta.title_custom = true;
                }
                meta.updated_at = now_secs();
            });
        emit(
            &event_tx,
            Event::SessionList {
                sessions: store.lock().expect("store lock").sorted_sessions(),
            },
        );
    }
    pub(super) fn delete_session(
        &mut self,
        event_tx: async_channel::Sender<Event>,
        session_id: String,
    ) {
        if let Err(error) = self
            .store
            .lock()
            .expect("store lock")
            .request_session_deletion(&session_id)
        {
            emit(
                &event_tx,
                Event::Error {
                    session_id: Some(session_id),
                    error,
                    fatal: false,
                },
            );
            return;
        }
        let pending = self.pending.clone();
        let pending_questions = self.pending_questions.clone();
        let turn_in_flight = self
            .sessions
            .get(&session_id)
            .is_some_and(|entry| entry.session.is_none());
        let removed = self.sessions.remove(&session_id).map(|entry| {
            if let Some(cancel) = &entry.cancel {
                cancel.cancel();
            }
            entry
        });
        super::reject_session_approvals(&pending, &session_id);
        super::drop_session_questions(&pending_questions, &session_id);
        self.deleted_sessions.insert(session_id.clone());
        self.pending_replays.retain(|id| id != &session_id);
        let pending_already = self.pending_deletions.contains_key(&session_id);
        self.pending_deletions
            .entry(session_id.clone())
            .or_insert(super::PendingDeletion {
                executions_idle: false,
                tasks_idle: removed.is_none(),
                turn_finished: !turn_in_flight,
            });
        if let Some(entry) = removed {
            let registry = entry.state.agents.clone();
            let deletion_tx = self.deletion_tx.clone();
            let deletion_events = event_tx.clone();
            let id = session_id.clone();
            let task_tx = deletion_tx.clone();
            let task_events = deletion_events.clone();
            let task_id = id.clone();
            tokio::spawn(async move {
                stop_session_tasks_until_idle(entry, task_events).await;
                let _ = task_tx
                    .send(super::DeletionProgress::TasksIdle(task_id))
                    .await;
            });
            tokio::spawn(async move {
                if tokio::time::timeout(super::SESSION_DELETE_GRACE, registry.wait_for_idle())
                    .await
                    .is_err()
                {
                    super::emit(
                        &deletion_events,
                        Event::Error {
                            session_id: Some(id.clone()),
                            error: CoreError::TaskStore {
                                detail: "Timed out waiting for Agent executions to stop; deletion remains pending and no data was removed".to_string(),
                            },
                            fatal: false,
                        },
                    );
                }
                registry.wait_for_idle().await;
                let _ = deletion_tx
                    .send(super::DeletionProgress::ExecutionsIdle(id))
                    .await;
            });
        } else if !pending_already
            && let Some(deletion) = self.pending_deletions.get_mut(&session_id)
        {
            deletion.executions_idle = true;
        }
        self.finish_delete_if_idle(event_tx, session_id);
    }

    pub(super) fn finish_delete_if_idle(
        &mut self,
        event_tx: async_channel::Sender<Event>,
        session_id: String,
    ) {
        let ready = self
            .pending_deletions
            .get(&session_id)
            .is_some_and(|deletion| {
                deletion.executions_idle && deletion.tasks_idle && deletion.turn_finished
            });
        if !ready {
            return;
        }
        if let Err(detail) = delete_session_artifacts(&self.sessions_dir, &session_id, true) {
            emit(
                &event_tx,
                Event::Error {
                    session_id: Some(session_id),
                    error: CoreError::TaskStore { detail },
                    fatal: false,
                },
            );
            return;
        }
        if let Err(error) = self
            .store
            .lock()
            .expect("store lock")
            .delete_session_checked(&session_id)
        {
            emit(
                &event_tx,
                Event::Error {
                    session_id: Some(session_id),
                    error,
                    fatal: false,
                },
            );
            return;
        }
        self.pending_deletions.remove(&session_id);
        emit(
            &event_tx,
            Event::SessionList {
                sessions: self.store.lock().expect("store lock").sorted_sessions(),
            },
        );
    }
}

pub(super) fn delete_session_artifacts(
    sessions_dir: &std::path::Path,
    session_id: &str,
    include_rollout: bool,
) -> Result<(), String> {
    let mut components = std::path::Path::new(session_id).components();
    let plain_id = matches!(components.next(), Some(std::path::Component::Normal(_)))
        && components.next().is_none();
    if !plain_id {
        return Err(format!(
            "Invalid session id for artifact cleanup: {session_id}"
        ));
    }
    let directories = vec![
        sessions_dir.join(session_id).join("tool-results"),
        pig_utils::paths::media_dir(sessions_dir, session_id),
        crate::agent::agents_dir(sessions_dir, session_id),
    ];
    let mut failures = Vec::new();
    if let Some(data_dir) = sessions_dir.parent() {
        let worktrees = data_dir.join("worktrees").join(session_id);
        if let Err(error) = crate::agent::remove_session_worktrees(&worktrees) {
            failures.push(error);
        }
    }
    for path in directories {
        if let Err(error) = std::fs::remove_dir_all(&path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            failures.push(format!("Failed to remove {}: {error}", path.display()));
        }
    }
    if include_rollout {
        let path = sessions_dir.join(format!("{session_id}.jsonl"));
        if let Err(error) = std::fs::remove_file(&path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            failures.push(format!("Failed to remove {}: {error}", path.display()));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_cleanup_reports_failures_and_can_be_retried() {
        let root = std::env::temp_dir().join(format!(
            "pig-delete-artifacts-{}-{}",
            std::process::id(),
            crate::model_io::now_ms()
        ));
        let sessions = root.join("sessions");
        std::fs::create_dir_all(&sessions).expect("sessions");
        let media = pig_utils::paths::media_dir(&sessions, "s1");
        std::fs::write(&media, "not a directory").expect("blocking file");
        let error = delete_session_artifacts(&sessions, "s1", false).expect_err("cleanup error");
        assert!(error.contains("Failed to remove"), "{error}");
        std::fs::remove_file(&media).expect("remove blocker");
        delete_session_artifacts(&sessions, "s1", false).expect("retry cleanup");
        assert!(delete_session_artifacts(&sessions, "../s1", false).is_err());
        let _ = std::fs::remove_dir_all(root);
    }
}
