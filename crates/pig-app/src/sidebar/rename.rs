use super::*;

impl Sidebar {
    pub(crate) fn start_rename(
        &mut self,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = self.workspace_name(&path);
        self.renaming = Some(RenameTarget::Workspace(path));
        self.rename_input.update(cx, |input, cx| {
            input.set_value(current, window, cx);
            input.focus(window, cx);
        });
        cx.notify();
    }

    pub(crate) fn start_session_rename(
        &mut self,
        id: String,
        title: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ensure_session_row_visible(&id, window, cx);
        self.renaming = Some(RenameTarget::Session(id));
        self.rename_input.update(cx, |input, cx| {
            input.set_value(title, window, cx);
            input.focus(window, cx);
        });
        cx.notify();
    }

    /// 行内重命名输入框画在会话行上，先保证该行真实渲染：不可见时退回
    /// 分组视图（归档会话同时展开归档区），并清掉会过滤掉它的搜索词
    fn ensure_session_row_visible(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.session_row_visible(id, cx) {
            return;
        }
        self.view = SidebarView::Group;
        if self.sessions.iter().any(|s| s.id == id && s.archived) {
            self.archived_open = true;
        }
        let blocked_by_query = self
            .sessions
            .iter()
            .find(|s| s.id == id)
            .is_some_and(|s| !self.matches(&self.query(cx), &s.title));
        if blocked_by_query {
            self.search_input.update(cx, |input, cx| {
                input.set_value("", window, cx);
            });
        }
    }

    /// 会话行在当前视图下是否可见：分组视图看归档区开合；工作区视图下归档
    /// 会话不渲染，普通会话还要求工作区未被搜索过滤、已展开且在分页范围内
    fn session_row_visible(&self, id: &str, cx: &App) -> bool {
        let Some(session) = self.sessions.iter().find(|s| s.id == id) else {
            return false;
        };
        let query = self.query(cx);
        match self.view {
            SidebarView::Group => {
                self.matches(&query, &session.title) && (!session.archived || self.archived_open)
            }
            SidebarView::Workspace => {
                if session.archived {
                    return false;
                }
                if session.pinned {
                    return self.matches(&query, &session.title);
                }
                let workspace = session.cwd.display().to_string();
                // 工作区视图按工作区名/路径过滤，不过滤会话标题
                if !self.matches(&query, &self.workspace_name(&workspace))
                    && !self.matches(&query, &workspace)
                {
                    return false;
                }
                if !self.expanded.contains(&workspace) {
                    return false;
                }
                let shown = self
                    .workspace_shown
                    .get(&workspace)
                    .copied()
                    .unwrap_or(WORKSPACE_PAGE_SIZE);
                let mut rows: Vec<(&str, u64)> = self
                    .sessions
                    .iter()
                    .filter(|s| {
                        s.cwd.display().to_string() == workspace && !s.pinned && !s.archived
                    })
                    .map(|s| (s.id.as_str(), s.updated_at))
                    .collect();
                rows.sort_by_key(|(_, updated_at)| std::cmp::Reverse(*updated_at));
                rows.iter().take(shown).any(|(sid, _)| *sid == id)
            }
        }
    }

    pub(crate) fn commit_rename(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.renaming.take() else {
            return;
        };
        let value = self.rename_input.read(cx).value().trim().to_string();
        match target {
            RenameTarget::Workspace(path) => {
                let default = std::path::Path::new(&path)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                let alias = if value.is_empty() || value == default {
                    None
                } else {
                    Some(value)
                };
                cx.emit(SidebarEvent::RenameWorkspace(path, alias));
            }
            RenameTarget::Session(id) => {
                // 空值视为取消，不改名
                if !value.is_empty() {
                    cx.emit(SidebarEvent::RenameSession(id, value));
                }
            }
        }
        cx.notify();
    }
}
