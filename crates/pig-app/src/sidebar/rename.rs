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

    /// The inline rename input is drawn on the session row, so first ensure the row
    /// actually renders: a regular session may be hidden in the workspace view by
    /// collapsing/pagination, so fall back to the flat list; archived sessions are
    /// not rendered in the sidebar (managed on the settings page's "archived
    /// sessions"), return false to give up
    fn ensure_session_row_visible(
        &mut self,
        id: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.session_row_visible(id, cx) {
            return true;
        }
        if self.sessions.iter().any(|s| s.id == id && s.archived) {
            return false;
        }
        self.view = SidebarView::Flat;
        self.session_row_visible(id, cx)
    }

    /// Whether a session row is visible in the current view: archived sessions are
    /// never visible (not rendered in the sidebar); the workspace view requires
    /// the workspace expanded and the session within the pagination range
    fn session_row_visible(&self, id: &str, _cx: &App) -> bool {
        let Some(session) = self.sessions.iter().find(|s| s.id == id) else {
            return false;
        };
        if session.archived {
            return false;
        }
        match self.view {
            SidebarView::Flat => true,
            SidebarView::Workspace => {
                if session.pinned {
                    return true;
                }
                let workspace = session.cwd.display().to_string();
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
                // An empty value means cancel; no rename
                if !value.is_empty() {
                    cx.emit(SidebarEvent::RenameSession(id, value));
                }
            }
        }
        cx.notify();
    }
}
