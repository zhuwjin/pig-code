use super::*;

impl Sidebar {
    pub(crate) fn start_rename(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
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
        self.renaming = Some(RenameTarget::Session(id));
        self.rename_input.update(cx, |input, cx| {
            input.set_value(title, window, cx);
            input.focus(window, cx);
        });
        cx.notify();
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
