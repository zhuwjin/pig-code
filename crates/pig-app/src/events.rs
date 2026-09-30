use super::*;

impl AppView {
    pub(crate) fn route_event(&mut self, event: Event, cx: &mut Context<Self>) {
        // 已删除会话的迟到事件（删除前已入队的流式/收尾事件）直接丢弃，
        // 防止 ensure_views 给已删会话重建僵尸视图
        if let Some(sid) = event_session_id(&event)
            && self.deleted_sessions.contains(&sid)
        {
            return;
        }
        match &event {
            Event::SessionConfigured {
                session_id,
                cwd,
                model,
                provider_name,
                provider_id,
                model_id,
                reasoning_level,
                exec_mode,
                fs_read_outside,
                fs_write_outside,
            } => {
                let session_id = session_id.clone();
                eprintln!(
                    "[model] SessionConfigured {session_id} → provider={provider_id:?} model={model_id:?} 思考={reasoning_level:?}"
                );
                self.ensure_views(&session_id, cx);
                self.current = Some(session_id.clone());
                // 换了会话：先清掉上一个会话的上下文水位（有数据的会话随后会收到补发）
                self.composer.update(cx, |composer, cx| {
                    composer.clear_context_usage(cx);
                });
                let label = if provider_name.is_empty() {
                    model.clone()
                } else {
                    format!("{provider_name}/{model}")
                };
                // 恢复会话的模型/模式/思考等级（core 持久化在 sessions 表）
                self.exec_mode = *exec_mode;
                self.reasoning_level = reasoning_level.clone();
                self.current_model = match (provider_id, model_id) {
                    (Some(p), Some(m)) => Some((p.clone(), m.clone())),
                    _ => None,
                };
                self.composer.update(cx, |composer, cx| {
                    composer.set_model_name(label, cx);
                    composer.set_hero_mode(false, cx);
                    composer.set_exec_mode(*exec_mode, cx);
                    composer.set_reasoning_level(reasoning_level.clone(), cx);
                    composer.set_fs_access(*fs_read_outside, *fs_write_outside, cx);
                });
                // 切换/新建会话：用缓存快照同步进度/任务面板（无快照则清空）
                let todos = self
                    .todos_by_session
                    .get(&session_id)
                    .cloned()
                    .unwrap_or_default();
                let tasks = self
                    .tasks_by_session
                    .get(&session_id)
                    .cloned()
                    .unwrap_or_default();
                self.composer.update(cx, |composer, cx| {
                    composer.set_todos(todos, cx);
                    composer.set_tasks(tasks, cx);
                });
                if let Some((text, files, images, mode)) = self.pending_first_send.take() {
                    self.agent
                        .send_message(session_id, text, files, images, mode);
                }
                // 切换/新建会话：标题栏分支跟随会话 cwd；拉工作区 git 状态（Review 面板）
                self.refresh_git_branch(Some(cwd.clone()), cx);
                self.agent.git_status(cwd.clone());
            }
            Event::ModelInfo { id, info } => {
                // 回填 InputState::set_value 需要 window，经 AnyWindowHandle 进入窗口上下文
                let (id, info) = (id.clone(), info.clone());
                let settings = self.settings.clone();
                if let Some(handle) = cx.windows().into_iter().next() {
                    let _ = handle.update(cx, |_, window, cx| {
                        settings.update(cx, |settings, cx| {
                            settings.apply_model_info(&id, info, window, cx);
                        });
                    });
                }
            }
            Event::ConfigSnapshot { config } => {
                self.config = Some(config.clone());
                let config = config.clone();
                self.settings
                    .update(cx, |settings, cx| settings.set_config(config, cx));
                self.apply_config_to_composer(cx);
            }
            Event::TestResult {
                provider_id,
                ok,
                message,
            } => {
                self.settings.update(cx, |settings, cx| {
                    settings.set_test_result(provider_id, *ok, message.clone(), cx);
                });
            }
            Event::McpServerList {
                session_id,
                connected,
            } => {
                // 设置页 MCP 连接状态回包：只采纳当前活动会话的应答
                if self.current.as_deref() == Some(session_id.as_str()) {
                    let connected = connected.clone();
                    self.settings.update(cx, |settings, cx| {
                        settings.set_mcp_connected(connected, cx);
                    });
                }
            }
            Event::WorkspaceList { workspaces } => {
                self.workspaces = workspaces
                    .iter()
                    .filter(|p| !p.hidden)
                    .map(|p| p.path.display().to_string())
                    .collect();
                self.hidden_workspaces = workspaces
                    .iter()
                    .filter(|p| p.hidden)
                    .map(|p| p.path.display().to_string())
                    .collect();
                self.workspace_aliases = workspaces
                    .iter()
                    .filter_map(|p| {
                        p.alias
                            .clone()
                            .map(|alias| (p.path.display().to_string(), alias))
                    })
                    .collect();
            }
            Event::SessionList { sessions } => {
                self.metas = sessions.clone();
                if self.current.is_none() && !self.metas.is_empty() {
                    if let Some(meta) = self.metas.iter().find(|s| !s.archived) {
                        self.agent.open_session(meta.id.clone());
                    }
                }
                self.push_hero_info(cx);
            }
            Event::SessionTitleChanged { session_id, title } => {
                // 自动命名 sidecar 完成：只补丁标题，不重发整个列表
                if let Some(meta) = self.metas.iter_mut().find(|m| &m.id == session_id) {
                    meta.title = title.clone();
                }
                self.refresh_sidebar(cx);
            }
            Event::ExecModeChanged {
                session_id, mode, ..
            } => {
                // core 侧主动切了模式（ExitPlanMode 确认后）：chip/缓存同步
                self.exec_mode = *mode;
                if let Some(meta) = self.metas.iter_mut().find(|m| &m.id == session_id) {
                    meta.exec_mode = *mode;
                }
                let mode = *mode;
                self.composer.update(cx, |composer, cx| {
                    composer.set_exec_mode(mode, cx);
                });
                cx.notify();
            }
            Event::Error {
                session_id: None,
                message,
                ..
            } => {
                let message = message.clone();
                if self.is_hero(cx) {
                    self.hero_error = Some(message);
                } else if let Some(sid) = self.current.clone() {
                    self.ensure_views(&sid, cx);
                    self.views[&sid].thread.update(cx, |thread, cx| {
                        thread.add_system_note(&format!("⚠ {message}"), cx);
                    });
                }
            }
            Event::FileSearchResults { query, results, .. } => {
                let results = results.clone();
                let _ = query;
                self.composer
                    .update(cx, |composer, cx| composer.set_mention_results(results, cx));
            }
            Event::GitInfo {
                cwd,
                current_branch,
                branches,
            } => {
                if self.hero_cwd.as_ref() == Some(cwd) {
                    self.hero_branch = current_branch.clone();
                    self.hero_branches = branches.clone();
                    self.hero_is_git = current_branch.is_some();
                    self.push_hero_info(cx);
                }
            }
            Event::BranchChanged { cwd, branch } => {
                if self.hero_cwd.as_ref() == Some(cwd) {
                    self.hero_branch = Some(branch.clone());
                    self.agent.git_info(cwd.clone());
                }
                // 标题栏分支切换器：当前会话目录切了分支 → 更新 chip + 重拉分支列表，
                // 并刷新工作区 git 状态（切分支后改动面板内容全变）
                if self.current_cwd().as_ref() == Some(cwd) {
                    self.git_branch = Some(branch.clone());
                    self.refresh_git_branch(Some(cwd.clone()), cx);
                    self.agent.git_status(cwd.clone());
                }
            }
            Event::GitStatus {
                cwd,
                is_git,
                unstaged,
                staged,
            } => {
                // 工作区口径：同 cwd 的所有会话面板都更新
                let (is_git, unstaged, staged) = (*is_git, unstaged.clone(), staged.clone());
                let sids: Vec<String> = self
                    .metas
                    .iter()
                    .filter(|m| m.cwd == *cwd)
                    .map(|m| m.id.clone())
                    .collect();
                for sid in &sids {
                    if let Some(views) = self.views.get(sid) {
                        let (unstaged, staged) = (unstaged.clone(), staged.clone());
                        views.review.update(cx, |review, cx| {
                            review.set_git_status(is_git, unstaged, staged, cx);
                        });
                    }
                }
                // 输入框上方的改动 chip：git 口径（未暂存 + 已暂存合并统计）
                if self.current.as_ref().is_some_and(|cur| sids.contains(cur)) {
                    let (adds, dels) = unstaged
                        .iter()
                        .chain(staged.iter())
                        .fold((0u32, 0u32), |(a, d), e| (a + e.additions, d + e.deletions));
                    let files: Vec<(String, u32, u32)> = unstaged
                        .iter()
                        .chain(staged.iter())
                        .map(|e| (e.path.clone(), e.additions, e.deletions))
                        .collect();
                    self.composer.update(cx, |composer, cx| {
                        composer.set_changes(adds, dels, files, cx);
                    });
                }
            }
            Event::GitDiff {
                cwd, path, diff, ..
            } => {
                let (path, diff) = (path.clone(), diff.clone());
                let sids: Vec<String> = self
                    .metas
                    .iter()
                    .filter(|m| m.cwd == *cwd)
                    .map(|m| m.id.clone())
                    .collect();
                for sid in sids {
                    if let Some(views) = self.views.get(&sid) {
                        let (path, diff) = (path.clone(), diff.clone());
                        views.review.update(cx, |review, cx| {
                            review.set_git_diff(path, diff, cx);
                        });
                    }
                }
            }
            Event::ContextUsage {
                session_id,
                used,
                total,
                cache_read_total,
                input_total,
                ..
            } => {
                if self.current.as_deref() == Some(session_id.as_str()) {
                    let (used, total, cache_read_total, input_total) =
                        (*used, *total, *cache_read_total, *input_total);
                    self.composer.update(cx, |composer, cx| {
                        composer.set_context_usage(used, total, cache_read_total, input_total, cx);
                    });
                }
            }
            Event::TodoListChanged {
                session_id, items, ..
            } => {
                self.todos_by_session
                    .insert(session_id.clone(), items.clone());
                if self.current.as_deref() == Some(session_id.as_str()) {
                    let items = items.clone();
                    self.composer
                        .update(cx, |composer, cx| composer.set_todos(items, cx));
                }
            }
            Event::TaskListChanged {
                session_id, tasks, ..
            } => {
                self.tasks_by_session
                    .insert(session_id.clone(), tasks.clone());
                if self.current.as_deref() == Some(session_id.as_str()) {
                    let tasks = tasks.clone();
                    self.composer
                        .update(cx, |composer, cx| composer.set_tasks(tasks, cx));
                }
            }
            Event::TurnComplete {
                session_id,
                duration_ms,
                ..
            } => {
                // duration_ms=0 是会话回放，不触发计划模式待执行标记
                if *duration_ms > 0 && self.exec_mode == pig_protocol::ExecMode::Plan {
                    let session_id = session_id.clone();
                    if let Some(views) = self.views.get(&session_id) {
                        views.thread.update(cx, |thread, cx| {
                            thread.set_plan_pending(true, cx);
                        });
                    }
                }
                self.refresh_git_branch(self.current_cwd(), cx);
                // 回合结束（agent 写文件已落定）：刷新工作区 git 状态
                if let Some(meta) = self.metas.iter().find(|m| &m.id == session_id) {
                    self.agent.git_status(meta.cwd.clone());
                }
            }
            Event::ContextCompacted {
                session_id, note, ..
            } => {
                let session_id = session_id.clone();
                self.ensure_views(&session_id, cx);
                let note = note.clone();
                self.views[&session_id].thread.update(cx, |thread, cx| {
                    thread.add_system_note(&note, cx);
                });
            }
            Event::SubagentHistory {
                session_id,
                agent_id,
                title,
                subtitle,
                items,
                running,
                ..
            } => {
                // 右侧「子代理」tab 的内容到达：只更新已开的 tab；
                // 会话归属不符（迟到/串会话的事件）忽略
                if let Some(panel) = self.subagent_tabs.get(agent_id)
                    && panel.read(cx).matches_session(session_id)
                {
                    let running = *running;
                    panel.update(cx, |panel, cx| {
                        panel.set_history(
                            title.clone(),
                            subtitle.clone(),
                            items.clone(),
                            running,
                            cx,
                        );
                    });
                }
            }
            Event::SubagentActivity {
                session_id,
                agent_id,
                item,
                finished,
                ..
            } => {
                // 子代理实时增量：追加展示项 / 收尾关「运行中」。
                // finished 后全量重拉一次收口——增量追加与初次加载分属不同任务
                //（同一事件通道 FIFO，但读文件与驱动并发），窄竞态以全量覆盖自愈
                if let Some(panel) = self.subagent_tabs.get(agent_id)
                    && panel.read(cx).matches_session(session_id)
                {
                    let item = item.clone();
                    let finished = *finished;
                    panel.update(cx, |panel, cx| {
                        if let Some(item) = item {
                            panel.push_item(item, cx);
                        }
                        if finished {
                            panel.set_finished(cx);
                        }
                    });
                    if finished {
                        self.agent
                            .load_subagent(session_id.clone(), agent_id.clone());
                    }
                }
            }
            _ => {}
        }

        // 运行状态跟踪（侧栏状态点 + 当前会话的 composer 状态）
        let sid = event_session_id(&event);
        if let Some(sid) = &sid {
            match &event {
                Event::TurnStarted { .. } => {
                    self.running.insert(sid.clone());
                }
                Event::TurnComplete { .. } | Event::TurnAborted { .. } => {
                    self.running.remove(sid);
                    self.approval_pending.remove(sid);
                    self.pending_approvals.remove(sid);
                    self.pending_questions.remove(sid);
                }
                Event::ApprovalRequested { tool, detail, .. } => {
                    self.approval_pending.insert(sid.clone());
                    let cwd = self
                        .metas
                        .iter()
                        .find(|m| &m.id == sid)
                        .map(|m| m.cwd.display().to_string())
                        .unwrap_or_default();
                    self.pending_approvals.insert(
                        sid.clone(),
                        PendingApproval {
                            tool: tool.clone(),
                            detail: detail.clone(),
                            cwd,
                        },
                    );
                }
                Event::QuestionRequested {
                    request_id,
                    questions,
                    ..
                } => {
                    self.pending_questions.insert(
                        sid.clone(),
                        PendingQuestion {
                            request_id: request_id.clone(),
                            questions: questions.clone(),
                        },
                    );
                }
                Event::Error { .. } => {
                    self.running.remove(sid);
                    self.approval_pending.remove(sid);
                    self.pending_approvals.remove(sid);
                    self.pending_questions.remove(sid);
                }
                _ => {}
            }
        }

        // 路由到 per-session 视图
        if let Some(sid) = &sid {
            self.ensure_views(sid, cx);
            if let Some(views) = self.views.get(sid) {
                match &event {
                    Event::FileChanged {
                        path,
                        unified_diff,
                        additions,
                        deletions,
                        ..
                    } => {
                        let (path, diff, adds, dels) =
                            (path.clone(), unified_diff.clone(), *additions, *deletions);
                        views.review.update(cx, |review, cx| {
                            review.upsert(path, diff, adds, dels, cx);
                        });
                    }
                    Event::FileReverted { path, .. } => {
                        let path = path.clone();
                        views.review.update(cx, |review, cx| {
                            review.remove(&path, cx);
                        });
                        // 撤销改变了工作区内容：刷新 git 状态
                        if let Some(meta) = self.metas.iter().find(|m| &m.id == sid) {
                            self.agent.git_status(meta.cwd.clone());
                        }
                    }
                    _ => {
                        views.thread.update(cx, |thread, cx| {
                            thread.reduce_event(event.clone(), cx);
                        });
                    }
                }
            }
            if self.current.as_ref() == Some(sid) {
                self.sync_composer_state(cx);
            }
        }
        self.refresh_sidebar(cx);
        self.sync_hero_mode(cx);
        cx.notify();
    }

    pub(crate) fn on_composer_event(
        &mut self,
        event: &ComposerEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            ComposerEvent::Send {
                text,
                files,
                images,
                mode,
            } => {
                if self.is_hero(cx) {
                    self.hero_send(text.clone(), files.clone(), images.clone(), *mode, cx);
                    return;
                }
                let Some(sid) = self.current.clone() else {
                    return;
                };
                // 用户消息由 core 的 Event::UserMessage 统一上屏（含排队出队路径）
                self.agent
                    .send_message(sid, text.clone(), files.clone(), images.clone(), *mode);
            }
            ComposerEvent::PickDirectory => self.pick_directory(window, cx),
            ComposerEvent::SelectCwd(cwd) => {
                let cwd = PathBuf::from(cwd);
                self.hero_error = None;
                self.agent.git_info(cwd.clone());
                self.hero_cwd = Some(cwd);
                self.push_hero_info(cx);
                // 换了工作区：按新工作区最近活跃会话重铺默认值
                self.apply_hero_defaults(cx);
            }
            ComposerEvent::ClearCwd => {
                self.hero_cwd = None;
                self.hero_error = None;
                self.hero_branch = None;
                self.hero_branches = vec![];
                self.hero_is_git = false;
                self.push_hero_info(cx);
                cx.notify();
            }
            ComposerEvent::CheckoutBranch(branch) => {
                if let Some(cwd) = self.hero_cwd.clone() {
                    self.agent.checkout_branch(cwd, branch.clone());
                }
            }
            ComposerEvent::Stop => {
                if let Some(sid) = &self.current {
                    self.agent.interrupt(sid.clone());
                }
            }
            ComposerEvent::Clear => {
                if let Some(sid) = &self.current {
                    if let Some(views) = self.views.get(sid) {
                        views.thread.update(cx, |thread, cx| thread.clear(cx));
                    }
                }
            }
            ComposerEvent::Compact => {
                if let Some(sid) = &self.current {
                    self.agent.compact(sid.clone());
                }
            }
            ComposerEvent::SetModel {
                provider_id,
                model_id,
            } => {
                eprintln!(
                    "[model] 收到切换 → {provider_id}/{model_id}（hero={}，原思考={:?}）",
                    self.current.is_none(),
                    self.reasoning_level
                );
                self.current_model = Some((provider_id.clone(), model_id.clone()));
                if self.current.is_none() {
                    // hero 态的显式选择：此后的工作区种子默认值不再覆盖模型
                    self.hero_model_dirty = true;
                }
                // 切模型的思考等级落点（优先级从高到低）：
                // 1. 新模型配置的默认档
                // 2. 继承上个模型的等级（需新模型支持；None=关原样继承）
                // 3. 继承档不被支持（Some 但不在表内）→ 启发式兜底（high → 首个非关档）
                let new_levels = self.model_reasoning_levels(provider_id, model_id);
                let target = self
                    .model_default_reasoning_level(provider_id, model_id)
                    .or_else(|| {
                        self.reasoning_level
                            .clone()
                            .filter(|lv| new_levels.contains(lv))
                    })
                    .or_else(|| {
                        self.reasoning_level
                            .is_some()
                            .then(|| Self::fallback_reasoning_level(&new_levels))
                            .flatten()
                    });
                if self.reasoning_level != target {
                    eprintln!(
                        "[model] 切换 {model_id}：思考等级 {:?} → {:?}",
                        self.reasoning_level, target
                    );
                    self.reasoning_level = target;
                    let level = self.reasoning_level.clone();
                    self.composer.update(cx, |composer, cx| {
                        composer.set_reasoning_level(level, cx);
                    });
                }
                let reasoning = self.reasoning_level.clone();
                self.update_current_meta(|m| {
                    m.provider_id = Some(provider_id.clone());
                    m.model_id = Some(model_id.clone());
                    m.reasoning_level = reasoning.clone();
                });
                if let Some(sid) = &self.current {
                    self.agent.set_model(
                        sid.clone(),
                        provider_id.clone(),
                        model_id.clone(),
                        self.reasoning_level.clone(),
                    );
                }
            }
            ComposerEvent::SetReasoning(level) => {
                self.reasoning_level = level.clone();
                // 思考等级独立于模型选择：未显式选模型时同样写穿并生效（作用于默认模型）
                self.update_current_meta(|m| {
                    m.reasoning_level = level.clone();
                });
                if let Some(sid) = &self.current {
                    self.agent.set_reasoning(sid.clone(), level.clone());
                }
            }
            ComposerEvent::SetExecMode(mode) => {
                self.apply_exec_mode(*mode, cx);
            }
            ComposerEvent::RequestYoloConfirm => {
                self.yolo_confirm_open = true;
                self.yolo_confirm_focus.focus(window, cx);
                cx.notify();
            }
            ComposerEvent::SetFsAccess {
                read_outside,
                write_outside,
            } => {
                self.update_current_meta(|m| {
                    m.fs_read_outside = *read_outside;
                    m.fs_write_outside = *write_outside;
                });
                if let Some(sid) = &self.current {
                    self.agent
                        .set_fs_access(sid.clone(), *read_outside, *write_outside);
                }
            }
            ComposerEvent::OpenSettings => self.open_settings(cx),
            ComposerEvent::DecideApproval(decision) => {
                // 走 ThreadView::decide_pending 单一路径：更新线程内审批卡状态并
                // 经 ThreadEvent::ApprovalReply 回复 core（那里同时清掉待审批态）
                if let Some(sid) = &self.current
                    && let Some(views) = self.views.get(sid)
                {
                    let decision = *decision;
                    views.thread.update(cx, |thread, cx| {
                        thread.decide_pending(decision, cx);
                    });
                }
            }
            ComposerEvent::QuestionReply {
                request_id,
                answers,
            } => {
                // 提交/跳过后立即撤掉问题条（不等回合结束），恢复输入框
                if let Some(sid) = &self.current {
                    self.pending_questions.remove(sid);
                }
                self.agent
                    .question_reply(request_id.clone(), answers.clone());
                self.sync_composer_state(cx);
            }
            ComposerEvent::SearchFiles(query) => {
                if let Some(sid) = &self.current {
                    self.agent.search_files(sid.clone(), query.clone());
                }
            }
            ComposerEvent::OpenChanges => {
                self.open_right_tab(RightTab::Changes, cx);
            }
            ComposerEvent::OpenSubagent { agent_id, title } => {
                // 「后台 Agent」弹层的任务行点击：开右侧子代理对话 tab
                //（弹层已在 composer 侧收起；同 agent_id 聚焦不重复加载）
                if let Some(sid) = self.current.clone() {
                    self.open_subagent_tab(sid, agent_id.clone(), title.clone(), cx);
                }
            }
        }
    }

    pub(crate) fn on_sidebar_event(
        &mut self,
        event: &SidebarEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            SidebarEvent::Select(id) => self.switch_session(id.clone(), cx),
            SidebarEvent::NewTask => {
                self.hero_cwd = None;
                self.enter_hero(cx);
            }
            SidebarEvent::NewTaskInWorkspace(path) => {
                self.hero_cwd = Some(PathBuf::from(path));
                self.enter_hero(cx);
            }
            SidebarEvent::SetPinned(id, pinned) => self.agent.set_pinned(id, *pinned),
            SidebarEvent::SetArchived(id, archived) => self.agent.set_archived(id, *archived),
            SidebarEvent::RenameSession(id, title) => self.rename_session(id, title, cx),
            SidebarEvent::DeleteSession(id) => self.delete_session(id, cx),
            SidebarEvent::RemoveWorkspace(path) => {
                self.agent.remove_workspace(PathBuf::from(path));
            }
            SidebarEvent::RenameWorkspace(path, alias) => {
                self.agent
                    .rename_workspace(PathBuf::from(path), alias.clone());
            }
            SidebarEvent::OpenSettings => self.open_settings(cx),
        }
    }
}

pub(crate) fn event_session_id(event: &Event) -> Option<String> {
    match event {
        Event::SessionConfigured { session_id, .. }
        | Event::UserMessage { session_id, .. }
        | Event::TurnStarted { session_id, .. }
        | Event::ReasoningDelta { session_id, .. }
        | Event::TextDelta { session_id, .. }
        | Event::TextDone { session_id, .. }
        | Event::ToolCallBegin { session_id, .. }
        | Event::ToolCallEnd { session_id, .. }
        | Event::ContextUsage { session_id, .. }
        | Event::ContextCompacted { session_id, .. }
        | Event::ApprovalRequested { session_id, .. }
        | Event::QuestionRequested { session_id, .. }
        | Event::FileChanged { session_id, .. }
        | Event::FileReverted { session_id, .. }
        | Event::TurnComplete { session_id, .. }
        | Event::TurnAborted { session_id, .. }
        | Event::TurnFileChanges { session_id, .. }
        | Event::MessageQueued { session_id, .. }
        | Event::TodoListChanged { session_id, .. }
        | Event::TaskListChanged { session_id, .. }
        | Event::SubagentProgress { session_id, .. }
        | Event::SubagentCard { session_id, .. }
        | Event::SubagentHistory { session_id, .. }
        | Event::SubagentActivity { session_id, .. }
        | Event::ExecModeChanged { session_id, .. }
        | Event::FileSearchResults { session_id, .. } => Some(session_id.clone()),
        Event::SessionList { .. }
        | Event::SessionTitleChanged { .. }
        | Event::GitInfo { .. }
        | Event::BranchChanged { .. }
        | Event::GitStatus { .. }
        | Event::GitDiff { .. }
        | Event::ConfigSnapshot { .. }
        | Event::TestResult { .. }
        | Event::ModelInfo { .. }
        | Event::McpServerList { .. }
        | Event::WorkspaceList { .. } => None,
        Event::Error { session_id, .. } => session_id.clone(),
    }
}
