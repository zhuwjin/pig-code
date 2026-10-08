use super::*;

impl AppView {
    pub(crate) fn route_event(&mut self, event: Event, cx: &mut Context<Self>) {
        // Late events of deleted sessions (streaming/wrap-up events enqueued
        // before deletion) are dropped outright, preventing ensure_views from
        // rebuilding zombie views for a deleted session
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
                plan_enabled,
                fs_read_outside,
                fs_write_outside,
            } => {
                let session_id = session_id.clone();
                eprintln!(
                    "[model] SessionConfigured {session_id} -> provider={provider_id:?} model={model_id:?} thinking={reasoning_level:?}"
                );
                self.ensure_views(&session_id, cx);
                self.current = Some(session_id.clone());
                // Session switched: if the trajectory tab is open, reload the new
                // session's persisted records
                if self.right_tabs.contains(&RightTab::Trajectory) {
                    self.reload_trajectory();
                }
                // Session switched: clear the previous session's context watermark
                // first (sessions with data receive a re-push shortly after)
                self.composer.update(cx, |composer, cx| {
                    composer.clear_context_usage(cx);
                });
                // Empty label = the "no model configured" sentinel (core sends
                // an empty-string sentinel; the composer chip renders the
                // localized placeholder at draw time)
                let label = if model.is_empty() {
                    String::new()
                } else if provider_name.is_empty() {
                    model.clone()
                } else {
                    format!("{provider_name}/{model}")
                };
                // Restore the session's model/mode/reasoning level (persisted by
                // core in the sessions table)
                self.exec_mode = *exec_mode;
                self.plan_enabled = *plan_enabled;
                self.reasoning_level = reasoning_level.clone();
                self.current_model = match (provider_id, model_id) {
                    (Some(p), Some(m)) => Some((p.clone(), m.clone())),
                    _ => None,
                };
                self.composer.update(cx, |composer, cx| {
                    composer.set_model_name(label, cx);
                    composer.set_hero_mode(false, cx);
                    composer.set_exec_mode(*exec_mode, cx);
                    composer.set_plan_enabled(*plan_enabled, cx);
                    composer.set_reasoning_level(reasoning_level.clone(), cx);
                    composer.set_fs_access(*fs_read_outside, *fs_write_outside, cx);
                });
                // Session switched/created: sync the progress/task panels from
                // the cached snapshots (cleared when there is no snapshot)
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
                // Session switched/created: the title bar branch follows the
                // session cwd; pull the workspace git status (Review panel)
                self.refresh_git_branch(Some(cwd.clone()), cx);
                self.agent.git_status(cwd.clone());
            }
            Event::ModelInfo { id, info } => {
                // Backfilling InputState::set_value needs a window; enter the
                // window context via AnyWindowHandle
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
                // Arrives both on startup load and after every save: apply the
                // config's fonts to the global theme (idempotent)
                font::apply_config_fonts(config, cx);
                // The language is likewise applied idempotently (windows are not
                // refreshed when the effective locale is unchanged)
                crate::i18n::apply_config_language(config, cx);
                // Sync the shell config when the terminal panel already exists
                // (open tabs unchanged; takes effect for later new ones)
                if let Some(panel) = &self.terminal {
                    let shell = config.terminal_shell.clone();
                    panel.update(cx, |panel, cx| panel.set_shell(shell, cx));
                }
                let config = config.clone();
                self.settings
                    .update(cx, |settings, cx| settings.set_config(config, cx));
                self.apply_config_to_composer(cx);
            }
            Event::TestResult {
                provider_id,
                result,
            } => {
                // Stored structured; the settings page localizes ok/failure
                // text at render time (failures pass the upstream English
                // detail through verbatim)
                self.settings.update(cx, |settings, cx| {
                    settings.set_test_result(provider_id, result.clone(), cx);
                });
            }
            Event::McpServerList {
                session_id,
                servers,
            } => {
                // Settings page MCP status reply: only accept the answer of the
                // currently active session
                if self.current.as_deref() == Some(session_id.as_str()) {
                    let servers = servers.clone();
                    self.settings.update(cx, |settings, cx| {
                        settings.set_mcp_status(servers, cx);
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
                // Sync workspace list changes to the settings page (candidates
                // for the MCP page scope selector)
                self.sync_scope_workspaces(cx);
            }
            Event::SessionList { sessions } => {
                self.metas = sessions.clone();
                if self.current.is_none()
                    && !self.metas.is_empty()
                    && let Some(meta) = self.metas.iter().find(|s| !s.archived)
                {
                    self.agent.open_session(meta.id.clone());
                }
                self.push_hero_info(cx);
            }
            Event::SessionTitleChanged { session_id, title } => {
                // Auto-naming sidecar finished: patch only the title, do not
                // resend the whole list
                if let Some(meta) = self.metas.iter_mut().find(|m| &m.id == session_id) {
                    meta.title = title.clone();
                }
                self.refresh_sidebar(cx);
            }
            Event::ExecModeChanged {
                session_id, mode, ..
            } => {
                // Core switched the mode on its own: sync chip/cache
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
            Event::PlanModeChanged {
                session_id,
                enabled,
                ..
            } => {
                // The model toggled itself via EnterPlanMode/ExitPlanMode: sync
                // chip/cache
                self.plan_enabled = *enabled;
                if let Some(meta) = self.metas.iter_mut().find(|m| &m.id == session_id) {
                    meta.plan_enabled = *enabled;
                }
                let enabled = *enabled;
                self.composer.update(cx, |composer, cx| {
                    composer.set_plan_enabled(enabled, cx);
                });
                cx.notify();
            }
            Event::Error {
                session_id: None,
                error,
                ..
            } => {
                // Stored structured; the "⚠ {localized text}" note/hero banner
                // is built at draw time (a language switch updates it too)
                if self.is_hero(cx) {
                    self.hero_error = Some(error.clone());
                } else if let Some(sid) = self.current.clone() {
                    self.ensure_views(&sid, cx);
                    self.views[&sid].thread.update(cx, |thread, cx| {
                        thread.add_error_note(error, cx);
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
                // Title bar branch switcher: the current session's directory
                // switched branch → update the chip + re-pull the branch list, and
                // refresh the workspace git status (after a branch switch the
                // changes panel content changes entirely)
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
                // Workspace scope: all session panels with the same cwd are
                // updated
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
                // Changes chip above the composer: git scope (unstaged + staged
                // merged stats)
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
                cwd,
                path,
                diff,
                note,
                ..
            } => {
                let (path, diff, note) = (path.clone(), diff.clone(), *note);
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
                            review.set_git_diff(path, diff, note, cx);
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
                // Turn ended with the trajectory tab in front: reload the
                // persisted records (this turn's new calls are already written)
                if *duration_ms > 0 && self.right_active.as_ref() == Some(&RightTab::Trajectory) {
                    self.reload_trajectory();
                }
                self.refresh_git_branch(self.current_cwd(), cx);
                // Turn ended (agent file writes have settled): refresh the
                // workspace git status
                if let Some(meta) = self.metas.iter().find(|m| &m.id == session_id) {
                    self.agent.git_status(meta.cwd.clone());
                }
            }
            Event::CompactStarted { session_id, .. } => {
                let session_id = session_id.clone();
                self.ensure_views(&session_id, cx);
                self.views[&session_id].thread.update(cx, |thread, cx| {
                    thread.set_compacting(true, cx);
                });
            }
            Event::ContextCompacted {
                session_id,
                note,
                omitted,
                ..
            } => {
                let session_id = session_id.clone();
                self.ensure_views(&session_id, cx);
                let note = note.clone();
                let omitted = *omitted;
                self.views[&session_id].thread.update(cx, |thread, cx| {
                    thread.set_compacting(false, cx);
                    if omitted == 0 {
                        // "History too short to compact": short texts are laid
                        // out flat directly
                        thread.add_system_note(&note, cx);
                    } else {
                        thread.add_compact_note(&note, cx);
                    }
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
                // Content arrived for the right "Subagent" tab: update only an
                // already-open tab; a session mismatch (late/cross-session event)
                // is ignored
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
                // Live subagent increments: append display items / turn off
                // "running" at the end. After finished, one full re-pull closes it
                // out — incremental appends and the initial load belong to
                // different tasks (the same FIFO event channel, but file reads
                // race the driver), and the full overwrite self-heals the narrow
                // race
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

        // Running state tracking (sidebar status dot + the current session's
        // composer state)
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
                Event::ApprovalRequested {
                    request_id,
                    tool,
                    detail,
                    danger_key,
                    ..
                } => {
                    self.approval_pending.insert(sid.clone());
                    let cwd = self
                        .metas
                        .iter()
                        .find(|m| &m.id == sid)
                        .map(|m| m.cwd.display().to_string())
                        .unwrap_or_default();
                    // Queue instead of overwriting a single slot: concurrent
                    // approvals (e.g. Swarm with multiple subagents) each keep an
                    // entry, shown and answered one by one on the approval bar;
                    // duplicate events with the same request_id are skipped
                    // idempotently
                    let queue = self.pending_approvals.entry(sid.clone()).or_default();
                    if !queue.iter().any(|p| p.request_id == *request_id) {
                        queue.push_back(PendingApproval {
                            request_id: request_id.clone(),
                            session_id: sid.clone(),
                            tool: tool.clone(),
                            detail: detail.clone(),
                            danger_key: danger_key.clone(),
                            cwd,
                        });
                    }
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

        // Route to per-session views
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
                        // The revert changed workspace content: refresh git
                        // status
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
                // User messages reach the screen uniformly via core's
                // Event::UserMessage (including the queued-then-dequeued path)
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
                // Workspace changed: re-lay defaults from the new workspace's
                // most recently active session
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
                if let Some(sid) = &self.current
                    && let Some(views) = self.views.get(sid)
                {
                    views.thread.update(cx, |thread, cx| thread.clear(cx));
                }
            }
            ComposerEvent::Compact { instruction } => {
                if let Some(sid) = &self.current {
                    self.agent.compact(sid.clone(), instruction.clone());
                }
            }
            ComposerEvent::SetModel {
                provider_id,
                model_id,
            } => {
                eprintln!(
                    "[model] switch received -> {provider_id}/{model_id} (hero={}, previous thinking={:?})",
                    self.current.is_none(),
                    self.reasoning_level
                );
                self.current_model = Some((provider_id.clone(), model_id.clone()));
                if self.current.is_none() {
                    // An explicit selection in hero mode: workspace seed defaults
                    // no longer override the model afterwards
                    self.hero_model_dirty = true;
                }
                // Reasoning level landing after a model switch (priority high to
                // low):
                // 1. The new model's configured default tier
                // 2. Inherit the previous model's level (requires the new model to
                //    support it; None = off, inherited as-is)
                // 3. Inherited level unsupported (Some but not in the list) →
                //    heuristic fallback (high → first non-off tier)
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
                        "[model] switching {model_id}: thinking level {:?} -> {:?}",
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
                // The reasoning level is independent of model selection:
                // write-through and apply even without an explicit model pick
                // (acts on the default model)
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
            ComposerEvent::SetPlanMode(enabled) => {
                self.apply_plan_mode(*enabled, cx);
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
            ComposerEvent::OpenFile { path } => {
                if let Some(sid) = self.current.clone() {
                    self.open_file_tab(&sid, path.clone(), None, cx);
                }
            }
            ComposerEvent::DecideApproval {
                request_id,
                decision,
                feedback,
            } => {
                // Take the single ThreadView path: update the in-thread approval
                // card state directed by the request_id carried on the approval
                // bar, and reply to core via ThreadEvent::ApprovalReply (which
                // also removes the entry from the queue there)
                if let Some(sid) = &self.current
                    && let Some(views) = self.views.get(sid)
                {
                    let request_id = request_id.clone();
                    let decision = *decision;
                    let feedback = feedback.clone();
                    views.thread.update(cx, |thread, cx| {
                        thread.decide_approval_by_id(&request_id, decision, feedback, cx);
                    });
                }
            }
            ComposerEvent::QuestionReply {
                request_id,
                answers,
            } => {
                // Remove the question bar immediately after submit/skip (not
                // waiting for turn end) and restore the composer
                if let Some(sid) = &self.current {
                    self.pending_questions.remove(sid);
                }
                self.agent
                    .question_reply(request_id.clone(), answers.clone());
                self.sync_composer_state(cx);
            }
            ComposerEvent::SearchFiles(query) => {
                // In a session, search the session directory; hero (no session
                // yet) searches the hero workspace — previously @ searches on
                // hero were dropped outright and the popup never got results
                let (sid, cwd) = match &self.current {
                    Some(sid) => (sid.clone(), None),
                    None => (
                        "hero".to_string(),
                        Some(
                            self.hero_cwd
                                .clone()
                                .unwrap_or_else(|| self.cwd.clone())
                                .to_string_lossy()
                                .into_owned(),
                        ),
                    ),
                };
                self.agent.search_files(sid, query.clone(), cwd);
            }
            ComposerEvent::OpenChanges => {
                self.open_right_tab(RightTab::Changes, cx);
            }
            ComposerEvent::OpenSubagent { agent_id, title } => {
                // Task row click in the "background Agent" popup: open the right
                // subagent conversation tab (the popup already collapsed on the
                // composer side; same agent_id focuses without reloading)
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
        | Event::CompactStarted { session_id, .. }
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
        | Event::PlanModeChanged { session_id, .. }
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
