use super::*;

struct SessionEntry {
    session: Option<Session>,
    /// Tool state sharing an Arc with Session: still usable to fetch todo/task
    /// snapshots while the session is inside the turn future (session=None)
    state: crate::task::SessionToolState,
    cancel: Option<CancellationToken>,
    /// The active turn's retry control (Op::RetryNow reaches the in-flight
    /// retry wait through it); same lifetime as `cancel`
    retry: Option<pig_provider::CallControl>,
    model_override: Option<ModelSelection>,
    /// Session-level reasoning level: exists independently of the model override (applies to the configured default model when there is no override)
    reasoning_level: Option<String>,
    /// Exec mode of the most recent user message/manual switch: wake turns started by
    /// background subagent completion use it to start turns (not default, which would
    /// override the user's current mode)
    last_mode: ExecMode,
    /// Messages arriving mid-turn are queued here (FIFO) and auto-continue when the turn ends
    queue: std::collections::VecDeque<(
        String,
        Vec<String>,
        Vec<pig_protocol::PendingImage>,
        ExecMode,
    )>,
    /// MCP status cache: refreshed from the Session's manager at turn wrap-up or
    /// settings-page queries (None = not lazily connected yet; answered from this
    /// cache while the Session is out of hand mid-turn)
    mcp_status: Option<Vec<pig_protocol::McpServerStatus>>,
}
type TurnFuture = std::pin::Pin<Box<dyn Future<Output = (String, Session)>>>;
#[allow(clippy::too_many_arguments)]
fn start_turn(
    entry: &mut SessionEntry,
    session_id: String,
    content: String,
    files: Vec<String>,
    images: Vec<pig_protocol::PendingImage>,
    mode: ExecMode,
    config: &ResolvedModel,
    event_tx: &async_channel::Sender<Event>,
    turns: &FuturesUnordered<TurnFuture>,
) {
    let mut session = entry.session.take().expect("session present");
    session.set_mode(mode);
    if let Some(selection) = &entry.model_override {
        session.set_model(selection.clone());
    }
    let cancel = CancellationToken::new();
    let control = pig_provider::CallControl::new(cancel.clone());
    entry.cancel = Some(cancel);
    entry.retry = Some(control.clone());
    let tx = event_tx.clone();
    let config = config.clone();
    turns.push(Box::pin(async move {
        session
            .run_turn(content, files, images, &config, &tx, control)
            .await;
        (session_id, session)
    }));
}
/// Agent manager: multiple concurrent sessions, each with its own turn/cancel/approval table.
pub async fn agent_loop(
    op_rx: async_channel::Receiver<Op>,
    event_tx: async_channel::Sender<Event>,
    config_path: Option<PathBuf>,
    default_cwd: PathBuf,
    data_dir: PathBuf,
) {
    let config_path = config_path.unwrap_or_else(config::default_path);
    let mut load_error: Option<CoreError> = None;
    let mut config: Option<AppConfig> = match config::load(&config_path) {
        Ok(config) => Some(config),
        Err(error) => {
            load_error = Some(error);
            None
        }
    };
    let sessions_dir = data_dir.join("sessions");
    let store = Arc::new(Mutex::new(
        Store::open(&data_dir).unwrap_or_else(|e| panic!("store initialization failed: {e}")),
    ));
    let pending: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));
    let pending_questions: PendingQuestions = Arc::new(Mutex::new(HashMap::new()));
    let mut seq = 0u64;
    macro_rules! emit_global {
        ($event:expr) => {{
            seq += 1;
            let _ = event_tx.send_blocking($event);
        }};
    }
    if let Some(error) = &load_error {
        // Structured config-load failure is sent directly (GUI appends a "check config.toml" hint by kind)
        emit_global!(Event::Error {
            session_id: None,
            seq,
            error: error.clone(),
        });
    }
    let mut sessions: HashMap<String, SessionEntry> = HashMap::new();
    // Deleted sessions: the rollout file is deleted after the in-flight turn wraps up (Session drop -> handle closed)
    let mut deleted_sessions: HashSet<String> = HashSet::new();
    let mut turns: FuturesUnordered<TurnFuture> = FuturesUnordered::new();
    let mut id_counter = 0u64;
    // Background task completion notifications: the watcher sends session_id -> the select branch pushes TaskListChanged
    let (task_notify_tx, mut task_notify_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    // Background subagent completion wake-ups: (session_id, notification text) -> synthesize a user message to start a new turn
    let (wake_tx, mut wake_rx) = tokio::sync::mpsc::unbounded_channel::<(String, String)>();
    macro_rules! resolve {
        ($override:expr, $level:expr) => {
            config
                .as_ref()
                .and_then(|c| resolve_model(c, $override, $level))
        };
    }
    macro_rules! model_label {
        ($override:expr) => {
            resolve!($override, None)
                .map(|r| (r.model.clone(), r.provider_name.clone()))
                // No model configured: empty-string sentinel (GUI renders the localized "New task/No model" placeholder)
                .unwrap_or_else(|| (String::new(), String::new()))
        };
    }
    loop {
        tokio::select! {
            op = op_rx.recv() => {
                let Ok(op) = op else { break };
                // Fork = persist a derived new session, then open it via the OpenSession cold path
                // (SessionConfigured + full replay reuse; the UI switches automatically through existing flows)
                let op = match op {
                    Op::ForkSession { session_id, turns } => {
                        match fork::fork_session(&sessions_dir, &store, &session_id, turns, &mut id_counter) {
                            Ok(new_id) => {
                                // The cold path does not emit SessionList: emit one so the forked session appears in the sidebar immediately
                                emit_global!(Event::SessionList {
                                    sessions: store.lock().expect("store lock").sorted_sessions(),
                                });
                                Op::OpenSession { session_id: new_id }
                            }
                            Err(error) => {
                                // Structured fork errors are forwarded as-is (no message wrapping)
                                emit_global!(Event::Error {
                                    session_id: Some(session_id),
                                    seq,
                                    error,
                                });
                                continue;
                            }
                        }
                    }
                    other => other,
                };
                match op {
                    Op::NewSession { cwd, provider_id, model_id, reasoning_level, exec_mode, plan_enabled } => {
                        let cwd = normalize_workspace_path(&cwd);
                        id_counter += 1;
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
                            exec_mode: exec_mode.unwrap_or_else(|| {
                                seed.as_ref().map(|m| m.exec_mode).unwrap_or_default()
                            }),
                            // Plan mode is transient: not inherited from the seed, only set explicitly by the UI
                            plan_enabled: plan_enabled.unwrap_or(false),
                            // Outside-workspace read/write switches inherit from the workspace seed (same policy as exec_mode)
                            fs_read_outside: seed.as_ref().map(|m| m.fs_read_outside).unwrap_or(false),
                            fs_write_outside: seed.as_ref().map(|m| m.fs_write_outside).unwrap_or(false),
                        };
                        // UI left the reasoning level unset and the model config has a default -> adopt the default tier
                        // (written into meta; SessionConfigured syncs it back to the UI's level chip)
                        if meta.reasoning_level.is_none()
                            && let Some(cfg) = config.as_ref()
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
                        match Session::create(meta.clone(), pending.clone(), pending_questions.clone(), store.clone(), &sessions_dir, data_dir.clone(), task_notify_tx.clone(), wake_tx.clone(), config.as_ref()) {
                            Ok(mut session) => {
                                session.set_mode(meta.exec_mode);
                                session.set_plan_mode(meta.plan_enabled);
                                session.set_fs_access(meta.fs_read_outside, meta.fs_write_outside);
                                let selection = meta_to_selection(&meta);
                                let state = session.state.clone();
                                sessions.insert(id.clone(), SessionEntry { session: Some(session), state, cancel: None, retry: None, model_override: selection.clone(), reasoning_level: meta.reasoning_level.clone(), last_mode: meta.exec_mode, queue: Default::default(), mcp_status: None });
                                store.lock().expect("store lock").upsert_session(&meta);
                                let (model, provider_name) = model_label!(selection.as_ref());
                                emit_global!(Event::SessionConfigured {
                                    session_id: id.clone(),
                                    cwd: meta.cwd.clone(),
                                    model,
                                    provider_name,
                                    provider_id: meta.provider_id.clone(),
                                    model_id: meta.model_id.clone(),
                                    reasoning_level: meta.reasoning_level.clone(),
                                    exec_mode: meta.exec_mode,
                                    plan_enabled: meta.plan_enabled,
                                    fs_read_outside: meta.fs_read_outside,
                                    fs_write_outside: meta.fs_write_outside,
                                });
                                // New session: panels initialize to empty snapshots
                                emit_global!(Event::TodoListChanged { session_id: id.clone(), seq, items: vec![] });
                                emit_global!(Event::TaskListChanged { session_id: id.clone(), seq, tasks: vec![] });
                                emit_global!(Event::SessionList {
                                    sessions: store.lock().expect("store lock").sorted_sessions(),
                                });
                                // Creating a session under a removed (hidden) workspace: automatically unhide it
                                if store.lock().expect("store lock").unhide_workspace(&meta.cwd) {
                                    emit_global!(Event::WorkspaceList {
                                        workspaces: store.lock().expect("store lock").workspaces(),
                                    });
                                }
                            }
                            Err(error) => emit_global!(Event::Error {
                                session_id: None,
                                seq,
                                error,
                            }),
                        }
                    }
                    Op::OpenSession { session_id } => {
                        if sessions.contains_key(&session_id) {
                            let meta = store.lock().expect("store lock").get_session(&session_id);
                            if let Some(meta) = meta {
                                let selection = meta_to_selection(&meta);
                                let (model, provider_name) = model_label!(selection.as_ref());
                                emit_global!(Event::SessionConfigured {
                                    session_id: session_id.clone(),
                                    cwd: meta.cwd,
                                    model,
                                    provider_name,
                                    provider_id: meta.provider_id.clone(),
                                    model_id: meta.model_id.clone(),
                                    reasoning_level: meta.reasoning_level.clone(),
                                    exec_mode: meta.exec_mode,
                                    plan_enabled: meta.plan_enabled,
                                    fs_read_outside: meta.fs_read_outside,
                                    fs_write_outside: meta.fs_write_outside,
                                });
                                // Switching back to an already-open session: re-emit panel snapshots so the UI resets its panels
                                if let Some(entry) = sessions.get(&session_id) {
                                    let items = entry.state.todos.lock().expect("todos lock").clone();
                                    let tasks = crate::task::snapshot(&entry.state.tasks);
                                    emit_global!(Event::TodoListChanged { session_id: session_id.clone(), seq, items });
                                    emit_global!(Event::TaskListChanged { session_id: session_id.clone(), seq, tasks });
                                    // Re-emit the usage watermark/totals (the composer's capacity chip would otherwise keep stale values after switching sessions)
                                    if let Some(session) = &entry.session
                                        && let Some(used) = session.last_total_tokens
                                        && let Some(resolved) = resolve!(
                                            entry.model_override.as_ref(),
                                            entry.reasoning_level.as_deref()
                                        )
                                    {
                                        emit_global!(Event::ContextUsage {
                                            session_id: session_id.clone(),
                                            seq,
                                            used,
                                            total: resolved.context_window,
                                            cache_read_total: session.cache_read_total,
                                            input_total: session.input_total,
                                        });
                                    }
                                }
                            }
                            continue;
                        }
                        match Session::load(&session_id, &sessions_dir, pending.clone(), pending_questions.clone(), store.clone(), data_dir.clone(), task_notify_tx.clone(), wake_tx.clone(), config.as_ref()) {
                            Ok((mut session, records)) => {
                                // Restore persisted mode/model overrides (meta stays current via Set* write-through)
                                let meta = store.lock().expect("store lock").get_session(&session_id);
                                let selection = meta.as_ref().and_then(meta_to_selection);
                                if let Some(meta) = &meta {
                                    session.set_mode(meta.exec_mode);
                                    session.set_plan_mode(meta.plan_enabled);
                                    session.set_fs_access(meta.fs_read_outside, meta.fs_write_outside);
                                }
                                let cwd = session.cwd.clone();
                                let state = session.state.clone();
                                sessions.insert(session_id.clone(), SessionEntry {
                                    session: Some(session),
                                    state,
                                    cancel: None,
                                    retry: None,
                                    model_override: selection.clone(),
                                    reasoning_level: meta.as_ref().and_then(|m| m.reasoning_level.clone()),
                                    last_mode: meta.as_ref().map(|m| m.exec_mode).unwrap_or_default(),
                                    queue: Default::default(),
                                    mcp_status: None,
                                });
                                let (model, provider_name) = model_label!(selection.as_ref());
                                emit_global!(Event::SessionConfigured {
                                    session_id: session_id.clone(),
                                    cwd,
                                    model,
                                    provider_name,
                                    provider_id: meta.as_ref().and_then(|m| m.provider_id.clone()),
                                    model_id: meta.as_ref().and_then(|m| m.model_id.clone()),
                                    reasoning_level: meta.as_ref().and_then(|m| m.reasoning_level.clone()),
                                    exec_mode: meta.as_ref().map(|m| m.exec_mode).unwrap_or_default(),
                                    plan_enabled: meta.as_ref().is_some_and(|m| m.plan_enabled),
                                    fs_read_outside: meta.as_ref().map(|m| m.fs_read_outside).unwrap_or(false),
                                    fs_write_outside: meta.as_ref().map(|m| m.fs_write_outside).unwrap_or(false),
                                });
                                // Reopened sessions have no persisted panel state: reset with empty snapshots
                                emit_global!(Event::TodoListChanged { session_id: session_id.clone(), seq, items: vec![] });
                                emit_global!(Event::TaskListChanged { session_id: session_id.clone(), seq, tasks: vec![] });
                                if let Some(entry) = sessions.get_mut(&session_id)
                                    && let Some(session) = entry.session.as_mut() {
                                        session.replay(&records, &event_tx);
                                    }
                                // Replay already restored the usage watermark and totals: re-emit context capacity so
                                // reopening the app shows it without waiting for the next message (skipped when no model is configured; no window to report)
                                if let Some(entry) = sessions.get(&session_id)
                                    && let Some(session) = &entry.session
                                    && let Some(used) = session.last_total_tokens
                                    && let Some(resolved) =
                                        resolve!(entry.model_override.as_ref(), entry.reasoning_level.as_deref())
                                {
                                    emit_global!(Event::ContextUsage {
                                        session_id: session_id.clone(),
                                        seq,
                                        used,
                                        total: resolved.context_window,
                                        cache_read_total: session.cache_read_total,
                                        input_total: session.input_total,
                                    });
                                }
                            }
                            Err(error) => emit_global!(Event::Error {
                                session_id: None,
                                seq,
                                error,
                            }),
                        }
                    }
                    Op::ListWorkspaces => {
                        emit_global!(Event::WorkspaceList {
                            workspaces: store.lock().expect("store lock").workspaces(),
                        });
                    }
                    Op::AddWorkspace { path } => {
                        let path = normalize_workspace_path(&path);
                        store.lock().expect("store lock").add_workspace(&path);
                        emit_global!(Event::WorkspaceList {
                            workspaces: store.lock().expect("store lock").workspaces(),
                        });
                    }
                    Op::RemoveWorkspace { path } => {
                        let path = normalize_workspace_path(&path);
                        store.lock().expect("store lock").hide_workspace(&path);
                        emit_global!(Event::WorkspaceList {
                            workspaces: store.lock().expect("store lock").workspaces(),
                        });
                    }
                    Op::RenameWorkspace { path, alias } => {
                        let path = normalize_workspace_path(&path);
                        store.lock().expect("store lock").rename_workspace(&path, alias);
                        emit_global!(Event::WorkspaceList {
                            workspaces: store.lock().expect("store lock").workspaces(),
                        });
                    }
                    Op::ListSessions => {
                        emit_global!(Event::SessionList {
                            sessions: store.lock().expect("store lock").sorted_sessions(),
                        });
                    }
                    Op::UpdateSessionMeta { session_id, pinned, archived, title } => {
                        store.lock().expect("store lock").update_session(&session_id, |meta| {
                            if let Some(pinned) = pinned { meta.pinned = pinned; }
                            if let Some(archived) = archived { meta.archived = archived; }
                            if let Some(title) = &title {
                                meta.title = title.clone();
                                // Manual rename: auto-naming never overrides it afterwards
                                meta.title_custom = true;
                            }
                            meta.updated_at = now_secs();
                        });
                        emit_global!(Event::SessionList {
                            sessions: store.lock().expect("store lock").sorted_sessions(),
                        });
                    }
                    Op::DeleteSession { session_id } => {
                        // Turn in flight: cancel first; the Session is still inside the turn future,
                        // so the rollout file can only be deleted after the turn wraps up and drops it
                        let turn_in_flight = sessions
                            .get(&session_id)
                            .is_some_and(|e| e.session.is_none());
                        if let Some(entry) = sessions.remove(&session_id)
                            && let Some(cancel) = &entry.cancel
                        {
                            cancel.cancel();
                        }
                        // Idle: the Session drops with the removed entry; the handle is already closed
                        deleted_sessions.insert(session_id.clone());
                        store.lock().expect("store lock").delete_session(&session_id);
                        if !turn_in_flight {
                            let _ = std::fs::remove_file(
                                sessions_dir.join(format!("{session_id}.jsonl")),
                            );
                        }
                        emit_global!(Event::SessionList {
                            sessions: store.lock().expect("store lock").sorted_sessions(),
                        });
                    }
                    Op::SendMessage { session_id, content, files, images, mode } => {
                        let Some(entry) = sessions.get_mut(&session_id) else {
                            emit_global!(Event::Error {
                                session_id: Some(session_id),
                                seq,
                                error: CoreError::SessionNotFoundOpen,
                            });
                            continue;
                        };
                        // Record the latest mode: wake turns start turns with it (queued case included)
                        entry.last_mode = mode;
                        // Turn in progress -> queue the message; it auto-continues when the turn ends (Interrupt does not clear the queue)
                        if entry.session.is_none() {
                            entry.queue.push_back((content.clone(), files, images, mode));
                            emit_global!(Event::MessageQueued {
                                session_id: session_id.clone(),
                                seq,
                                text: content,
                            });
                            continue;
                        }
                        let Some(resolved) = resolve!(entry.model_override.as_ref(), entry.reasoning_level.as_deref()) else {
                            emit_global!(Event::Error {
                                session_id: Some(session_id),
                                seq,
                                error: CoreError::NoModelConfigured,
                            });
                            continue;
                        };
                        start_turn(entry, session_id, content, files, images, mode, &resolved, &event_tx, &turns);
                    }
                    Op::CancelQueued { session_id, text } => {
                        if let Some(entry) = sessions.get_mut(&session_id)
                            && let Some(pos) = entry.queue.iter().position(|(t, _, _, _)| t == &text) {
                                entry.queue.remove(pos);
                            }
                    }
                    Op::GitInfo { cwd } => {
                        let tx = event_tx.clone();
                        tokio::spawn(async move {
                            let cwd2 = cwd.clone();
                            let result = tokio::task::spawn_blocking(move || crate::git::git_info(&cwd2)).await;
                            if let Ok((current_branch, branches)) = result {
                                let _ = tx.send(Event::GitInfo {
                                    cwd,
                                    current_branch,
                                    branches,
                                }).await;
                            }
                        });
                    }
                    Op::CheckoutBranch { cwd, branch } => {
                        let tx = event_tx.clone();
                        tokio::spawn(async move {
                            let cwd2 = cwd.clone();
                            let branch2 = branch.clone();
                            let result = tokio::task::spawn_blocking(move || {
                                crate::git::checkout(&cwd2, &branch2)
                            })
                            .await;
                            match result {
                                Ok(Ok(())) => {
                                    let _ = tx.send(Event::BranchChanged { cwd, branch }).await;
                                }
                                Ok(Err(error)) => {
                                    let _ = tx.send(Event::Error {
                                        session_id: None,
                                        seq: 0,
                                        error,
                                    }).await;
                                }
                                Err(e) => {
                                    let _ = tx.send(Event::Error {
                                        session_id: None,
                                        seq: 0,
                                        error: CoreError::GitTask {
                                            detail: e.to_string(),
                                        },
                                    }).await;
                                }
                            }
                        });
                    }
                    Op::GitStatus { cwd } => {
                        let tx = event_tx.clone();
                        tokio::spawn(async move {
                            let cwd2 = cwd.clone();
                            let result =
                                tokio::task::spawn_blocking(move || crate::git::git_status(&cwd2))
                                    .await;
                            if let Ok(result) = result {
                                let (is_git, unstaged, staged) = match result {
                                    Some((unstaged, staged)) => (true, unstaged, staged),
                                    None => (false, vec![], vec![]),
                                };
                                let _ = tx
                                    .send(Event::GitStatus {
                                        cwd,
                                        is_git,
                                        unstaged,
                                        staged,
                                    })
                                    .await;
                            }
                        });
                    }
                    Op::GitDiff { cwd, path, staged } => {
                        let tx = event_tx.clone();
                        tokio::spawn(async move {
                            let cwd2 = cwd.clone();
                            let path2 = path.clone();
                            let result = tokio::task::spawn_blocking(move || {
                                crate::git::git_diff(&cwd2, &path2, staged)
                            })
                            .await;
                            if let Ok((diff, note)) = result {
                                let _ = tx
                                    .send(Event::GitDiff {
                                        cwd,
                                        path,
                                        staged,
                                        diff,
                                        note,
                                    })
                                    .await;
                            }
                        });
                    }
                    Op::LoadSubagent { session_id, agent_id } => {
                        // No Session instance needed: read the subagent context JSONL directly (read-only display in the right-side tab)
                        let tx = event_tx.clone();
                        let path = crate::agent::agents_dir(&sessions_dir, &session_id)
                            .join(format!("{agent_id}.jsonl"));
                        // "Running" criterion: same agent_id in the session task registry with status Running;
                        // session not in memory (not loaded) -> false
                        let running = sessions.get(&session_id).is_some_and(|entry| {
                            entry.state.tasks.lock().expect("task registry lock").iter().any(|t| {
                                t.agent_id.as_deref() == Some(agent_id.as_str())
                                    && matches!(t.status, pig_protocol::TaskStatus::Running)
                            })
                        });
                        tokio::spawn(async move {
                            let result =
                                tokio::task::spawn_blocking(move || crate::agent::read_agent(&path))
                                    .await;
                            match result {
                                Ok(Ok((meta, msgs))) => {
                                    let (title, subtitle, items) =
                                        crate::agent::display_items(&meta, &msgs);
                                    let _ = tx
                                        .send(Event::SubagentHistory {
                                            session_id,
                                            seq: 0,
                                            agent_id,
                                            title,
                                            subtitle,
                                            items,
                                            running,
                                        })
                                        .await;
                                }
                                Ok(Err(error)) => {
                                    let _ = tx
                                        .send(Event::Error {
                                            session_id: Some(session_id),
                                            seq: 0,
                                            error: CoreError::SubagentRead { detail: error },
                                        })
                                        .await;
                                }
                                Err(e) => {
                                    let _ = tx
                                        .send(Event::Error {
                                            session_id: Some(session_id),
                                            seq: 0,
                                            error: CoreError::SubagentReadTask {
                                                detail: e.to_string(),
                                            },
                                        })
                                        .await;
                                }
                            }
                        });
                    }
                    Op::Interrupt { session_id } => {
                        if let Some(entry) = sessions.get(&session_id)
                            && let Some(cancel) = &entry.cancel
                        {
                            cancel.cancel();
                        }
                    }
                    Op::RetryNow { session_id } => {
                        if let Some(entry) = sessions.get(&session_id)
                            && let Some(control) = &entry.retry
                        {
                            control.retry_now();
                        }
                    }
                    Op::ApprovalReply { request_id, decision, feedback } => {
                        // The decision also wakes concurrent waiters under the same merge key (see resolve_approval)
                        super::resolve_approval(&pending, &request_id, decision, feedback);
                    }
                    Op::QuestionReply { request_id, answers } => {
                        if let Some(reply) = pending_questions.lock().expect("pending questions lock").remove(&request_id) {
                            let _ = reply.send(answers);
                        }
                    }
                    Op::SetExecMode { session_id, mode } => {
                        if let Some(entry) = sessions.get_mut(&session_id) {
                            // Manual mode switch: wake turns follow the latest mode
                            entry.last_mode = mode;
                            if let Some(session) = entry.session.as_mut() {
                                session.set_mode(mode);
                            }
                            store.lock().expect("store lock").update_session(&session_id, |m| {
                                m.exec_mode = mode;
                            });
                        }
                    }
                    Op::SetPlanMode { session_id, enabled } => {
                        // The plan toggle is orthogonal to the mode tier: the UI is the initiator, so no PlanModeChanged is emitted back
                        if let Some(entry) = sessions.get_mut(&session_id)
                            && let Some(session) = entry.session.as_mut()
                        {
                            session.set_plan_mode(enabled);
                            store.lock().expect("store lock").update_session(&session_id, |m| {
                                m.plan_enabled = enabled;
                            });
                        }
                    }
                    Op::SetFsAccess { session_id, read_outside, write_outside } => {
                        if let Some(entry) = sessions.get_mut(&session_id) {
                            // state is a shared handle: also takes effect mid-turn (session=None)
                            entry.state.fs_read_outside.store(read_outside, std::sync::atomic::Ordering::Relaxed);
                            entry.state.fs_write_outside.store(write_outside, std::sync::atomic::Ordering::Relaxed);
                            store.lock().expect("store lock").update_session(&session_id, |m| {
                                m.fs_read_outside = read_outside;
                                m.fs_write_outside = write_outside;
                            });
                        }
                    }
                    Op::RevertFile { session_id, path } => {
                        match sessions.get_mut(&session_id) {
                            Some(entry) if entry.session.is_some() => {
                                entry.session.as_mut().expect("session").revert_file(&path, &event_tx);
                            }
                            Some(_) => emit_global!(Event::Error {
                                session_id: Some(session_id),
                                seq,
                                error: CoreError::RevertTurnInFlight,
                            }),
                            None => emit_global!(Event::Error {
                                session_id: Some(session_id),
                                seq,
                                error: CoreError::SessionNotFound,
                            }),
                        }
                    }
                    Op::SearchFiles { session_id, query, cwd } => {
                        let cwd = cwd
                            .map(std::path::PathBuf::from)
                            .or_else(|| sessions.get(&session_id)
                                .and_then(|e| e.session.as_ref().map(|s| s.cwd.clone())))
                            .unwrap_or_else(|| default_cwd.clone());
                        let tx = event_tx.clone();
                        let query_for_search = query.clone();
                        tokio::spawn(async move {
                            let results = tokio::task::spawn_blocking(move || {
                                tool::search_files(&cwd, &query_for_search, 20)
                            })
                            .await
                            .unwrap_or_default();
                            let _ = tx.send(Event::FileSearchResults {
                                session_id,
                                query,
                                results,
                            }).await;
                        });
                    }
                    Op::ModelLookup { id } => {
                        // Answer directly on cache hit; refetch only on a miss (new model ID) with a stale cache,
                        // to avoid hammering models.dev while the user tries IDs in the dialog
                        const REFETCH_AFTER_SECS: u64 = 10 * 60;
                        let tx = event_tx.clone();
                        let cache_path = data_dir.join("models-dev-cache.json");
                        tokio::spawn(async move {
                            let lookup_id = id.clone();
                            let loaded = tokio::task::spawn_blocking({
                                let cache_path = cache_path.clone();
                                move || crate::models_registry::load_cache(&cache_path)
                            })
                            .await
                            .ok()
                            .flatten();
                            let (fetched_at, index) = match loaded {
                                Some((_, index)) if index.contains_key(&lookup_id) => {
                                    // Hit: no network needed
                                    let _ = tx
                                        .send(Event::ModelInfo {
                                            id,
                                            info: index.get(&lookup_id).cloned(),
                                        })
                                        .await;
                                    return;
                                }
                                Some(pair) => pair,
                                None => (0, HashMap::new()),
                            };
                            let fresh =
                                fetched_at > 0 && crate::models_registry::unix_now() < fetched_at + REFETCH_AFTER_SECS;
                            let index = if fresh {
                                index
                            } else {
                                match crate::models_registry::fetch_index().await {
                                    Ok(fetched) => {
                                        let for_save = fetched.clone();
                                        let path2 = cache_path.clone();
                                        let _ = tokio::task::spawn_blocking(move || {
                                            crate::models_registry::save_cache(
                                                &path2,
                                                crate::models_registry::unix_now(),
                                                &for_save,
                                            );
                                        })
                                        .await;
                                        fetched
                                    }
                                    Err(_) => index,
                                }
                            };
                            let _ = tx
                                .send(Event::ModelInfo {
                                    id,
                                    info: index.get(&lookup_id).cloned(),
                                })
                                .await;
                        });
                    }
                    Op::Compact { session_id, instruction } => {
                        match sessions.get_mut(&session_id) {
                            Some(entry) if entry.session.is_some() => {
                                let mut session = entry.session.take().expect("session present");
                                let tx = event_tx.clone();
                                let sid = session_id.clone();
                                let resolved = resolve!(entry.model_override.as_ref(), entry.reasoning_level.as_deref());
                                entry.cancel = Some(CancellationToken::new());
                                let cancel = entry.cancel.clone().expect("cancel");
                                turns.push(Box::pin(async move {
                                    session.run_compact(resolved.as_ref(), false, instruction.as_deref(), &tx, &cancel).await;
                                    (sid, session)
                                }));
                            }
                            _ => emit_global!(Event::Error {
                                session_id: Some(session_id),
                                seq,
                                error: CoreError::CompactUnavailable,
                            }),
                        }
                    }
                    Op::SetModel { session_id, provider_id, model_id, reasoning_level } => {
                        if let Some(entry) = sessions.get_mut(&session_id) {
                            entry.model_override = Some(ModelSelection {
                                provider_id: provider_id.clone(),
                                model_id: model_id.clone(),
                                reasoning_level: reasoning_level.clone(),
                            });
                            entry.reasoning_level = reasoning_level.clone();
                            // Write-through to the sessions table: both reopen and new-session inheritance read from here
                            store.lock().expect("store lock").update_session(&session_id, |m| {
                                m.provider_id = Some(provider_id.clone());
                                m.model_id = Some(model_id.clone());
                                m.reasoning_level = reasoning_level.clone();
                            });
                        }
                    }
                    Op::SetReasoning { session_id, reasoning_level } => {
                        if let Some(entry) = sessions.get_mut(&session_id) {
                            // Saved independently of the model override: applies to the configured default model when there is no override (resolve's default_level)
                            entry.reasoning_level = reasoning_level.clone();
                            if let Some(sel) = &mut entry.model_override {
                                sel.reasoning_level = reasoning_level.clone();
                            }
                            store.lock().expect("store lock").update_session(&session_id, |m| {
                                m.reasoning_level = reasoning_level.clone();
                            });
                        }
                    }
                    Op::GetConfig => {
                        emit_global!(Event::ConfigSnapshot {
                            config: config.clone().unwrap_or_default(),
                        });
                    }
                    Op::SaveConfig { config: new_config } => {
                        if let Err(error) = config::save(&config_path, &new_config) {
                            emit_global!(Event::Error {
                                session_id: None,
                                seq,
                                error,
                            });
                        } else {
                            config = Some(new_config);
                        }
                        emit_global!(Event::ConfigSnapshot {
                            config: config.clone().unwrap_or_default(),
                        });
                    }
                    Op::TestProvider { provider_id } => {
                        let found = config.as_ref().and_then(|c| {
                            c.providers.iter().find(|p| p.id == provider_id).cloned()
                        });
                        let tx = event_tx.clone();
                        tokio::spawn(async move {
                            let Some(provider) = found else {
                                // Provider not found: structured error (a different channel from connection results)
                                let _ = tx.send(Event::Error {
                                    session_id: None,
                                    seq: 0,
                                    error: CoreError::ProviderNotFound,
                                }).await;
                                return;
                            };
                            let model = provider
                                .models
                                .first()
                                .map(|m| m.id.clone())
                                .unwrap_or_else(|| "ping".to_string());
                            let result = pig_provider::test_provider(
                                &provider.base_url,
                                &config::expand_env(&provider.api_key),
                                provider.api_format,
                                &model,
                            )
                            .await;
                            let _ = tx.send(Event::TestResult {
                                provider_id,
                                result,
                            }).await;
                        });
                    }
                    Op::ListMcpServers { session_id } => {
                        let servers = match sessions.get_mut(&session_id) {
                            Some(entry) => {
                                // Mid-turn (session=None) the manager is unreachable: answer from the cached list
                                if let Some(statuses) = entry
                                    .session
                                    .as_ref()
                                    .and_then(|session| session.mcp.as_ref())
                                    .map(|mcp| mcp.statuses())
                                {
                                    entry.mcp_status = Some(statuses);
                                }
                                entry.mcp_status.clone()
                            }
                            None => None,
                        };
                        emit_global!(Event::McpServerList { session_id, servers });
                    }
                    Op::Shutdown => break,
                    Op::ForkSession { .. } => {
                        unreachable!("ForkSession is converted to OpenSession before dispatch")
                    }
                }
            }
            Some(session_id) = task_notify_rx.recv() => {
                // Background task status change: push panel snapshots (entry.state shares an Arc with
                // the Session, so the registry is reachable while the session is inside the turn future)
                if let Some(entry) = sessions.get(&session_id) {
                    let tasks = crate::task::snapshot(&entry.state.tasks);
                    emit_global!(Event::TaskListChanged { session_id, seq, tasks });
                }
            }
            Some((session_id, content)) = wake_rx.recv() => {
                // Background subagent completion wake-up: synthesize a user message to start a new turn.
                // Busy/idle detection mirrors SendMessage: queue when busy (auto-continue at turn end);
                // the mode comes from last_mode (the user's most recent mode, no fallback to default)
                let Some(entry) = sessions.get_mut(&session_id) else {
                    continue;
                };
                if entry.session.is_none() {
                    entry.queue.push_back((content, vec![], vec![], entry.last_mode));
                    continue;
                }
                if let Some(resolved) = resolve!(entry.model_override.as_ref(), entry.reasoning_level.as_deref()) {
                    start_turn(entry, session_id, content, vec![], vec![], entry.last_mode, &resolved, &event_tx, &turns);
                }
            }
            Some((session_id, session)) = turns.next(), if !turns.is_empty() => {
                if deleted_sessions.contains(&session_id) {
                    // Turn wrap-up for a deleted session: the Session drops here; delete the file after the handle closes
                    let _ = std::fs::remove_file(sessions_dir.join(format!("{session_id}.jsonl")));
                    continue;
                }
                if let Some(entry) = sessions.get_mut(&session_id) {
                    // Refresh the MCP status cache at turn wrap-up (lazy connection happens inside the turn)
                    entry.mcp_status = session
                        .mcp
                        .as_ref()
                        .map(|mcp| mcp.statuses());
                    entry.session = Some(session);
                    entry.cancel = None;
                    entry.retry = None;
                    // After the turn ends (including abort/error), automatically pop the queue head and continue
                    if let Some((content, files, images, mode)) = entry.queue.pop_front()
                        && let Some(resolved) = resolve!(entry.model_override.as_ref(), entry.reasoning_level.as_deref()) {
                            start_turn(entry, session_id.clone(), content, files, images, mode, &resolved, &event_tx, &turns);
                        }
                }
            }
        }
    }
}
