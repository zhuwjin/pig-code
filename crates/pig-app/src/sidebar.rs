use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
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
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SidebarView {
    Group,
    Workspace,
}

#[derive(Clone)]
pub enum SidebarEvent {
    Select(String),
    NewTask,
    /// 在指定工作区目录下新建任务（hero 预设 cwd）
    NewTaskInWorkspace(String),
    SetPinned(String, bool),
    SetArchived(String, bool),
    RemoveWorkspace(String),
    /// 重命名工作区显示名；None 恢复默认目录名
    RenameWorkspace(String, Option<String>),
    OpenSettings,
}

impl EventEmitter<SidebarEvent> for Sidebar {}

/// 悬停标题跑马灯的进行状态
struct TitleMarquee {
    session_id: String,
    position: f32,
    forward: bool,
    hold_ticks: u8,
}

/// 跑马灯在两端的停留时长（16ms × 45 ≈ 0.7s）
const MARQUEE_HOLD_TICKS: u8 = 45;
/// 悬停后先静止片刻再开始滚动
const MARQUEE_START_TICKS: u8 = 30;

pub struct Sidebar {
    view: SidebarView,
    sessions: Vec<SidebarSession>,
    /// 工作区列表 = 可见手动工作区 ∪ 会话 cwd（AppView 已排序、已排除隐藏工作区）
    workspaces: Vec<String>,
    /// 工作区路径 → 用户自定义显示名
    aliases: std::collections::HashMap<String, String>,
    /// 正在重命名的工作区路径
    renaming: Option<String>,
    rename_input: Entity<InputState>,
    active: Option<String>,
    search_open: bool,
    search_input: Entity<InputState>,
    expanded: std::collections::HashSet<String>,
    archived_open: bool,
    /// 会话标题的横向滚动把手（悬停跑马灯用），key = 会话 id；
    /// render_session_row 只持 &self，故用 RefCell
    title_scrolls: std::cell::RefCell<std::collections::HashMap<String, ScrollHandle>>,
    marquee: Option<TitleMarquee>,
    _subscriptions: Vec<Subscription>,
}

impl Sidebar {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("搜索会话或工作区…"));
        let rename_input = cx.new(|cx| InputState::new(window, cx));
        let _subscriptions = vec![
            cx.subscribe_in(
                &search_input,
                window,
                |_this: &mut Self, _, event: &InputEvent, _, cx| {
                    if matches!(event, InputEvent::Change) {
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &rename_input,
                window,
                |this: &mut Self, _, event: &InputEvent, _, cx| {
                    if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                        this.commit_rename(cx);
                    }
                },
            ),
        ];
        Self {
            view: SidebarView::Group,
            sessions: vec![],
            workspaces: vec![],
            aliases: std::collections::HashMap::new(),
            renaming: None,
            rename_input,
            active: None,
            search_open: false,
            search_input,
            expanded: std::collections::HashSet::new(),
            archived_open: false,
            title_scrolls: std::cell::RefCell::new(std::collections::HashMap::new()),
            marquee: None,
            _subscriptions,
        }
    }

    pub fn set_state(
        &mut self,
        sessions: Vec<SidebarSession>,
        workspaces: Vec<String>,
        aliases: std::collections::HashMap<String, String>,
        active: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.sessions = sessions;
        self.workspaces = workspaces;
        self.aliases = aliases;
        self.active = active;
        self.title_scrolls
            .borrow_mut()
            .retain(|id, _| self.sessions.iter().any(|s| &s.id == id));
        if self
            .marquee
            .as_ref()
            .is_some_and(|m| !self.sessions.iter().any(|s| s.id == m.session_id))
        {
            self.marquee = None;
        }
        cx.notify();
    }

    pub fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_open = true;
        self.search_input.update(cx, |input, cx| {
            input.focus(window, cx);
        });
        cx.notify();
    }

    /// 悬停超宽标题：启动跑马灯，把文字缓慢滚到末尾再折返
    fn begin_title_marquee(&mut self, session_id: &str, cx: &mut Context<Self>) {
        let Some(handle) = self.title_scrolls.borrow().get(session_id).cloned() else {
            return;
        };
        // 未超宽（无横向溢出）不必滚动
        if f32::from(handle.max_offset().x) <= 1.0 {
            return;
        }
        if self
            .marquee
            .as_ref()
            .is_some_and(|m| m.session_id == session_id)
        {
            return;
        }
        self.marquee = Some(TitleMarquee {
            session_id: session_id.to_string(),
            position: 0.0,
            forward: true,
            hold_ticks: MARQUEE_START_TICKS,
        });
        let session_id = session_id.to_string();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(16))
                    .await;
                match this.update(cx, |this, cx| this.tick_title_marquee(&session_id, cx)) {
                    Ok(true) => {}
                    _ => break,
                }
            }
        })
        .detach();
    }

    fn end_title_marquee(&mut self, session_id: &str, cx: &mut Context<Self>) {
        if self
            .marquee
            .as_ref()
            .is_some_and(|m| m.session_id == session_id)
        {
            self.marquee = None;
        }
        if let Some(handle) = self.title_scrolls.borrow().get(session_id) {
            if handle.offset().x != px(0.) {
                handle.set_offset(point(px(0.), px(0.)));
            }
        }
        cx.notify();
    }

    /// 跑马灯推进一步；返回 false 表示循环该停了
    fn tick_title_marquee(&mut self, session_id: &str, cx: &mut Context<Self>) -> bool {
        if self
            .marquee
            .as_ref()
            .is_none_or(|m| m.session_id != session_id)
        {
            return false;
        }
        let Some(handle) = self.title_scrolls.borrow().get(session_id).cloned() else {
            self.marquee = None;
            return false;
        };
        let max = f32::from(handle.max_offset().x);
        if max <= 1.0 {
            handle.set_offset(point(px(0.), px(0.)));
            self.marquee = None;
            cx.notify();
            return false;
        }
        let marquee = self.marquee.as_mut().expect("checked above");
        if marquee.hold_ticks > 0 {
            marquee.hold_ticks -= 1;
            return true;
        }
        if marquee.forward {
            marquee.position += 1.5;
            if marquee.position >= max {
                marquee.position = max;
                marquee.forward = false;
                marquee.hold_ticks = MARQUEE_HOLD_TICKS;
            }
        } else {
            marquee.position -= 3.0;
            if marquee.position <= 0.0 {
                marquee.position = 0.0;
                marquee.forward = true;
                marquee.hold_ticks = MARQUEE_HOLD_TICKS;
            }
        }
        handle.set_offset(point(px(-marquee.position), px(0.)));
        cx.notify();
        true
    }

    fn query(&self, cx: &App) -> String {
        self.search_input.read(cx).value().to_lowercase()
    }

    fn matches(&self, query: &str, text: &str) -> bool {
        query.is_empty() || text.to_lowercase().contains(query)
    }

    /// 工作区显示名：优先用户别名，否则取目录名。
    fn workspace_name(&self, path: &str) -> String {
        self.aliases.get(path).cloned().unwrap_or_else(|| {
            std::path::Path::new(path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path.to_string())
        })
    }

    fn start_rename(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.workspace_name(&path);
        self.renaming = Some(path);
        self.rename_input.update(cx, |input, cx| {
            input.set_value(current, window, cx);
            input.focus(window, cx);
        });
        cx.notify();
    }

    fn commit_rename(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.renaming.take() else {
            return;
        };
        let value = self.rename_input.read(cx).value().trim().to_string();
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
        cx.notify();
    }

    /// 工作区行的选项菜单（行尾 “...” 按钮与右键共用）：
    /// 复制路径 / 重命名 / 移除工作区（从侧栏隐藏，会话数据保留）。
    fn workspace_menu(
        view: &WeakEntity<Self>,
        path: &str,
    ) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + Clone + 'static
    {
        let view = view.clone();
        let path = path.to_string();
        move |menu, _, _| {
            let copy_path = path.clone();
            let rename_view = view.clone();
            let rename_path = path.clone();
            let remove_view = view.clone();
            let remove_path = path.clone();
            menu.item(PopupMenuItem::new("复制路径").on_click(move |_, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(copy_path.clone()));
            }))
            .item(PopupMenuItem::new("重命名").on_click(move |_, window, cx| {
                let _ = rename_view.update(cx, |this, cx| {
                    this.start_rename(rename_path.clone(), window, cx);
                });
            }))
            .separator()
            .item(PopupMenuItem::new("移除工作区").on_click(move |_, _, cx| {
                let _ = remove_view.update(cx, |_, cx| {
                    cx.emit(SidebarEvent::RemoveWorkspace(remove_path.clone()));
                });
            }))
        }
    }

    /// 自测用：工作区列表。
    pub fn debug_workspaces(&self) -> &[String] {
        &self.workspaces
    }

    /// 自测用：某工作区下的会话 id。
    pub fn debug_workspace_sessions(&self, path: &str) -> Vec<String> {
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
                // 分段控件：分组 | 工作区
                h_flex()
                    .w_full()
                    .gap_1()
                    .mt_1()
                    .p_0p5()
                    .rounded(cx.theme().radius)
                    .bg(cx.theme().accent.opacity(0.4))
                    .children([SidebarView::Group, SidebarView::Workspace].map(|view| {
                        let selected = self.view == view;
                        div()
                            .id(match view {
                                SidebarView::Group => "tab-group",
                                SidebarView::Workspace => "tab-workspace",
                            })
                            .flex_1()
                            .text_center()
                            .text_xs()
                            .py_0p5()
                            .rounded_sm()
                            .cursor_pointer()
                            .when(selected, |this| this.bg(cx.theme().background))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.view = view;
                                cx.notify();
                            }))
                            .child(match view {
                                SidebarView::Group => "分组",
                                SidebarView::Workspace => "工作区",
                            })
                    })),
            )
            .into_any_element()
    }

    /// 用文本系统量出标题单行渲染宽度。
    ///
    /// gpui 的文本测量会把宽度钳制进可用空间，导致 ScrollHandle 感知不到
    /// 溢出（实测 max_offset 恒为 0）；给内容显式真实宽度后滚动机制才生效。
    fn measure_title_width(title: &str, window: &Window, cx: &App) -> Pixels {
        let font_size = rems(0.875).to_pixels(window.rem_size());
        let font = Font {
            family: cx.theme().font_family.clone(),
            ..Font::default()
        };
        window
            .text_system()
            .shape_line(
                SharedString::from(title.to_string()),
                font_size,
                &[TextRun {
                    len: title.len(),
                    font,
                    color: black(),
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                }],
                None,
            )
            .width
    }

    fn render_session_row(
        &self,
        window: &Window,
        ix: usize,
        show_time: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
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
            .child({
                let title_handle = self
                    .title_scrolls
                    .borrow_mut()
                    .entry(session.id.clone())
                    .or_insert_with(ScrollHandle::new)
                    .clone();
                let hover_id = session.id.clone();
                // 显式真实宽度（+2px 余量防字宽取整误差），把溢出撑给 ScrollHandle
                let title_width = Self::measure_title_width(&session.title, window, cx) + px(2.);
                div()
                    .id(("session-title", ix))
                    .text_sm()
                    .flex_1()
                    // 双轴滚动而非 overflow_x_scroll：gpui 对单轴滚动容器会把另一轴的
                    // 滚轮增量折进来，双轴下纵向滚轮原样冒泡给会话列表，互不影响
                    .overflow_scroll()
                    .whitespace_nowrap()
                    .track_scroll(&title_handle)
                    .on_hover(cx.listener(move |this, hovered, _, cx| {
                        if *hovered {
                            this.begin_title_marquee(&hover_id, cx);
                        } else {
                            this.end_title_marquee(&hover_id, cx);
                        }
                    }))
                    .child(div().w(title_width).child(session.title.clone()))
            })
            .when_some(status_color, |this, color| {
                this.child(div().size_2().rounded_full().bg(color))
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

    fn render_group_view(&self, window: &Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let query = self.query(cx);
        let filtered: Vec<usize> = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| self.matches(&query, &s.title))
            .map(|(ix, _)| ix)
            .collect();

        let mut out: Vec<AnyElement> = vec![];
        let mut section =
            |pinned: bool, archived: bool, title: &'static str, out: &mut Vec<AnyElement>| {
                let rows: Vec<AnyElement> = filtered
                    .iter()
                    .copied()
                    .filter(|ix| {
                        let s = &self.sessions[*ix];
                        s.pinned == pinned && s.archived == archived
                    })
                    .map(|ix| self.render_session_row(window, ix, false, cx))
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
                        .map(|ix| self.render_session_row(window, ix, false, cx)),
                );
            }
        }
        out
    }

    fn render_workspace_view(&self, window: &Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let query = self.query(cx);
        let mut out: Vec<AnyElement> = vec![
            div()
                .px_3()
                .py_1()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child("工作区")
                .into_any_element(),
        ];

        for (p_ix, workspace) in self.workspaces.iter().enumerate() {
            let name = self.workspace_name(workspace);
            if !self.matches(&query, &name) && !self.matches(&query, workspace) {
                continue;
            }
            let expanded = self.expanded.contains(workspace);
            let renaming = self.renaming.as_deref() == Some(workspace.as_str());
            let workspace_path = workspace.clone();

            // 该工作区下的会话：未归档在前按 updated 倒序，归档的灰色垫底
            let mut sessions: Vec<usize> = self
                .sessions
                .iter()
                .enumerate()
                .filter(|(_, s)| s.cwd.display().to_string() == *workspace)
                .map(|(ix, _)| ix)
                .collect();
            sessions.sort_by_key(|ix| {
                let s = &self.sessions[*ix];
                (s.archived, std::cmp::Reverse(s.updated_at))
            });

            let group_name: SharedString = format!("workspace-row-{p_ix}").into();
            let mut row = h_flex()
                .id(("workspace", p_ix))
                .group(group_name.clone())
                .mx_2()
                .px_2()
                .py_1()
                .gap_2()
                .rounded(cx.theme().radius)
                .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
                .child(
                    Icon::new(if expanded {
                        IconName::FolderOpen
                    } else {
                        IconName::FolderClosed
                    })
                    .size_4()
                    .text_color(cx.theme().muted_foreground),
                );
            if renaming {
                row = row.child(div().flex_1().child(Input::new(&self.rename_input).small()));
            } else {
                row = row.child(
                    div()
                        .text_sm()
                        .flex_1()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child(name),
                );
            }
            if renaming {
                out.push(row.into_any_element());
            } else {
                let menu = Self::workspace_menu(&cx.entity().downgrade(), workspace);
                let new_task_path = workspace.clone();
                // 悬停显示：选项（...）与该工作区下新建任务（+）
                let row = row
                    .child(
                        div()
                            .invisible()
                            .group_hover(group_name.clone(), |this| this.visible())
                            .child(
                                Button::new(("workspace-menu", p_ix))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Ellipsis)
                                    .dropdown_menu(menu.clone()),
                            ),
                    )
                    .child(
                        div()
                            .invisible()
                            .group_hover(group_name.clone(), |this| this.visible())
                            .child(
                                Button::new(("workspace-add", p_ix))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Plus)
                                    .on_click(cx.listener(move |_, _, _, cx| {
                                        cx.emit(SidebarEvent::NewTaskInWorkspace(
                                            new_task_path.clone(),
                                        ));
                                    })),
                            ),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.expanded.remove(&workspace_path) {
                            this.expanded.insert(workspace_path.clone());
                        }
                        cx.notify();
                    }))
                    .context_menu(menu);
                out.push(row.into_any_element());
            }

            if expanded {
                for ix in sessions {
                    out.push(
                        div()
                            .pl_4()
                            .child(self.render_session_row(window, ix, true, cx))
                            .into_any_element(),
                    );
                }
            }
        }
        out
    }
}

impl Render for Sidebar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match self.view {
            SidebarView::Group => self.render_group_view(window, cx),
            SidebarView::Workspace => self.render_workspace_view(window, cx),
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
                div()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .p_2()
                    .child(
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
