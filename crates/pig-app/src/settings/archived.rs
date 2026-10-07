use gpui_kit::assets::IconName as AssetsIconName;

use super::*;
use crate::RelativeTime as _;

/// Row data of the "archived sessions" page (fed by AppView from SessionMeta plus
/// workspace aliases)
#[derive(Clone, PartialEq)]
pub(crate) struct ArchivedSessionRow {
    pub id: String,
    pub title: String,
    /// Workspace path (cwd)
    pub workspace: String,
    /// Workspace display name (alias first)
    pub workspace_name: String,
    pub created_at: u64,
    /// Archiving refreshes updated_at, so this is effectively the "archived time"
    pub updated_at: u64,
}

/// Archived list sort: archived time (default) / created time / alphabetical
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArchivedSort {
    ArchivedTime,
    CreatedTime,
    Alphabetical,
}

/// The workspace filter dropdown's "no filter" sentinel entry; its label changes
/// with the UI language, and options/backfill/filter comparisons all take the
/// string from this function (sync_archived_ws_options rebuilds options)
pub(crate) fn all_workspaces_label() -> String {
    rust_i18n::t!("settings.archived.all_workspaces").to_string()
}

impl SettingsView {
    /// Archived list feed (pushed by AppView when the settings page opens /
    /// SessionList changes); notify only when the value changes
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

    /// The workspace filter dropdown's options follow scope_workspaces (dirty
    /// flag plus sync before render, same approach as appearance_dirty —
    /// SelectState::set_items needs a Window); when the selected entry
    /// disappears, fall back to "all workspaces"
    pub(crate) fn sync_archived_ws_options(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut options = vec![all_workspaces_label()];
        options.extend(self.scope_workspaces.iter().map(|(_, name)| name.clone()));
        let selected = self
            .archived_workspace
            .read(cx)
            .selected_value()
            .cloned()
            .filter(|v| options.contains(v))
            .unwrap_or_else(all_workspaces_label);
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
            .filter(|v| *v != all_workspaces_label());
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
                            // Distinguish "nothing archived" from "search/filter
                            // has no match"
                            .child(if self.archived_sessions.is_empty() {
                                rust_i18n::t!("settings.archived.empty_none").to_string()
                            } else {
                                rust_i18n::t!("settings.archived.empty_no_match").to_string()
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

    /// Sort switch: archived time / created time / alphabetical (segmented
    /// pills, aligned with ZCode)
    fn render_archived_sort_tabs(&self, cx: &mut Context<Self>) -> AnyElement {
        let tabs = [
            (
                ArchivedSort::ArchivedTime,
                rust_i18n::t!("settings.archived.sort_archived"),
                AssetsIconName::Clock,
            ),
            (
                ArchivedSort::CreatedTime,
                rust_i18n::t!("settings.archived.sort_created"),
                AssetsIconName::CalendarClock,
            ),
            (
                ArchivedSort::Alphabetical,
                rust_i18n::t!("settings.archived.sort_alpha"),
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

    /// Archived session row: title plus time on one line, owning workspace plus
    /// restore/delete buttons on another
    fn render_archived_row(
        &self,
        row: &ArchivedSessionRow,
        ix: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // The time column follows the sort criterion (like ZCode: created time
        // is shown when sorting by created time)
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
                            .child(crate::sidebar::display_title(&row.title)),
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
                            .tooltip(rust_i18n::t!("settings.archived.restore_tooltip"))
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(SettingsEvent::RestoreSession(restore_id.clone()));
                            })),
                    )
                    .child(
                        Button::new(("archived-delete", ix))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Delete)
                            .tooltip(rust_i18n::t!("settings.archived.delete_tooltip"))
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(SettingsEvent::DeleteSession(delete_id.clone()));
                            })),
                    ),
            )
            .into_any_element()
    }
}
