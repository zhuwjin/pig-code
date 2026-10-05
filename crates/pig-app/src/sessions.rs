use super::*;

impl AppView {
    pub(crate) fn sync_composer_state(&self, cx: &mut Context<Self>) {
        let Some(sid) = &self.current else { return };
        let streaming = self.running.contains(sid);
        // 审批条显示队首那笔：并发审批按到达顺序逐笔答复
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

    /// 工作区列表 = 可见手动工作区 ∪ 会话 cwd（排除已移除/隐藏的工作区）；
    /// 按最近活跃/添加时间倒序。
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

    /// 设置页「已归档的会话」数据：归档会话清单（含工作区显示名）推给设置页
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
            // 快速路径不发 SessionConfigured：清上一个会话的水位，
            // core 的 OpenSession 补发（有数据时）随后到达
            self.composer.update(cx, |composer, cx| {
                composer.clear_context_usage(cx);
            });
            // 已打开过的会话走这条快速路径，core 不会再发 SessionConfigured——
            // 必须按 meta 恢复会话级的模型/模式/思考等级，否则会带着上一个会话的值
            if let Some(meta) = self.metas.iter().find(|m| m.id == session_id).cloned() {
                self.exec_mode = meta.exec_mode;
                self.reasoning_level = meta.reasoning_level.clone();
                let label = match (&meta.provider_id, &meta.model_id) {
                    (Some(p), Some(m)) => {
                        self.current_model = Some((p.clone(), m.clone()));
                        Some(self.model_display_label(p, m))
                    }
                    _ => {
                        self.current_model = None;
                        None
                    }
                };
                self.composer.update(cx, |composer, cx| {
                    composer.set_exec_mode(meta.exec_mode, cx);
                    composer.set_reasoning_level(meta.reasoning_level.clone(), cx);
                    composer.set_fs_access(meta.fs_read_outside, meta.fs_write_outside, cx);
                    if let Some(label) = label {
                        composer.set_model_name(label, cx);
                    }
                });
            }
            self.sync_composer_state(cx);
            self.refresh_git_branch(self.current_cwd(), cx);
            self.refresh_sidebar(cx);
            cx.notify();
            // 仍要通知 core：它按当前会话补发面板快照与上下文水位
            //（SessionConfigured 的处理是幂等的，重复恢复无害）
            self.agent.open_session(session_id);
        } else {
            self.agent.open_session(session_id);
        }
    }

    /// 手动重命名会话：本地缓存即时更新（侧栏立刻生效），
    /// core 落库（置 title_custom，自动命名不再覆盖）后发 SessionList 再同步
    pub(crate) fn rename_session(&mut self, id: &str, title: &str, cx: &mut Context<Self>) {
        if let Some(meta) = self.metas.iter_mut().find(|m| m.id == id) {
            meta.title = title.to_string();
        }
        self.agent.rename_session(id, title);
        self.refresh_sidebar(cx);
        cx.notify();
    }

    /// 删除会话：本地视图/缓存清理 + 通知 core 清库与 rollout 文件。
    /// 删的是当前会话时切到最近的未归档会话，没有则回 hero
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

    /// 标题栏分支：cwd=None（hero 态）或非 git 目录时不显示。
    /// 一次查齐当前分支 + 本地分支列表（切换菜单用）
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

    /// ConfigSnapshot → composer 模型列表（启用供应商的启用模型，按供应商分组平铺）
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
                            // (等级 id, 显示名)——显示名缺省回退 id 本身
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
        // 主题模式可能在设置页关闭期间被系统外观改变，打开时重新同步下拉
        self.settings.update(cx, |settings, cx| {
            settings.appearance_dirty = true;
            cx.notify();
        });
        cx.notify();
    }

    /// 把侧栏同口径的工作区清单喂给设置页（MCP/技能两页作用域选择器的候选）
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

    /// 设置页 MCP 数据刷新：按设置页作用域（用户级 / 指定工作区）重读 mcp.json，
    /// 并查询当前会话的连接状态（无活动会话时不查询，页面只展示配置）
    pub(crate) fn refresh_mcp(&mut self, cx: &mut Context<Self>) {
        let session_id = self.current.clone();
        let session_cwd = self.current_cwd();
        let workspace = match self.settings.read(cx).mcp_scope().clone() {
            // 用户级：不参与项目合并（页面只列用户级条目）
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

    /// 设置页技能数据刷新：按设置页作用域（用户级 / 指定工作区）重读技能目录。
    /// 技能是静态文件（无连接态），不需要向 core 查询
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
