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
        if !self.ensure_session_row_visible(&id, window, cx) {
            return;
        }
        self.renaming = Some(RenameTarget::Session(id));
        self.rename_input.update(cx, |input, cx| {
            input.set_value(title, window, cx);
            input.focus(window, cx);
        });
        cx.notify();
    }

    /// 行内重命名输入框画在会话行上，先保证该行真实渲染：普通会话在工作区
    /// 视图可能被折叠/分页/名称过滤挡住，退回平铺列表并清掉拦路搜索词；
    /// 归档会话不在侧栏渲染（设置页「已归档的会话」管理），返回 false 放弃
    fn ensure_session_row_visible(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.session_row_visible(id, cx) {
            return true;
        }
        if self.sessions.iter().any(|s| s.id == id && s.archived) {
            return false;
        }
        self.view = SidebarView::Flat;
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
        self.session_row_visible(id, cx)
    }

    /// 会话行在当前视图下是否可见：归档会话恒不可见（不在侧栏渲染）；
    /// 平铺视图看标题过滤；工作区视图还要求工作区未被搜索过滤、已展开
    /// 且在分页范围内
    fn session_row_visible(&self, id: &str, cx: &App) -> bool {
        let Some(session) = self.sessions.iter().find(|s| s.id == id) else {
            return false;
        };
        if session.archived {
            return false;
        }
        let query = self.query(cx);
        match self.view {
            SidebarView::Flat => self.matches(&query, &session.title),
            SidebarView::Workspace => {
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
