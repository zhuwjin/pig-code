use super::*;

impl AppView {
    pub(crate) fn sync_composer_state(&self, cx: &mut Context<Self>) {
        let Some(sid) = &self.current else { return };
        let streaming = self.running.contains(sid);
        // The approval bar shows the head of the queue: concurrent approvals are answered one by one in arrival order
        let approval = self
            .pending_approvals
            .get(sid)
            .and_then(|queue| queue.front().cloned());
        let question = self.pending_questions.get(sid).cloned();
        self.composer.update(cx, |composer, cx| {
            composer.set_streaming(streaming, cx);
            composer.set_approval(approval, cx);
            composer.set_question(question, cx);
        });
    }

    /// Workspace list = visible manual workspaces ∪ session cwds (excluding
    /// removed/hidden workspaces); sorted by latest activity/add time, newest
    /// first.
    pub(crate) fn compute_workspaces(&self) -> Vec<String> {
        let mut latest: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        for meta in &self.metas {
            let key = meta.cwd.display().to_string();
            if self.hidden_workspaces.contains(&key) {
                continue;
            }
            let entry = latest.entry(key).or_insert(0);
            *entry = (*entry).max(meta.updated_at);
        }
        for path in &self.workspaces {
            latest.entry(path.clone()).or_insert(0);
        }
        let mut all: Vec<(String, u64)> = latest.into_iter().collect();
        all.sort_by_key(|(_, ts)| std::cmp::Reverse(*ts));
        all.into_iter().map(|(path, _)| path).collect()
    }

    pub(crate) fn refresh_sidebar(&self, cx: &mut Context<Self>) {
        let sessions: Vec<SidebarSession> = self
            .metas
            .iter()
            .map(|meta| SidebarSession {
                id: meta.id.clone(),
                title: meta.title.clone(),
                cwd: meta.cwd.clone(),
                updated_at: meta.updated_at,
                pinned: meta.pinned,
                archived: meta.archived,
                running: self.running.contains(&meta.id),
                waiting_approval: self.approval_pending.contains(&meta.id),
            })
            .collect();
        let workspaces = self.compute_workspaces();
        let aliases = self.workspace_aliases.clone();
        let active = self.current.clone();
        self.sidebar.update(cx, |sidebar, cx| {
            sidebar.set_state(sessions, workspaces, aliases, active, cx);
        });
        if self.settings_open {
            self.sync_archived_page(cx);
        }
    }

    /// Data for the settings page's "archived sessions": the archived session list (with workspace display names) is pushed to the settings page
    pub(crate) fn sync_archived_page(&self, cx: &mut Context<Self>) {
        let rows: Vec<crate::settings::ArchivedSessionRow> = self
            .metas
            .iter()
            .filter(|m| m.archived)
            .map(|m| {
                let workspace = m.cwd.display().to_string();
                let workspace_name = crate::settings::workspace_display_name(
                    &m.cwd,
                    self.workspace_aliases.get(&workspace).map(String::as_str),
                );
                crate::settings::ArchivedSessionRow {
                    id: m.id.clone(),
                    title: m.title.clone(),
                    workspace,
                    workspace_name,
                    created_at: m.created_at,
                    updated_at: m.updated_at,
                }
            })
            .collect();
        self.settings.update(cx, |settings, cx| {
            settings.set_archived_sessions(rows, cx);
        });
    }

    pub(crate) fn switch_session(&mut self, session_id: String, cx: &mut Context<Self>) {
        if self.views.contains_key(&session_id) {
            self.current = Some(session_id.clone());
            // The fast path does not send SessionConfigured: clear the previous
            // session's watermark; the one re-sent by core's OpenSession (when
            // it has data) arrives right after
            self.composer.update(cx, |composer, cx| {
                composer.clear_context_usage(cx);
            });
            // A previously opened session takes this fast path and core will not
            // send SessionConfigured again, so the session-level model/mode/
            // reasoning level must be restored from meta; otherwise the previous
            // session's values would linger
            if let Some(meta) = self.metas.iter().find(|m| m.id == session_id).cloned() {
                self.exec_mode = meta.exec_mode;
                self.plan_enabled = meta.plan_enabled;
                self.reasoning_level = meta.reasoning_level.clone();
                let label = match (&meta.provider_id, &meta.model_id) {
                    (Some(p), Some(m)) => {
                        self.current_model = Some((p.clone(), m.clone()));
                        self.model_display_label(p, m)
                    }
                    // Session with no configured model: show the "no model configured" placeholder (no leftover label from the previous session)
                    _ => {
                        self.current_model = None;
                        rust_i18n::t!("composer.no_model").to_string()
                    }
                };
                self.composer.update(cx, |composer, cx| {
                    composer.set_exec_mode(meta.exec_mode, cx);
                    composer.set_plan_enabled(meta.plan_enabled, cx);
                    composer.set_reasoning_level(meta.reasoning_level.clone(), cx);
                    composer.set_fs_access(meta.fs_read_outside, meta.fs_write_outside, cx);
                    composer.set_model_name(label, cx);
                });
            }
            self.sync_composer_state(cx);
            self.refresh_git_branch(self.current_cwd(), cx);
            self.refresh_sidebar(cx);
            cx.notify();
            // Still notify core: it re-sends the panel snapshot and context
            // watermark for the current session (SessionConfigured handling is
            // idempotent; restoring twice is harmless)
            self.agent.open_session(session_id);
        } else {
            self.agent.open_session(session_id);
        }
    }

    /// Manually rename a session: the local cache updates immediately (the
    /// sidebar reflects it at once); core persists it (sets title_custom so
    /// auto-naming no longer overwrites) then sends SessionList to re-sync
    pub(crate) fn rename_session(&mut self, id: &str, title: &str, cx: &mut Context<Self>) {
        if let Some(meta) = self.metas.iter_mut().find(|m| m.id == id) {
            meta.title = title.to_string();
        }
        self.agent.rename_session(id, title);
        self.refresh_sidebar(cx);
        cx.notify();
    }

    /// Delete a session: clean up local views/caches plus tell core to clear its
    /// store and the rollout file. When the current session is deleted, switch to
    /// the most recent unarchived session, or return to the hero screen if none
    pub(crate) fn delete_session(&mut self, id: &str, cx: &mut Context<Self>) {
        self.deleted_sessions.insert(id.to_string());
        self.views.remove(id);
        self.running.remove(id);
        self.approval_pending.remove(id);
        self.pending_approvals.remove(id);
        self.pending_questions.remove(id);
        self.todos_by_session.remove(id);
        self.tasks_by_session.remove(id);
        self.metas.retain(|m| m.id != id);
        self.agent.delete_session(id);
        if self.current.as_deref() == Some(id) {
            self.current = None;
            match self.metas.iter().find(|m| !m.archived) {
                Some(next) => self.switch_session(next.id.clone(), cx),
                None => self.enter_hero(cx),
            }
        }
        self.refresh_sidebar(cx);
        cx.notify();
    }

    /// Title bar branch: hidden when cwd=None (hero state) or the directory is
    /// not a git repo. Fetches the current branch plus the local branch list in
    /// one go (for the switch menu)
    pub(crate) fn refresh_git_branch(&mut self, cwd: Option<PathBuf>, cx: &mut Context<Self>) {
        let task = cx.background_executor().spawn(async move {
            let Some(cwd) = cwd else {
                return (None, Vec::new());
            };
            pig_core::git::git_info(&cwd)
        });
        cx.spawn(async move |this: WeakEntity<AppView>, cx| {
            let (branch, branches) = task.await;
            let _ = this.update(cx, |app, cx| {
                app.git_branch = branch;
                app.title_branches = branches;
                cx.notify();
            });
        })
        .detach();
    }

    /// ConfigSnapshot → composer model list (enabled models of enabled providers, flattened grouped by provider)
    pub(crate) fn apply_config_to_composer(&self, cx: &mut Context<Self>) {
        let Some(config) = &self.config else { return };
        let models: Vec<crate::composer::ModelOption> = config
            .providers
            .iter()
            .filter(|p| p.enabled)
            .flat_map(|p| {
                p.models
                    .iter()
                    .filter(|m| m.enabled)
                    .map(|m| {
                        (
                            p.name.clone(),
                            p.id.clone(),
                            m.id.clone(),
                            // (level id, display name); the display name falls back to the id itself when absent
                            m.reasoning_levels
                                .iter()
                                .map(|lv| {
                                    let label = m
                                        .reasoning_labels
                                        .get(lv)
                                        .cloned()
                                        .unwrap_or_else(|| lv.clone());
                                    (lv.clone(), label)
                                })
                                .collect::<Vec<_>>(),
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        self.composer
            .update(cx, |composer, cx| composer.set_models(models, cx));
    }

    pub(crate) fn open_settings(&mut self, cx: &mut Context<Self>) {
        self.settings_open = true;
        self.agent.get_config();
        self.sync_scope_workspaces(cx);
        self.sync_archived_page(cx);
        self.refresh_mcp(cx);
        self.refresh_skills(cx);
        // The theme mode may have changed via system appearance while the settings page was closed; re-sync the dropdown on open
        self.settings.update(cx, |settings, cx| {
            settings.appearance_dirty = true;
            cx.notify();
        });
        cx.notify();
    }

    /// Feed the settings page the workspace list computed the same way as the sidebar (candidates for the scope pickers on the MCP/skills pages)
    pub(crate) fn sync_scope_workspaces(&mut self, cx: &mut Context<Self>) {
        let aliases = self.workspace_aliases.clone();
        let entries: Vec<(std::path::PathBuf, String)> = self
            .compute_workspaces()
            .into_iter()
            .map(|path| {
                let display = crate::settings::workspace_display_name(
                    std::path::Path::new(&path),
                    aliases.get(&path).map(String::as_str),
                );
                (std::path::PathBuf::from(path), display)
            })
            .collect();
        self.settings.update(cx, |settings, cx| {
            settings.set_scope_workspaces(entries, cx);
        });
    }

    /// Refresh settings-page MCP data: re-read mcp.json per the settings page's
    /// scope (user level / a chosen workspace), and query the current session's
    /// connection state (no query without an active session; the page then shows
    /// configuration only)
    pub(crate) fn refresh_mcp(&mut self, cx: &mut Context<Self>) {
        let session_id = self.current.clone();
        let session_cwd = self.current_cwd();
        let workspace = match self.settings.read(cx).mcp_scope().clone() {
            // User level: no project merge involved (the page lists user-level entries only)
            crate::settings::McpScope::User => None,
            crate::settings::McpScope::Workspace(path) => Some(path),
        };
        let snapshot = crate::settings::load_mcp_snapshot(workspace.as_deref());
        self.settings.update(cx, |settings, cx| {
            settings.set_mcp_config(session_id.clone(), session_cwd.clone(), snapshot, cx);
        });
        if let Some(session_id) = session_id {
            self.agent.list_mcp_servers(session_id);
        }
    }

    /// Refresh settings-page skills data: re-read the skills directory per the
    /// settings page's scope (user level / a chosen workspace). Skills are
    /// static files (no connection state), so no core query is needed
    pub(crate) fn refresh_skills(&mut self, cx: &mut Context<Self>) {
        let workspace = match self.settings.read(cx).skills_scope().clone() {
            crate::settings::McpScope::User => None,
            crate::settings::McpScope::Workspace(path) => Some(path),
        };
        let snapshot = crate::settings::load_skills_snapshot(workspace.as_deref());
        self.settings.update(cx, |settings, cx| {
            settings.set_skills_config(snapshot, cx);
        });
    }
}
