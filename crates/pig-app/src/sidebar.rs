use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::RelativeTime as _;

/// 侧栏会话行数据（AppView 汇总 SessionMeta + 运行时状态后传入）
pub struct SidebarSession {
    pub id: String,
    pub title: String,
    pub cwd: std::path::PathBuf,
    pub updated_at: u64,
    pub pinned: bool,
    pub archived: bool,
    pub running: bool,
    pub waiting_approval: bool,
    pub added: Option<u32>,
    pub removed: Option<u32>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SidebarView {
    Group,
    Project,
}

#[derive(Clone)]
pub enum SidebarEvent {
    Select(String),
    NewTask,
    /// 在指定项目目录下新建任务（hero 预设 cwd）
    NewTaskInProject(String),
    SetPinned(String, bool),
    SetArchived(String, bool),
    AddProject,
    RemoveProject(String),
    OpenSettings,
}

impl EventEmitter<SidebarEvent> for Sidebar {}

pub struct Sidebar {
    view: SidebarView,
    sessions: Vec<SidebarSession>,
    /// 项目列表 = 手动项目 ∪ 会话 cwd（AppView 已排序）
    projects: Vec<String>,
    /// 手动添加的项目（无会话的项目才可移除）
    manual_projects: Vec<String>,
    active: Option<String>,
    search_open: bool,
    search_input: Entity<InputState>,
    expanded: std::collections::HashSet<String>,
    archived_open: bool,
    _subscriptions: Vec<Subscription>,
}

impl Sidebar {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("搜索会话或项目…")
        });
        let _subscriptions = vec![cx.subscribe_in(
            &search_input,
            window,
            |_this: &mut Self, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            },
        )];
        Self {
            view: SidebarView::Group,
            sessions: vec![],
            projects: vec![],
            manual_projects: vec![],
            active: None,
            search_open: false,
            search_input,
            expanded: std::collections::HashSet::new(),
            archived_open: false,
            _subscriptions,
        }
    }

    pub fn set_state(
        &mut self,
        sessions: Vec<SidebarSession>,
        projects: Vec<String>,
        manual_projects: Vec<String>,
        active: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.sessions = sessions;
        self.projects = projects;
        self.manual_projects = manual_projects;
        self.active = active;
        cx.notify();
    }

    pub fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_open = true;
        self.search_input.update(cx, |input, cx| {
            input.focus(window, cx);
        });
        cx.notify();
    }

    fn query(&self, cx: &App) -> String {
        self.search_input.read(cx).value().to_lowercase()
    }

    fn matches(&self, query: &str, text: &str) -> bool {
        query.is_empty() || text.to_lowercase().contains(query)
    }

    /// 自测用：项目列表。
    pub fn debug_projects(&self) -> &[String] {
        &self.projects
    }

    /// 自测用：某项目下的会话 id。
    pub fn debug_project_sessions(&self, path: &str) -> Vec<String> {
        self.sessions
            .iter()
            .filter(|s| s.cwd.display().to_string() == path)
            .map(|s| s.id.clone())
            .collect()
    }

    fn render_action_rows(&self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .p_2()
            .gap_1()
            .child(
                h_flex()
                    .id("new-task")
                    .gap_2()
                    .px_2()
                    .py_1()
                    .w_full()
                    .rounded(cx.theme().radius)
                    .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
                    .on_click(cx.listener(|_, _, _, cx| {
                        cx.emit(SidebarEvent::NewTask);
                    }))
                    .child(Icon::new(IconName::Plus).size_4())
                    .child(div().text_sm().flex_1().child("新建任务"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("Ctrl+N"),
                    ),
            )
            .child(
                h_flex()
                    .id("open-search")
                    .gap_2()
                    .px_2()
                    .py_1()
                    .w_full()
                    .rounded(cx.theme().radius)
                    .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_search(window, cx);
                    }))
                    .child(Icon::new(IconName::Search).size_4())
                    .child(div().text_sm().flex_1().child("搜索"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("Ctrl+K"),
                    ),
            )
            .when(self.search_open, |this| {
                this.child(
                    div()
                        .key_context("search")
                        .on_action(cx.listener(|this, _: &crate::CloseSearch, window, cx| {
                            this.search_open = false;
                            this.search_input.update(cx, |input, cx| {
                                input.set_value("", window, cx);
                            });
                            cx.notify();
                        }))
                        .child(Input::new(&self.search_input).small()),
                )
            })
            .child(
                // 分段控件：分组 | 项目
                h_flex()
                    .w_full()
                    .gap_1()
                    .mt_1()
                    .p_0p5()
                    .rounded(cx.theme().radius)
                    .bg(cx.theme().accent.opacity(0.4))
                    .children([SidebarView::Group, SidebarView::Project].map(|view| {
                        let selected = self.view == view;
                        div()
                            .id(match view {
                                SidebarView::Group => "tab-group",
                                SidebarView::Project => "tab-project",
                            })
                            .flex_1()
                            .text_center()
                            .text_xs()
                            .py_0p5()
                            .rounded_sm()
                            .cursor_pointer()
                            .when(selected, |this| {
                                this.bg(cx.theme().background)
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.view = view;
                                cx.notify();
                            }))
                            .child(match view {
                                SidebarView::Group => "分组",
                                SidebarView::Project => "项目",
                            })
                    })),
            )
            .into_any_element()
    }

    fn render_session_row(&self, ix: usize, show_time: bool, cx: &mut Context<Self>) -> AnyElement {
        let session = &self.sessions[ix];
        let id = session.id.clone();
        let active = self.active.as_deref() == Some(session.id.as_str());
        let status_color = if session.running {
            Some(cx.theme().success)
        } else if session.waiting_approval {
            Some(cx.theme().warning)
        } else {
            None
        };

        let mut row = h_flex()
            .id(("session", ix))
            .mx_2()
            .px_2()
            .py_1()
            .gap_2()
            .rounded(cx.theme().radius)
            .when(session.archived, |this| {
                this.text_color(cx.theme().muted_foreground)
            })
            .when(active, |this| this.bg(cx.theme().accent))
            .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.emit(SidebarEvent::Select(id.clone()));
            }))
            .child(
                div()
                    .text_sm()
                    .flex_1()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(session.title.clone()),
            )
            .when_some(status_color, |this, color| {
                this.child(div().size_2().rounded_full().bg(color))
            })
            .when_some(session.added, |this, added| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().success)
                        .child(format!("+{added}")),
                )
            })
            .when_some(session.removed, |this, removed| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().danger)
                        .child(format!("-{removed}")),
                )
            });
        if show_time {
            row = row.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(session.updated_at.relative()),
            );
        } else {
            row = row
                .child({
                    let id = session.id.clone();
                    let pinned = session.pinned;
                    Button::new(("pin", ix))
                        .ghost()
                        .xsmall()
                        .icon(if pinned {
                            IconName::StarFill
                        } else {
                            IconName::Star
                        })
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(SidebarEvent::SetPinned(id.clone(), !pinned));
                        }))
                })
                .child({
                    let id = session.id.clone();
                    let archived = session.archived;
                    Button::new(("archive", ix))
                        .ghost()
                        .xsmall()
                        .icon(if archived {
                            IconName::Undo2
                        } else {
                            IconName::Inbox
                        })
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(SidebarEvent::SetArchived(id.clone(), !archived));
                        }))
                });
        }
        row.into_any_element()
    }

    fn render_group_view(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let query = self.query(cx);
        let filtered: Vec<usize> = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| self.matches(&query, &s.title))
            .map(|(ix, _)| ix)
            .collect();

        let mut out: Vec<AnyElement> = vec![];
        let mut section = |pinned: bool, archived: bool, title: &'static str, out: &mut Vec<AnyElement>| {
            let rows: Vec<AnyElement> = filtered
                .iter()
                .copied()
                .filter(|ix| {
                        let s = &self.sessions[*ix];
                        s.pinned == pinned && s.archived == archived
                    })
                    .map(|ix| self.render_session_row(ix, false, cx))
                    .collect();
            if !rows.is_empty() {
                out.push(
                    div()
                        .px_3()
                        .py_1()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(title)
                        .into_any_element(),
                );
                out.extend(rows);
            }
        };
        section(true, false, "置顶", &mut out);
        section(false, false, "任务", &mut out);

        let archived_count = filtered
            .iter()
            .filter(|ix| self.sessions[**ix].archived)
            .count();
        if archived_count > 0 {
            out.push(
                h_flex()
                    .id("archived-toggle")
                    .mx_2()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .rounded(cx.theme().radius)
                    .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.archived_open = !this.archived_open;
                        cx.notify();
                    }))
                    .child(
                        Icon::new(if self.archived_open {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .size_4()
                        .text_color(cx.theme().muted_foreground),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("已归档（{archived_count}）")),
                    )
                    .into_any_element(),
            );
            if self.archived_open {
                out.extend(
                    filtered
                        .iter()
                        .copied()
                        .filter(|ix| self.sessions[*ix].archived)
                        .map(|ix| self.render_session_row(ix, false, cx)),
                );
            }
        }
        out
    }

    fn render_project_view(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let query = self.query(cx);
        let mut out: Vec<AnyElement> = vec![
            h_flex()
                .px_3()
                .py_1()
                .child(
                    div()
                        .text_xs()
                        .flex_1()
                        .text_color(cx.theme().muted_foreground)
                        .child("项目"),
                )
                .child(
                    Button::new("add-project")
                        .ghost()
                        .xsmall()
                        .icon(IconName::Plus)
                        .on_click(cx.listener(|_, _, _, cx| {
                            cx.emit(SidebarEvent::AddProject);
                        })),
                )
                .into_any_element(),
        ];

        for (p_ix, project) in self.projects.iter().enumerate() {
            let name = std::path::Path::new(project)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| project.clone());
            if !self.matches(&query, &name) && !self.matches(&query, project) {
                continue;
            }
            let expanded = self.expanded.contains(project);
            let project_path = project.clone();

            // 该项目下的会话：未归档在前按 updated 倒序，归档的灰色垫底
            let mut sessions: Vec<usize> = self
                .sessions
                .iter()
                .enumerate()
                .filter(|(_, s)| s.cwd.display().to_string() == *project)
                .map(|(ix, _)| ix)
                .collect();
            sessions.sort_by_key(|ix| {
                let s = &self.sessions[*ix];
                (s.archived, std::cmp::Reverse(s.updated_at))
            });

            let removable = sessions.is_empty() && self.manual_projects.contains(project);
            let has_sessions = !sessions.is_empty();

            let mut row = h_flex()
                .id(("project", p_ix))
                .mx_2()
                .px_2()
                .py_1()
                .gap_2()
                .rounded(cx.theme().radius)
                .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
                .child(
                    Icon::new(if has_sessions || self.manual_projects.contains(project) {
                        IconName::Folder
                    } else {
                        IconName::FolderClosed
                    })
                    .size_4()
                    .text_color(cx.theme().muted_foreground),
                )
                .child(
                    div()
                        .text_sm()
                        .flex_1()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child(name),
                );
            if removable {
                row = row.child({
                    let path = project.clone();
                    Button::new(("remove-project", p_ix))
                        .ghost()
                        .xsmall()
                        .icon(IconName::Close)
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(SidebarEvent::RemoveProject(path.clone()));
                        }))
                });
            }
            row = row
                .child(
                    Icon::new(if expanded {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .size_4()
                    .text_color(cx.theme().muted_foreground),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if !this.expanded.remove(&project_path) {
                        this.expanded.insert(project_path.clone());
                    }
                    cx.notify();
                }));
            out.push(row.into_any_element());

            if expanded {
                for ix in sessions {
                    out.push(
                        div().pl_4().child(self.render_session_row(ix, true, cx)).into_any_element(),
                    );
                }
                if has_sessions {
                    let path = project.clone();
                    out.push(
                        div()
                            .id(("project-new-task", p_ix))
                            .pl_8()
                            .pr_2()
                            .py_0p5()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .cursor_pointer()
                            .hover(|this| this.text_color(cx.theme().foreground))
                            .child("+ 该项目下新建任务")
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(SidebarEvent::NewTaskInProject(path.clone()));
                            }))
                            .into_any_element(),
                    );
                }
            }
        }
        out
    }
}

impl Render for Sidebar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match self.view {
            SidebarView::Group => self.render_group_view(cx),
            SidebarView::Project => self.render_project_view(cx),
        };

        v_flex()
            .size_full()
            .bg(cx.theme().sidebar)
            .border_r_1()
            .border_color(cx.theme().border)
            .child(self.render_action_rows(cx))
            .child(
                div()
                    .id("session-list")
                    .flex_1()
                    .overflow_y_scroll()
                    .child(v_flex().gap_1().py_1().children(content)),
            )
            .child(
                div().border_t_1().border_color(cx.theme().border).p_2().child(
                    h_flex()
                        .id("settings-entry")
                        .gap_2()
                        .px_2()
                        .py_1()
                        .rounded(cx.theme().radius)
                        .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
                        .on_click(cx.listener(|_, _, _, cx| {
                            cx.emit(SidebarEvent::OpenSettings);
                        }))
                        .child(Icon::new(IconName::Settings).size_4())
                        .child(div().text_sm().child("设置")),
                ),
            )
    }
}
