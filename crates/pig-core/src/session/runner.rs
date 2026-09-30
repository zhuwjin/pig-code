use super::*;

struct SessionEntry {
    session: Option<Session>,
    /// 与 Session 共享 Arc 的工具状态：session 进入 turn future（session=None）时
    /// 仍可取待办/任务快照
    state: crate::task::SessionToolState,
    cancel: Option<CancellationToken>,
    model_override: Option<ModelSelection>,
    /// 会话级思考等级：独立于模型覆盖存在（无覆盖时作用于配置默认模型）
    reasoning_level: Option<String>,
    /// 最近一次用户消息/手动切换的执行模式：后台子代理完成的唤醒回合用它起 turn
    ///（不能用 default——会覆盖用户当前模式）
    last_mode: ExecMode,
    /// 回合进行中到达的消息在此排队（FIFO），回合结束自动接续
    queue: std::collections::VecDeque<(
        String,
        Vec<String>,
        Vec<pig_protocol::PendingImage>,
        ExecMode,
    )>,
    /// MCP 状态缓存：回合收尾/设置页查询时从 Session 的 manager 刷新
    ///（None = 尚未懒连接；回合进行中 Session 不在手边时按此缓存应答）
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
    entry.cancel = Some(cancel.clone());
    let tx = event_tx.clone();
    let config = config.clone();
    turns.push(Box::pin(async move {
        session
            .run_turn(content, files, images, &config, &tx, cancel)
            .await;
        (session_id, session)
    }));
}
/// agent manager：多会话并存，各会话独立 turn/cancel/审批表。
pub async fn agent_loop(
    op_rx: async_channel::Receiver<Op>,
    event_tx: async_channel::Sender<Event>,
    config_path: Option<PathBuf>,
    default_cwd: PathBuf,
    data_dir: PathBuf,
) {
    let config_path = config_path.unwrap_or_else(config::default_path);
    let mut load_error: Option<String> = None;
    let mut config: Option<AppConfig> = match config::load(&config_path) {
        Ok(config) => Some(config),
        Err(error) => {
            load_error = Some(error);
            None
        }
    };
    let sessions_dir = data_dir.join("sessions");
    let store = Arc::new(Mutex::new(
        Store::open(&data_dir).unwrap_or_else(|e| panic!("store 初始化失败: {e}")),
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
        emit_global!(Event::Error {
            session_id: None,
            seq,
            message: format!("配置解析失败: {error}。请检查或修复 ~/.pigcode/config.toml"),
        });
    }
    let mut sessions: HashMap<String, SessionEntry> = HashMap::new();
    // 已删除会话：在飞回合收尾（Session drop → 句柄关闭）后补删 rollout 文件
    let mut deleted_sessions: HashSet<String> = HashSet::new();
    let mut turns: FuturesUnordered<TurnFuture> = FuturesUnordered::new();
    let mut id_counter = 0u64;
    // 后台任务完成通知：watcher 发 session_id → select 分支推 TaskListChanged
    let (task_notify_tx, mut task_notify_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    // 后台子代理完成唤醒：(session_id, 通知文本) → 合成 user 消息起新回合
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
                .unwrap_or_else(|| ("未配置模型".to_string(), String::new()))
        };
    }
    loop {
        tokio::select! {
            op = op_rx.recv() => {
                let Ok(op) = op else { break };
                match op {
                    Op::NewSession { cwd, provider_id, model_id, reasoning_level, exec_mode } => {
                        let cwd = normalize_workspace_path(&cwd);
                        id_counter += 1;
                        let id = format!("s{}-{}", now_secs(), id_counter);
                        // 新会话默认值 = 工作区最近活跃会话；UI 显式传入的字段优先于种子
                        let seed = store
                            .lock()
                            .expect("store lock")
                            .latest_active_in_workspace(&cwd);
                        let mut meta = SessionMeta {
                            id: id.clone(),
                            title: "新任务".to_string(),
                            title_custom: false,
                            cwd,
                            created_at: now_secs(),
                            updated_at: now_secs(),
                            pinned: false,
                            archived: false,
                            provider_id: provider_id.or_else(|| seed.as_ref().and_then(|m| m.provider_id.clone())),
                            model_id: model_id.or_else(|| seed.as_ref().and_then(|m| m.model_id.clone())),
                            // 思考等级按 UI 原样（hero 默认值已把种子下达到 UI；None = 关）
                            reasoning_level,
                            exec_mode: exec_mode.unwrap_or_else(|| {
                                seed.as_ref().map(|m| m.exec_mode).unwrap_or_default()
                            }),
                            // 区外读写开关随工作区种子继承（与 exec_mode 同口径）
                            fs_read_outside: seed.as_ref().map(|m| m.fs_read_outside).unwrap_or(false),
                            fs_write_outside: seed.as_ref().map(|m| m.fs_write_outside).unwrap_or(false),
                        };
                        // UI 未指定思考等级且模型配置了默认等级 → 采用默认档
                        //（写进 meta，SessionConfigured 会同步回 UI 的等级 chip）
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
                                session.set_fs_access(meta.fs_read_outside, meta.fs_write_outside);
                                let selection = meta_to_selection(&meta);
                                let state = session.state.clone();
                                sessions.insert(id.clone(), SessionEntry { session: Some(session), state, cancel: None, model_override: selection.clone(), reasoning_level: meta.reasoning_level.clone(), last_mode: meta.exec_mode, queue: Default::default(), mcp_status: None });
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
                                    fs_read_outside: meta.fs_read_outside,
                                    fs_write_outside: meta.fs_write_outside,
                                });
                                // 新会话面板初始化为空快照
                                emit_global!(Event::TodoListChanged { session_id: id.clone(), seq, items: vec![] });
                                emit_global!(Event::TaskListChanged { session_id: id.clone(), seq, tasks: vec![] });
                                emit_global!(Event::SessionList {
                                    sessions: store.lock().expect("store lock").sorted_sessions(),
                                });
                                // 已移除（隐藏）的工作区下新建会话：自动恢复显示
                                if store.lock().expect("store lock").unhide_workspace(&meta.cwd) {
                                    emit_global!(Event::WorkspaceList {
                                        workspaces: store.lock().expect("store lock").workspaces(),
                                    });
                                }
                            }
                            Err(error) => emit_global!(Event::Error {
                                session_id: None,
                                seq,
                                message: error,
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
                                    fs_read_outside: meta.fs_read_outside,
                                    fs_write_outside: meta.fs_write_outside,
                                });
                                // 切回已打开会话：补发面板快照，UI 重置面板
                                if let Some(entry) = sessions.get(&session_id) {
                                    let items = entry.state.todos.lock().expect("todos lock").clone();
                                    let tasks = crate::task::snapshot(&entry.state.tasks);
                                    emit_global!(Event::TodoListChanged { session_id: session_id.clone(), seq, items });
                                    emit_global!(Event::TaskListChanged { session_id: session_id.clone(), seq, tasks });
                                    // 补发水位/累计（composer 的容量 chip 换会话后仍是旧值）
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
                                // 恢复持久化的模式/模型覆盖（meta 由 Set* 写穿保持最新）
                                let meta = store.lock().expect("store lock").get_session(&session_id);
                                let selection = meta.as_ref().and_then(meta_to_selection);
                                if let Some(meta) = &meta {
                                    session.set_mode(meta.exec_mode);
                                    session.set_fs_access(meta.fs_read_outside, meta.fs_write_outside);
                                }
                                let cwd = session.cwd.clone();
                                let state = session.state.clone();
                                sessions.insert(session_id.clone(), SessionEntry {
                                    session: Some(session),
                                    state,
                                    cancel: None,
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
                                    fs_read_outside: meta.as_ref().map(|m| m.fs_read_outside).unwrap_or(false),
                                    fs_write_outside: meta.as_ref().map(|m| m.fs_write_outside).unwrap_or(false),
                                });
                                // 重新打开的会话无持久化面板状态：空快照重置
                                emit_global!(Event::TodoListChanged { session_id: session_id.clone(), seq, items: vec![] });
                                emit_global!(Event::TaskListChanged { session_id: session_id.clone(), seq, tasks: vec![] });
                                if let Some(entry) = sessions.get_mut(&session_id)
                                    && let Some(session) = entry.session.as_mut() {
                                        session.replay(&records, &event_tx);
                                    }
                                // 回放已恢复水位与累计：补发上下文容量，重开 app 不必
                                // 等下一条消息即显示（模型未配置则跳过，无窗口可报）
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
                                message: error,
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
                                // 手动重命名：自动命名此后不再覆盖
                                meta.title_custom = true;
                            }
                            meta.updated_at = now_secs();
                        });
                        emit_global!(Event::SessionList {
                            sessions: store.lock().expect("store lock").sorted_sessions(),
                        });
                    }
                    Op::DeleteSession { session_id } => {
                        // 在飞回合：先取消；Session 还在 turn future 里，
                        // rollout 句柄要等回合收尾 drop 后才能删文件
                        let turn_in_flight = sessions
                            .get(&session_id)
                            .is_some_and(|e| e.session.is_none());
                        if let Some(entry) = sessions.remove(&session_id)
                            && let Some(cancel) = &entry.cancel
                        {
                            cancel.cancel();
                        }
                        // idle：Session 随 entry 移除而 drop，句柄已关
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
                                message: "会话不存在，请先新建或打开".to_string(),
                            });
                            continue;
                        };
                        // 记录最近模式：唤醒回合按它起 turn（含排队情形）
                        entry.last_mode = mode;
                        // 回合进行中 → 排队，回合结束自动接续（Interrupt 不清队列）
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
                                message: "未配置模型，请在 设置 → 模型设置 中添加供应商和模型".to_string(),
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
                                        message: error,
                                    }).await;
                                }
                                Err(e) => {
                                    let _ = tx.send(Event::Error {
                                        session_id: None,
                                        seq: 0,
                                        message: format!("git 任务失败: {e}"),
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
                            if let Ok(diff) = result {
                                let _ = tx
                                    .send(Event::GitDiff {
                                        cwd,
                                        path,
                                        staged,
                                        diff,
                                    })
                                    .await;
                            }
                        });
                    }
                    Op::LoadSubagent { session_id, agent_id } => {
                        // 无需 Session 实例：直接读子代理上下文 JSONL（右侧 tab 只读展示）
                        let tx = event_tx.clone();
                        let path = crate::agent::agents_dir(&sessions_dir, &session_id)
                            .join(format!("{agent_id}.jsonl"));
                        // 「运行中」口径：会话任务注册表里同 agent_id 且 Running；
                        // 会话不在内存（未加载）→ false
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
                                            message: format!("读取子代理记录失败: {error}"),
                                        })
                                        .await;
                                }
                                Err(e) => {
                                    let _ = tx
                                        .send(Event::Error {
                                            session_id: Some(session_id),
                                            seq: 0,
                                            message: format!("读取子代理记录任务失败: {e}"),
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
                    Op::ApprovalReply { request_id, decision } => {
                        // 决议同时唤醒同合并键的并发等待者（见 resolve_approval）
                        super::resolve_approval(&pending, &request_id, decision);
                    }
                    Op::QuestionReply { request_id, answers } => {
                        if let Some(reply) = pending_questions.lock().expect("pending questions lock").remove(&request_id) {
                            let _ = reply.send(answers);
                        }
                    }
                    Op::SetExecMode { session_id, mode } => {
                        if let Some(entry) = sessions.get_mut(&session_id) {
                            // 手动切模式：唤醒回合跟随最近模式
                            entry.last_mode = mode;
                            if let Some(session) = entry.session.as_mut() {
                                session.set_mode(mode);
                                // 手动切模式：EnterPlanMode 的记忆作废（之后再
                                // ExitPlanMode 回落到默认「变更前确认」）
                                session.pre_plan_mode = None;
                            }
                            store.lock().expect("store lock").update_session(&session_id, |m| {
                                m.exec_mode = mode;
                            });
                        }
                    }
                    Op::SetFsAccess { session_id, read_outside, write_outside } => {
                        if let Some(entry) = sessions.get_mut(&session_id) {
                            // state 是共享句柄：回合进行中（session=None）同样生效
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
                                message: "回合进行中，无法撤销文件".to_string(),
                            }),
                            None => emit_global!(Event::Error {
                                session_id: Some(session_id),
                                seq,
                                message: "会话不存在".to_string(),
                            }),
                        }
                    }
                    Op::SearchFiles { session_id, query } => {
                        let cwd = sessions.get(&session_id)
                            .and_then(|e| e.session.as_ref().map(|s| s.cwd.clone()))
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
                        // 缓存命中直接回；未命中（新模型 ID）且缓存不新鲜才重拉，
                        // 避免用户在对话框试错 ID 时连打 models.dev
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
                                    // 命中：无需网络
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
                    Op::Compact { session_id } => {
                        match sessions.get_mut(&session_id) {
                            Some(entry) if entry.session.is_some() => {
                                let mut session = entry.session.take().expect("session present");
                                let tx = event_tx.clone();
                                let sid = session_id.clone();
                                let resolved = resolve!(entry.model_override.as_ref(), entry.reasoning_level.as_deref());
                                entry.cancel = Some(CancellationToken::new());
                                let cancel = entry.cancel.clone().expect("cancel");
                                turns.push(Box::pin(async move {
                                    session.run_compact(resolved.as_ref(), false, &tx, &cancel).await;
                                    (sid, session)
                                }));
                            }
                            _ => emit_global!(Event::Error {
                                session_id: Some(session_id),
                                seq,
                                message: "回合进行中或会话不存在，无法压缩".to_string(),
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
                            // 写穿 sessions 表：重开/新建继承都从这里取
                            store.lock().expect("store lock").update_session(&session_id, |m| {
                                m.provider_id = Some(provider_id.clone());
                                m.model_id = Some(model_id.clone());
                                m.reasoning_level = reasoning_level.clone();
                            });
                        }
                    }
                    Op::SetReasoning { session_id, reasoning_level } => {
                        if let Some(entry) = sessions.get_mut(&session_id) {
                            // 独立于模型覆盖保存：无覆盖时作用于配置默认模型（resolve 的 default_level）
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
                                message: error,
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
                            let result = match found {
                                Some(provider) => {
                                    let model = provider
                                        .models
                                        .first()
                                        .map(|m| m.id.clone())
                                        .unwrap_or_else(|| "ping".to_string());
                                    provider::test_provider(
                                        &provider.base_url,
                                        &config::expand_env(&provider.api_key),
                                        provider.api_format,
                                        &model,
                                    )
                                    .await
                                }
                                None => Err("供应商不存在".to_string()),
                            };
                            let (ok, message) = match result {
                                Ok(message) => (true, message),
                                Err(message) => (false, message),
                            };
                            let _ = tx.send(Event::TestResult {
                                provider_id,
                                ok,
                                message,
                            }).await;
                        });
                    }
                    Op::ListMcpServers { session_id } => {
                        let servers = match sessions.get_mut(&session_id) {
                            Some(entry) => {
                                // 回合进行中（session=None）读不到 manager：回缓存清单
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
                }
            }
            Some(session_id) = task_notify_rx.recv() => {
                // 后台任务状态变化：推面板快照（entry.state 与 Session 共享 Arc，
                // session 在 turn future 中也能取到注册表）
                if let Some(entry) = sessions.get(&session_id) {
                    let tasks = crate::task::snapshot(&entry.state.tasks);
                    emit_global!(Event::TaskListChanged { session_id, seq, tasks });
                }
            }
            Some((session_id, content)) = wake_rx.recv() => {
                // 后台子代理完成唤醒：合成 user 消息起新回合。
                // 忙/闲判定对齐 SendMessage：忙则排队（回合结束自动接续）；
                // 模式用 last_mode（最近一次用户模式，不回落 default）
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
                    // 已删除会话的回合收尾：Session 在此 drop，句柄关闭后补删文件
                    let _ = std::fs::remove_file(sessions_dir.join(format!("{session_id}.jsonl")));
                    continue;
                }
                if let Some(entry) = sessions.get_mut(&session_id) {
                    // 回合收尾刷新 MCP 状态缓存（懒连接发生在回合内）
                    entry.mcp_status = session
                        .mcp
                        .as_ref()
                        .map(|mcp| mcp.statuses());
                    entry.session = Some(session);
                    entry.cancel = None;
                    // 回合结束（含中止/出错）后自动取出队首继续
                    if let Some((content, files, images, mode)) = entry.queue.pop_front()
                        && let Some(resolved) = resolve!(entry.model_override.as_ref(), entry.reasoning_level.as_deref()) {
                            start_turn(entry, session_id.clone(), content, files, images, mode, &resolved, &event_tx, &turns);
                        }
                }
            }
        }
    }
}
