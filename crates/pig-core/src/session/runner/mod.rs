use super::*;

pub(super) mod entry;
mod notifications;
mod operations;
mod plugin_sessions;
mod plugins;
mod queue;
mod runtime;
mod services;
mod trust;

use entry::{SessionEntry, TurnFuture};
use plugin_sessions::PluginTurnWaiter;
use queue::{queue_len, queue_snapshot, start_queued_head, steer_snapshot};
use runtime::install_sandbox_settings;
#[cfg(test)]
use trust::persist_workspace_trust;

/// One agent-manager instance: the sessions it owns and the shared stores they
/// use. Turn futures live next to the loop (`turns`) rather than in here so the
/// select loop can poll them without borrowing the manager.
#[derive(Default)]
pub(super) struct PendingDeletion {
    executions_idle: bool,
    tasks_idle: bool,
    turn_finished: bool,
}

pub(super) enum DeletionProgress {
    ExecutionsIdle(String),
    TasksIdle(String),
}

const SESSION_DELETE_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

pub(super) struct Manager {
    config_path: PathBuf,
    sessions_dir: PathBuf,
    data_dir: PathBuf,
    default_cwd: PathBuf,
    config: Option<AppConfig>,
    credentials: std::sync::Arc<crate::credentials::CredentialManager>,
    pending: PendingApprovals,
    pending_questions: PendingQuestions,
    store: std::sync::Arc<std::sync::Mutex<Store>>,
    plugins: crate::plugins::SharedPluginRegistry,
    sessions: HashMap<String, SessionEntry>,
    deleted_sessions: std::collections::HashSet<String>,
    pending_deletions: HashMap<String, PendingDeletion>,
    deletion_tx: tokio::sync::mpsc::Sender<DeletionProgress>,
    deletion_rx: tokio::sync::mpsc::Receiver<DeletionProgress>,
    pending_terminal: Vec<(
        String,
        crate::store::BackgroundTask,
        pig_protocol::TaskNotification,
    )>,
    pending_replays: Vec<String>,
    id_counter: u64,
    task_notify_tx: async_channel::Sender<crate::task::TaskChange>,
    /// Plugin turns waited on by `PluginSessionAction::Wait`.
    plugin_waiters: Vec<PluginTurnWaiter>,
    op_rx: async_channel::Receiver<Op>,
    task_notify_rx: async_channel::Receiver<crate::task::TaskChange>,
}

pub(super) fn emit(event_tx: &async_channel::Sender<Event>, event: Event) {
    let event = match event {
        Event::Error {
            session_id: Some(session_id),
            error,
            fatal,
        } => Event::Error {
            error: error.with_context(pig_protocol::CoreErrorContext {
                agent_id: session_id.clone(),
                session_id: session_id.clone(),
                run_id: None,
                attempt: None,
                call_id: None,
            }),
            session_id: Some(session_id),
            fatal,
        },
        other => other,
    };
    let _ = event_tx.send_blocking(event);
}

impl Manager {
    fn new(
        event_tx: &async_channel::Sender<Event>,
        op_rx: async_channel::Receiver<Op>,
        config_path: Option<PathBuf>,
        default_cwd: PathBuf,
        data_dir: PathBuf,
        credentials: std::sync::Arc<crate::credentials::CredentialManager>,
    ) -> Self {
        let config_path = config_path.unwrap_or_else(config::default_path);
        // First launch: write the sensitive-file deny groups into the user's own
        // config before it is read, so the seeded table is in force from the very
        // first command (a fresh install must not run unprotected until the first
        // save). A file that already has `[sandbox] deny` — even an empty list — is
        // left alone; a broken one is reported by the load below.
        match config::ensure_seeded_deny_rules(&config_path) {
            Ok(true) => tracing::info!("seeded the default read-deny groups"),
            Ok(false) => {}
            Err(error) => {
                tracing::warn!("could not seed the default read-deny groups: {error:?}");
            }
        }
        let mut load_error: Option<CoreError> = None;
        let config: Option<AppConfig> = match config::load(&config_path) {
            Ok(config) => Some(config),
            Err(error) => {
                load_error = Some(error);
                None
            }
        };
        let sessions_dir = data_dir.join("sessions");
        if let Some(config) = &config {
            install_sandbox_settings(config);
        }
        let store = std::sync::Arc::new(std::sync::Mutex::new(
            Store::open(&data_dir).unwrap_or_else(|e| panic!("store initialization failed: {e}")),
        ));
        let pending: PendingApprovals = std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));
        let pending_questions: PendingQuestions =
            std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));
        if let Some(error) = &load_error {
            // Structured config-load failure is sent directly (GUI appends a "check config.toml" hint by kind)
            emit(
                event_tx,
                Event::Error {
                    session_id: None,
                    error: error.clone(),
                    fatal: false,
                },
            );
        }
        let (task_notify_tx, task_notify_rx) =
            async_channel::bounded::<crate::task::TaskChange>(1024);
        let (deletion_tx, deletion_rx) = tokio::sync::mpsc::channel(256);
        Self {
            config_path,
            sessions_dir,
            data_dir: data_dir.clone(),
            default_cwd,
            config,
            credentials,
            pending,
            pending_questions,
            store,
            plugins: crate::plugins::PluginRegistry::shared(data_dir.clone()),
            sessions: HashMap::new(),
            deleted_sessions: std::collections::HashSet::new(),
            pending_deletions: HashMap::new(),
            deletion_tx,
            deletion_rx,
            pending_terminal: Vec::new(),
            pending_replays: Vec::new(),
            id_counter: 0,
            task_notify_tx,
            plugin_waiters: Vec::new(),
            op_rx,
            task_notify_rx,
        }
    }

    /// Dispatch one Op. Returns false when the manager must shut down.
    fn handle_op(
        &mut self,
        op: Op,
        event_tx: async_channel::Sender<Event>,
        turns: &mut FuturesUnordered<TurnFuture>,
    ) -> bool {
        // Fork = persist a derived new session, then open it via the OpenSession cold path
        // (SessionConfigured + full replay reuse; the UI switches automatically through existing flows)
        let op = match op {
            Op::ForkSession {
                session_id,
                turns: fork_turns,
            } => {
                match super::fork::fork_session(
                    &self.sessions_dir,
                    &self.store,
                    &session_id,
                    fork_turns,
                    &mut self.id_counter,
                ) {
                    Ok(new_id) => {
                        // The cold path does not emit SessionList: emit one so the forked session appears in the sidebar immediately
                        emit(
                            &event_tx,
                            Event::SessionList {
                                sessions: self.store.lock().expect("store lock").sorted_sessions(),
                            },
                        );
                        Op::OpenSession { session_id: new_id }
                    }
                    Err(error) => {
                        // Structured fork errors are forwarded as-is (no message wrapping)
                        emit(
                            &event_tx,
                            Event::Error {
                                session_id: Some(session_id),
                                error,
                                fatal: false,
                            },
                        );
                        return true;
                    }
                }
            }
            other => other,
        };
        match op {
            Op::NewSession {
                cwd,
                provider_id,
                model_id,
                reasoning_level,
                permissions,
                plan_enabled,
            } => self.new_session(
                event_tx,
                cwd,
                provider_id,
                model_id,
                reasoning_level,
                permissions,
                plan_enabled,
            ),
            Op::OpenSession { session_id } => self.open_session(event_tx, session_id),
            Op::ListWorkspaces => self.list_workspaces(event_tx),
            Op::AddWorkspace { path } => self.add_workspace(event_tx, path),
            Op::RemoveWorkspace { path } => self.remove_workspace(event_tx, path),
            Op::RenameWorkspace { path, alias } => self.rename_workspace(event_tx, path, alias),
            Op::ListSessions => self.list_sessions(event_tx),
            Op::UpdateSessionMeta {
                session_id,
                pinned,
                archived,
                title,
            } => self.update_session_meta(event_tx, session_id, pinned, archived, title),
            Op::DeleteSession { session_id } => self.delete_session(event_tx, session_id),
            Op::SendMessage {
                session_id,
                content,
                files,
                images,
                permissions,
                delivery,
            } => self.send_message(
                event_tx,
                session_id,
                content,
                files,
                images,
                permissions,
                delivery,
                None,
                turns,
            ),
            Op::SendExecutionMessage {
                session_id,
                message_id,
                content,
                permissions,
                delivery,
            } => self.send_message(
                event_tx,
                session_id,
                content,
                Vec::new(),
                Vec::new(),
                permissions,
                delivery,
                Some(message_id),
                turns,
            ),
            Op::CancelQueued { session_id, id } => self.cancel_queued(event_tx, session_id, id),
            Op::ReorderQueued {
                session_id,
                id,
                before_id,
            } => self.reorder_queued(event_tx, session_id, id, before_id),
            Op::MoveQueuedLane {
                session_id,
                id,
                steer,
            } => self.move_queued_lane(event_tx, session_id, id, steer),
            Op::SendQueuedNow { session_id, id } => {
                self.send_queued_now(event_tx, session_id, id, turns)
            }
            Op::ResumeQueue { session_id } => self.resume_queue(event_tx, session_id, turns),
            Op::ResolveHeldQueue {
                session_id,
                disposition,
            } => self.resolve_held_queue(event_tx, session_id, disposition, turns),
            Op::GitInfo { cwd } => self.git_info(event_tx, cwd),
            Op::CheckoutBranch { cwd, branch } => self.checkout_branch(event_tx, cwd, branch),
            Op::GitStatus { cwd } => self.git_status(event_tx, cwd),
            Op::GitDiff { cwd, path, staged } => self.git_diff(event_tx, cwd, path, staged),
            Op::ListAgents { session_id } => self.list_agents(event_tx, session_id),
            Op::AgentOutput {
                session_id,
                agent_id,
            } => self.agent_output(event_tx, session_id, agent_id),
            Op::LoadSubagent {
                session_id,
                agent_id,
            } => self.load_subagent(event_tx, session_id, agent_id),
            Op::Interrupt { session_id } => self.interrupt(session_id),
            Op::RetryNow { session_id } => self.retry_now(session_id),
            Op::ApprovalReply {
                request_id,
                decision,
                feedback,
            } => self.approval_reply(request_id, decision, feedback),
            Op::QuestionReply {
                request_id,
                answers,
            } => self.question_reply(request_id, answers),
            Op::SetPermissions {
                session_id,
                permissions,
            } => self.set_permissions(session_id, permissions),
            Op::SetPlanMode {
                session_id,
                enabled,
            } => self.set_plan_mode(event_tx, session_id, enabled),
            Op::RevertFile { session_id, path } => self.revert_file(event_tx, session_id, path),
            Op::RecallLastMessage { session_id } => self.recall_last_message(event_tx, session_id),
            Op::SearchFiles {
                session_id,
                query,
                cwd,
            } => self.search_files(event_tx, session_id, query, cwd),
            Op::ModelLookup { id } => self.model_lookup(event_tx, id),
            Op::Compact {
                session_id,
                instruction,
            } => self.compact(event_tx, session_id, instruction, turns),
            Op::SetModel {
                session_id,
                provider_id,
                model_id,
                reasoning_level,
            } => self.set_model(session_id, provider_id, model_id, reasoning_level),
            Op::SetReasoning {
                session_id,
                reasoning_level,
            } => self.set_reasoning(session_id, reasoning_level),
            Op::GetConfig => self.get_config(event_tx),
            Op::SaveConfig { config } => self.save_config(event_tx, config),
            Op::TestProvider { provider_id } => self.test_provider(event_tx, provider_id),
            Op::SetProviderApiKey {
                provider_id,
                secret,
            } => self.set_provider_api_key(event_tx, provider_id, secret),
            Op::ClearProviderApiKey { provider_id } => {
                self.clear_provider_api_key(event_tx, provider_id)
            }
            Op::ListMcpServers { session_id } => self.list_mcp_servers(event_tx, session_id),
            Op::StartMcpOAuth {
                session_id,
                server_name,
            } => self.start_mcp_oauth(event_tx, session_id, server_name),
            Op::ClearMcpOAuth {
                session_id,
                server_name,
            } => self.clear_mcp_oauth(event_tx, session_id, server_name),
            Op::TrustProjectHooks { session_id } => self.trust_project_hooks(event_tx, session_id),
            Op::SyncPlugins { packages } => self.sync_plugins(event_tx, packages),
            Op::SetPluginEnabled {
                package,
                digest,
                enabled,
            } => self.set_plugin_enabled(event_tx, package, digest, enabled),
            Op::SyncPluginRuntimeContributions {
                request_id,
                package,
                digest,
                epoch,
                generation,
                contributions,
            } => self.sync_plugin_runtime_contributions(
                event_tx,
                pig_protocol::PluginRuntimeContributionRequest {
                    request_id,
                    package,
                    digest,
                    epoch,
                    generation,
                    contributions,
                },
            ),
            Op::CommitPluginUpdate { package, digest } => {
                self.finish_plugin_update(event_tx, package, digest, true)
            }
            Op::CancelPluginUpdate { package, digest } => {
                self.finish_plugin_update(event_tx, package, digest, false)
            }
            Op::RefreshPluginMarketplace => self.refresh_plugin_marketplace(event_tx),
            Op::InstallMarketplacePlugin { package } => {
                self.install_marketplace_plugin(event_tx, package)
            }
            Op::QueryPlugin {
                request_id,
                package,
                digest,
                scope,
                session_id,
            } => self.query_plugin(event_tx, request_id, package, digest, scope, session_id),
            Op::PluginAction {
                request_id,
                package,
                digest,
                action,
                session_id,
            } => self.plugin_action(event_tx, request_id, package, digest, action, session_id),
            Op::PluginToolResult { result } => self.plugin_tool_result(result),
            Op::PluginToolsSelection { selection } => self.plugin_tools_selection(selection),
            Op::PluginSessionAction {
                request_id,
                package,
                digest,
                action,
            } => self.plugin_session_action(event_tx, request_id, package, digest, action, turns),
            Op::PluginGitRead {
                request_id,
                package,
                digest,
                session_id,
                request,
            } => self.plugin_git_read(event_tx, request_id, package, digest, session_id, request),
            Op::PluginWorkspaceFileRead {
                request_id,
                package,
                digest,
                session_id,
                request,
            } => self.plugin_workspace_file_read(
                event_tx, request_id, package, digest, session_id, request,
            ),
            Op::PluginHttpRequest {
                request_id,
                package,
                digest,
                request,
            } => self.plugin_http_request(event_tx, request_id, package, digest, request),
            Op::SetPluginSecret {
                request_id,
                package,
                digest,
                credential,
                secret,
            } => self.set_plugin_secret(event_tx, request_id, package, digest, credential, secret),
            Op::ClearPluginSecret {
                request_id,
                package,
                digest,
                credential,
            } => self.clear_plugin_secret(event_tx, request_id, package, digest, credential),
            Op::SetPluginCredentialGrant {
                package,
                digest,
                credential,
                origin,
                allowed,
            } => self.set_plugin_credential_grant(
                event_tx, package, digest, credential, origin, allowed,
            ),
            Op::Shutdown => return false,
            Op::ForkSession { .. } => {
                unreachable!("ForkSession is converted to OpenSession before dispatch")
            }
        }
        true
    }

    fn turn_finished(
        &mut self,
        event_tx: async_channel::Sender<Event>,
        session_id: String,
        session: Session,
        outcome: TurnOutcome,
        turns: &mut FuturesUnordered<TurnFuture>,
    ) {
        let store = self.store.clone();
        if self.deleted_sessions.contains(&session_id) {
            drop(session);
            if let Some(deletion) = self.pending_deletions.get_mut(&session_id) {
                deletion.turn_finished = true;
            }
            self.finish_delete_if_idle(event_tx, session_id);
            return;
        }
        self.refresh_notification_queue(&event_tx, &session_id);
        self.settle_plugin_waiters(&event_tx, &session_id, &outcome);
        let sessions = &mut self.sessions;
        if let Some(entry) = sessions.get_mut(&session_id) {
            // Refresh the MCP status cache at turn wrap-up (lazy connection happens inside the turn)
            entry.mcp_status = session.mcp.as_ref().map(|mcp| mcp.statuses());
            entry.active_run = None;
            entry.session = Some(session);
            entry.cancel = None;
            entry.retry = None;
            entry.steerable = false;
            if let Some(request_id) = entry.active_claim.take() {
                let result = store
                    .lock()
                    .expect("store lock")
                    .clear_execution_claim(&session_id, &request_id);
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
            // A SaveConfig or trust decision that arrived mid-turn becomes
            // effective here, once, before any queued turn starts. The entry
            // and the Session share the runtime through an `Arc`, so this is
            // also the sync point for a config/trust refresh that happened
            // while the turn owned it: `apply_trust` refreshes every project
            // surface when the decision changed and is a no-op otherwise.
            if let Some(session) = entry.session.as_mut() {
                session.set_runtime(entry.runtime.clone());
                if entry.plugin_surfaces_stale {
                    session.refresh_plugin_surfaces();
                    entry.plugin_surfaces_stale = false;
                    entry.mcp_status = None;
                }
            }
            // Unconsumed steer items fall back to the queue lane IN
            // PLACE (steer flag flip): the turn ended before a safe
            // boundary could inject them (abort, error, compaction),
            // and the global admission order stays intact
            let fell_back = {
                let mut pending = entry.pending.lock().expect("pending lock");
                let count = pending.iter().filter(|m| m.steer).count();
                for message in pending.iter_mut().filter(|m| m.steer) {
                    message.steer = false;
                }
                count
            };
            if fell_back > 0 {
                let items = steer_snapshot(&entry.pending);
                emit(
                    &event_tx,
                    Event::SteerChanged {
                        session_id: session_id.clone(),
                        items,
                    },
                );
                // Show the flipped items in the queue lane before the
                // head pop below may start one of them (a consistent
                // event history for the UI rebuild)
                let items = queue_snapshot(&entry.pending);
                emit(
                    &event_tx,
                    Event::QueueChanged {
                        session_id: session_id.clone(),
                        items,
                    },
                );
            }
            // Queue drain policy: only a completed turn on an
            // auto-draining queue may start the next item. A user stop
            // or a failure pauses a non-empty queue; a manual compact
            // (Completed) never unpauses a paused queue; an internal
            // promotion cancel (SendQueuedNow) keeps draining.
            let notifications_pending = !entry
                .notifications
                .lock()
                .expect("notification lock")
                .is_empty();
            let queued = queue_len(entry) + usize::from(notifications_pending);
            let drain = match outcome.status {
                crate::session::AgentRunStatus::Completed => entry.auto_drain,
                crate::session::AgentRunStatus::Cancelled => {
                    entry.auto_drain && entry.cancel_promote
                }
                crate::session::AgentRunStatus::Failed => false,
                crate::session::AgentRunStatus::Queued
                | crate::session::AgentRunStatus::Running
                | crate::session::AgentRunStatus::Lost => {
                    unreachable!("settled agent runs have terminal outcomes")
                }
            };
            entry.cancel_promote = false;
            if let Some(completion) = outcome.completion.clone() {
                emit(
                    &event_tx,
                    Event::TurnComplete {
                        session_id: session_id.clone(),
                        duration_ms: completion.duration_ms,
                        run: completion.run,
                        stats: completion.stats,
                        finish_reason: completion.finish_reason,
                    },
                );
            }
            if !drain
                && !matches!(outcome.status, crate::session::AgentRunStatus::Completed)
                && queued > 0
            {
                entry.auto_drain = false;
                let reason = match outcome.status {
                    crate::session::AgentRunStatus::Failed => pig_protocol::QueuePauseReason::Error,
                    _ => pig_protocol::QueuePauseReason::Stopped,
                };
                emit(
                    &event_tx,
                    Event::QueueDrainChanged {
                        session_id: session_id.clone(),
                        auto_drain: false,
                        reason: Some(reason),
                    },
                );
                super::persist_pending(&store, &session_id, &entry.pending);
            } else if drain {
                if queue_len(entry) > 0 {
                    start_queued_head(entry, &session_id, &event_tx, turns, &store);
                } else {
                    notifications::start_notification_turn(
                        entry,
                        &session_id,
                        &event_tx,
                        turns,
                        &store,
                    );
                }
            } else {
                super::persist_pending(&store, &session_id, &entry.pending);
            }
        }
        let pending_replays = std::mem::take(&mut self.pending_replays);
        for session_id in pending_replays {
            self.replay_loaded_session(&event_tx, session_id);
        }
    }
}

/// Agent manager: multiple concurrent sessions, each with its own turn/cancel/approval table.
pub async fn agent_loop(
    op_rx: async_channel::Receiver<Op>,
    event_tx: async_channel::Sender<Event>,
    config_path: Option<PathBuf>,
    default_cwd: PathBuf,
    data_dir: PathBuf,
    credentials: std::sync::Arc<crate::credentials::CredentialManager>,
) {
    let mut manager = Manager::new(
        &event_tx,
        op_rx,
        config_path,
        default_cwd,
        data_dir,
        credentials,
    );
    if let Some(config) = manager.config.as_ref() {
        manager
            .credentials
            .refresh_providers(&config.providers)
            .await;
    }
    let deletion_requests = manager
        .store
        .lock()
        .expect("store lock")
        .pending_session_deletions();
    match deletion_requests {
        Ok(requests) => {
            for session_id in requests {
                manager.deleted_sessions.insert(session_id.clone());
                manager.pending_deletions.insert(
                    session_id.clone(),
                    PendingDeletion {
                        executions_idle: true,
                        tasks_idle: true,
                        turn_finished: true,
                    },
                );
                manager.finish_delete_if_idle(event_tx.clone(), session_id);
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
    let mut turns: FuturesUnordered<TurnFuture> = FuturesUnordered::new();
    loop {
        #[allow(clippy::large_enum_variant)] // one event at a time; a turn owns a full Session
        enum Incoming {
            Op(Op),
            TaskNotify(crate::task::TaskChange),
            Turn(String, Session, TurnOutcome),
            DeletionIdle(DeletionProgress),
            RetryTerminal,
            Tick,
        }
        let has_terminal_retries = !manager.pending_terminal.is_empty();
        let incoming = {
            let Manager {
                op_rx,
                task_notify_rx,
                deletion_rx,
                ..
            } = &mut manager;
            tokio::select! {
                op = op_rx.recv() => match op {
                    Ok(op) => Incoming::Op(op),
                    Err(_) => break,
                },
                change = task_notify_rx.recv() => match change {
                    Ok(change) => Incoming::TaskNotify(change),
                    Err(_) => break,
                },
                Some(progress) = deletion_rx.recv() => Incoming::DeletionIdle(progress),
                Some((session_id, session, outcome)) = turns.next(), if !turns.is_empty() => {
                    Incoming::Turn(session_id, session, outcome)
                }
                _ = tokio::time::sleep(std::time::Duration::from_secs(1)), if has_terminal_retries => {
                    Incoming::RetryTerminal
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => Incoming::Tick,
            }
        };
        match incoming {
            Incoming::Op(op) => {
                if !manager.handle_op(op, event_tx.clone(), &mut turns) {
                    break;
                }
            }
            Incoming::TaskNotify(change) => {
                manager.task_changed(event_tx.clone(), change, &mut turns)
            }
            Incoming::Turn(session_id, session, outcome) => {
                manager.turn_finished(event_tx.clone(), session_id, session, outcome, &mut turns)
            }
            Incoming::DeletionIdle(progress) => {
                let session_id = match progress {
                    DeletionProgress::ExecutionsIdle(session_id) => {
                        if let Some(deletion) = manager.pending_deletions.get_mut(&session_id) {
                            deletion.executions_idle = true;
                        }
                        session_id
                    }
                    DeletionProgress::TasksIdle(session_id) => {
                        if let Some(deletion) = manager.pending_deletions.get_mut(&session_id) {
                            deletion.tasks_idle = true;
                        }
                        session_id
                    }
                };
                manager.finish_delete_if_idle(event_tx.clone(), session_id);
            }
            Incoming::RetryTerminal => {
                manager.retry_terminal_notifications(&event_tx);
                for session_id in manager.sessions.keys().cloned().collect::<Vec<_>>() {
                    manager.refresh_notification_queue(&event_tx, &session_id);
                    manager.start_notification_turn_if_ready(&event_tx, &session_id, &mut turns);
                }
            }
            Incoming::Tick => {
                manager.expire_plugin_waiters(&event_tx);
                let pending = manager
                    .pending_deletions
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>();
                for session_id in pending {
                    manager.finish_delete_if_idle(event_tx.clone(), session_id);
                }
            }
        }
    }
}

#[cfg(test)]
mod trust_persist_tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "pig-trust-persist-{tag}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|duration| duration.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&dir).expect("temp dir");
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Without an in-memory config there is nothing to persist: trust must not
    /// be activated from a decision that could not be written.
    #[test]
    fn a_missing_config_snapshot_is_rejected_without_writing() {
        let tmp = TempDir::new("missing");
        let cwd = tmp.path().join("workspace");
        std::fs::create_dir_all(&cwd).unwrap();
        let config_path = tmp.path().join("config.toml");
        let mut config: Option<AppConfig> = None;
        let error = persist_workspace_trust(
            &mut config,
            &config_path,
            &cwd,
            std::sync::Arc::new(crate::credentials::CredentialManager::mock()),
        )
        .expect_err("no snapshot means no trust");
        assert!(
            format!("{error:?}").contains("could not be loaded"),
            "{error:?}"
        );
        assert!(
            !config_path.exists(),
            "nothing is written without a snapshot"
        );
    }

    /// A failed save leaves the in-memory user layer unchanged (no half-applied
    /// trust decision) and returns the structured write error.
    #[test]
    fn a_failed_save_leaves_the_in_memory_config_unchanged() {
        let tmp = TempDir::new("failed-save");
        let cwd = tmp.path().join("workspace");
        std::fs::create_dir_all(&cwd).unwrap();
        // A directory at the config path makes the atomic rename fail.
        let config_path = tmp.path().join("config.toml");
        std::fs::create_dir(&config_path).unwrap();
        let mut config = Some(AppConfig::default());
        let error = persist_workspace_trust(
            &mut config,
            &config_path,
            &cwd,
            std::sync::Arc::new(crate::credentials::CredentialManager::mock()),
        )
        .expect_err("save failure must abort activation");
        assert!(matches!(error, CoreError::ConfigWrite { .. }), "{error:?}");
        assert!(
            config
                .as_ref()
                .expect("snapshot kept")
                .hooks
                .trusted_projects
                .is_empty(),
            "the failed decision must not be applied in memory"
        );
    }

    /// Success writes the canonical workspace, updates the in-memory snapshot
    /// and returns the runtime loaded from exactly that saved file.
    #[test]
    fn a_successful_save_returns_the_exact_trusted_runtime() {
        let tmp = TempDir::new("success");
        let cwd = tmp.path().join("workspace");
        std::fs::create_dir_all(&cwd).unwrap();
        let config_path = tmp.path().join("config.toml");
        let mut config = Some(AppConfig::default());
        let runtime = persist_workspace_trust(
            &mut config,
            &config_path,
            &cwd,
            std::sync::Arc::new(crate::credentials::CredentialManager::mock()),
        )
        .expect("a writable config persists the decision");
        let canonical = cwd.canonicalize().expect("canonical workspace");
        assert!(
            runtime.trust().admits(&cwd),
            "the returned runtime carries this workspace's trust"
        );
        assert!(
            config
                .as_ref()
                .expect("snapshot")
                .hooks
                .trusted_projects
                .iter()
                .any(|entry| Path::new(entry) == canonical),
            "the in-memory user layer matches the saved decision"
        );
        let loaded = crate::config::load(&config_path).expect("reload the saved config");
        assert!(
            loaded
                .hooks
                .trusted_projects
                .iter()
                .any(|entry| Path::new(entry) == canonical),
            "the decision is durable"
        );
    }
}
