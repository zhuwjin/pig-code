use gpui_kit::assets::IconName as AssetsIconName;

use super::*;
use crate::RelativeTime as _;

/// 「已归档的会话」页的行数据（AppView 从 SessionMeta + 工作区别名汇总喂入）
#[derive(Clone, PartialEq)]
pub(crate) struct ArchivedSessionRow {
    pub id: String,
    pub title: String,
    /// 工作区路径（cwd）
    pub workspace: String,
    /// 工作区显示名（别名优先）
    pub workspace_name: String,
    pub created_at: u64,
    /// 归档动作会刷新 updated_at，此处即「归档时间」
    pub updated_at: u64,
}

/// 归档列表排序：归档时间（默认）/ 创建时间 / 按字母顺序
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArchivedSort {
    ArchivedTime,
    CreatedTime,
    Alphabetical,
}

/// 工作区过滤下拉的「不过滤」哨兵项
const ALL_WORKSPACES: &str = "所有工作区";

impl SettingsView {
    /// 归档列表喂入（AppView 在打开设置页 / SessionList 变化时推送）；值变才 notify
    pub(crate) fn set_archived_sessions(
        &mut self,
        rows: Vec<ArchivedSessionRow>,
        cx: &mut Context<Self>,
    ) {
        if self.archived_sessions != rows {
            self.archived_sessions = rows;
            cx.notify();
        }
    }

    /// 工作区过滤下拉的选项跟随 scope_workspaces（脏标记 + render 前同步，
    /// 与 appearance_dirty 同手法——SelectState::set_items 需要 Window）；
    /// 选中项已消失时回到「所有工作区」
    pub(crate) fn sync_archived_ws_options(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut options = vec![ALL_WORKSPACES.to_string()];
        options.extend(self.scope_workspaces.iter().map(|(_, name)| name.clone()));
        let selected = self
            .archived_workspace
            .read(cx)
            .selected_value()
            .cloned()
            .filter(|v| options.contains(v))
            .unwrap_or_else(|| ALL_WORKSPACES.to_string());
        self.archived_workspace.update(cx, |state, cx| {
            state.set_items(SearchableVec::new(options), window, cx);
            state.set_selected_value(&selected, window, cx);
        });
    }

    pub(crate) fn render_archived_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let query = self.archived_search.read(cx).value().to_lowercase();
        let ws_filter = self
            .archived_workspace
            .read(cx)
            .selected_value()
            .cloned()
            .filter(|v| v != ALL_WORKSPACES);
        let mut rows: Vec<&ArchivedSessionRow> = self
            .archived_sessions
            .iter()
            .filter(|r| query.is_empty() || r.title.to_lowercase().contains(&query))
            .filter(|r| {
                ws_filter
                    .as_deref()
                    .is_none_or(|w| r.workspace_name == w || r.workspace == w)
            })
            .collect();
        match self.archived_sort {
            ArchivedSort::ArchivedTime => rows.sort_by_key(|r| std::cmp::Reverse(r.updated_at)),
            ArchivedSort::CreatedTime => rows.sort_by_key(|r| std::cmp::Reverse(r.created_at)),
            ArchivedSort::Alphabetical => rows.sort_by_key(|r| r.title.to_lowercase()),
        }

        v_flex()
            .w_full()
            .gap_3()
            .child(Input::new(&self.archived_search).small())
            .child(Select::new(&self.archived_workspace).w_full())
            .child(self.render_archived_sort_tabs(cx))
            .child(if rows.is_empty() {
                div()
                    .w_full()
                    .py_10()
                    .rounded_lg()
                    .border_1()
                    .border_color(cx.theme().border)
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .text_center()
                            // 区分「本无归档」与「搜索/过滤无匹配」
                            .child(if self.archived_sessions.is_empty() {
                                "还没有归档的会话"
                            } else {
                                "没有匹配的归档会话"
                            }),
                    )
                    .into_any_element()
            } else {
                v_flex()
                    .gap_1()
                    .children(
                        rows.into_iter()
                            .enumerate()
                            .map(|(ix, row)| self.render_archived_row(row, ix, cx)),
                    )
                    .into_any_element()
            })
            .into_any_element()
    }

    /// 排序切换：归档时间 / 创建时间 / 按字母顺序（分段 pill，对齐 ZCode）
    fn render_archived_sort_tabs(&self, cx: &mut Context<Self>) -> AnyElement {
        let tabs = [
            (
                ArchivedSort::ArchivedTime,
                "归档时间",
                AssetsIconName::Clock,
            ),
            (
                ArchivedSort::CreatedTime,
                "创建时间",
                AssetsIconName::CalendarClock,
            ),
            (
                ArchivedSort::Alphabetical,
                "按字母顺序",
                AssetsIconName::ArrowDownAZ,
            ),
        ];
        h_flex()
            .gap_1()
            .p_0p5()
            .rounded(cx.theme().radius)
            .bg(cx.theme().accent.opacity(0.4))
            .children(
                tabs.into_iter()
                    .enumerate()
                    .map(|(ix, (sort, label, icon))| {
                        let selected = self.archived_sort == sort;
                        h_flex()
                            .id(("archived-sort", ix))
                            .gap_1()
                            .px_2()
                            .py_0p5()
                            .rounded_sm()
                            .cursor_pointer()
                            .text_xs()
                            .when(!selected, |this| {
                                this.text_color(cx.theme().muted_foreground)
                            })
                            .when(selected, |this| this.bg(cx.theme().background))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.archived_sort = sort;
                                cx.notify();
                            }))
                            .child(Icon::new(icon).size_3())
                            .child(label)
                    }),
            )
            .into_any_element()
    }

    /// 归档会话行：标题 + 时间一行，所属工作区 + 恢复/删除按钮一行
    fn render_archived_row(
        &self,
        row: &ArchivedSessionRow,
        ix: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // 时间列跟随排序口径（ZCode 同款：按创建时间排序时显示创建时间）
        let time = match self.archived_sort {
            ArchivedSort::CreatedTime => row.created_at,
            _ => row.updated_at,
        };
        let restore_id = row.id.clone();
        let delete_id = row.id.clone();
        v_flex()
            .id(("archived-session", ix))
            .px_3()
            .py_2()
            .gap_0p5()
            .rounded(cx.theme().radius)
            .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .child(row.title.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(time.relative()),
                    ),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Icon::new(IconName::FolderClosed)
                            .size_3()
                            .text_color(cx.theme().muted_foreground),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .child(row.workspace_name.clone()),
                    )
                    .child(
                        Button::new(("archived-restore", ix))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Undo2)
                            .tooltip("恢复到会话列表")
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(SettingsEvent::RestoreSession(restore_id.clone()));
                            })),
                    )
                    .child(
                        Button::new(("archived-delete", ix))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Delete)
                            .tooltip("删除会话（不可恢复）")
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(SettingsEvent::DeleteSession(delete_id.clone()));
                            })),
                    ),
            )
            .into_any_element()
    }
}
