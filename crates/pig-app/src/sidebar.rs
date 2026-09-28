use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::spinner::Spinner;
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
    /// 会话手动重命名（此后自动命名不再覆盖）
    RenameSession(String, String),
    /// 删除会话（清库 + rollout 文件，不可恢复）
    DeleteSession(String),
    RemoveWorkspace(String),
    /// 重命名工作区显示名；None 恢复默认目录名
    RenameWorkspace(String, Option<String>),
    OpenSettings,
}

/// 行内重命名的目标：工作区或会话（共用一个输入框实体）
#[derive(Clone, PartialEq)]
enum RenameTarget {
    Workspace(String),
    Session(String),
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
/// 工作区展开后每页展示的会话条数（默认一页，展开更多每次 +1 页）
const WORKSPACE_PAGE_SIZE: usize = 5;

pub struct Sidebar {
    view: SidebarView,
    sessions: Vec<SidebarSession>,
    /// 工作区列表 = 可见手动工作区 ∪ 会话 cwd（AppView 已排序、已排除隐藏工作区）
    workspaces: Vec<String>,
    /// 工作区路径 → 用户自定义显示名
    aliases: std::collections::HashMap<String, String>,
    /// 正在重命名的目标（工作区/会话共用输入框）
    renaming: Option<RenameTarget>,
    rename_input: Entity<InputState>,
    active: Option<String>,
    search_open: bool,
    search_input: Entity<InputState>,
    expanded: std::collections::HashSet<String>,
    archived_open: bool,
    /// 悬停中的工作区行路径：行尾浮层（名字渐隐 + 按钮）仅悬停时渲染占位，
    /// 未悬停时名字用满行宽、不裁减
    hovered_workspace: Option<String>,
    /// 悬停中的会话行 id：渐隐底色与行尾按钮（分组视图）显隐跟随行悬停
    hovered_session: Option<String>,
    /// 工作区会话分页：路径 → 当前展示条数（缺省 = WORKSPACE_PAGE_SIZE）
    workspace_shown: std::collections::HashMap<String, usize>,
    /// 会话标题的横向滚动把手（悬停跑马灯用），key = 会话 id；
    /// render_session_row 只持 &self，故用 RefCell
    title_scrolls: std::cell::RefCell<std::collections::HashMap<String, ScrollHandle>>,
    marquee: Option<TitleMarquee>,
    /// dock 侧栏内容锚定宽（AppView 每次 render 推送）：开合动画期间 dock 实
    /// 宽被补间，内容固定该宽并锚定分隔线一侧，滑出/滑入而非压缩重排
    panel_width: f32,
    focus_handle: FocusHandle,
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
            hovered_workspace: None,
            hovered_session: None,
            workspace_shown: std::collections::HashMap::new(),
            title_scrolls: std::cell::RefCell::new(std::collections::HashMap::new()),
            marquee: None,
            // 与 AppView 的侧栏初始宽一致；AppView 每次 render 都会推送，这里
            // 只是首帧前的兜底
            panel_width: 220.,
            focus_handle: cx.focus_handle(),
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
        self.title_scrolls.borrow_mut().retain(|id, _| {
            // 置顶区行的键带 "pinned-" 前缀（同一会话在两个视图的键不冲突）
            let sid = id.strip_prefix("pinned-").unwrap_or(id.as_str());
            self.sessions.iter().any(|s| s.id == sid)
        });
        self.workspace_shown
            .retain(|path, _| self.workspaces.iter().any(|w| w == path));
        if self.marquee.as_ref().is_some_and(|m| {
            let sid = m
                .session_id
                .strip_prefix("pinned-")
                .unwrap_or(m.session_id.as_str());
            !self.sessions.iter().any(|s| s.id == sid)
        }) {
            self.marquee = None;
        }
        cx.notify();
    }

    /// AppView 推送侧栏展开目标宽（拖宽/补钳后可能变化）：值变才 notify，
    /// 开合动画帧不额外扰动侧栏重渲染
    pub fn set_panel_width(&mut self, width: f32, cx: &mut Context<Self>) {
        if self.panel_width != width {
            self.panel_width = width;
            cx.notify();
        }
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
        self.renaming = Some(RenameTarget::Workspace(path));
        self.rename_input.update(cx, |input, cx| {
            input.set_value(current, window, cx);
            input.focus(window, cx);
        });
        cx.notify();
    }

    fn start_session_rename(
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

    fn commit_rename(&mut self, cx: &mut Context<Self>) {
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

    /// 会话行的右键菜单：重命名 / 置顶 / 归档 / 删除（清库+rollout，不可恢复）
    fn session_menu(
        view: &WeakEntity<Self>,
        session: &SidebarSession,
    ) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + Clone + 'static
    {
        let view = view.clone();
        let id = session.id.clone();
        let title = session.title.clone();
        let pinned = session.pinned;
        let archived = session.archived;
        move |menu, _, _| {
            let rename_view = view.clone();
            let rename_id = id.clone();
            let rename_title = title.clone();
            let pin_view = view.clone();
            let pin_id = id.clone();
            let archive_view = view.clone();
            let archive_id = id.clone();
            let delete_view = view.clone();
            let delete_id = id.clone();
            menu.item(PopupMenuItem::new("重命名").on_click(move |_, window, cx| {
                let _ = rename_view.update(cx, |this, cx| {
                    this.start_session_rename(rename_id.clone(), rename_title.clone(), window, cx);
                });
            }))
            .item(
                PopupMenuItem::new(if pinned { "取消置顶" } else { "置顶" }).on_click(
                    move |_, _, cx| {
                        let _ = pin_view.update(cx, |_, cx| {
                            cx.emit(SidebarEvent::SetPinned(pin_id.clone(), !pinned));
                        });
                    },
                ),
            )
            .item(
                PopupMenuItem::new(if archived { "取消归档" } else { "归档" }).on_click(
                    move |_, _, cx| {
                        let _ = archive_view.update(cx, |_, cx| {
                            cx.emit(SidebarEvent::SetArchived(archive_id.clone(), !archived));
                        });
                    },
                ),
            )
            .separator()
            .item(PopupMenuItem::new("删除会话").on_click(move |_, _, cx| {
                let _ = delete_view.update(cx, |_, cx| {
                    cx.emit(SidebarEvent::DeleteSession(delete_id.clone()));
                });
            }))
        }
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

    /// 会话标题端部的渐隐条：base 为行背景实色（常态 sidebar / 选中 accent），
    /// tint 为悬停叠加层（accent 60%）；叠加后与行背景合成一致，尾端无色差
    fn title_fade(leading: bool, base: Hsla, tint: Option<Hsla>) -> Div {
        let (from, to) = if leading {
            (base, base.opacity(0.))
        } else {
            (base.opacity(0.), base)
        };
        let fade = div()
            .absolute()
            .top_0()
            .bottom_0()
            .when(leading, |this| this.left_0())
            .when(!leading, |this| this.right_0())
            .w(px(20.))
            .bg(linear_gradient(
                90.,
                linear_color_stop(from, 0.),
                linear_color_stop(to, 1.),
            ));
        match tint {
            Some(tint) => {
                let (from, to) = if leading {
                    (tint, tint.opacity(0.))
                } else {
                    (tint.opacity(0.), tint)
                };
                fade.child(div().size_full().bg(linear_gradient(
                    90.,
                    linear_color_stop(from, 0.),
                    linear_color_stop(to, 1.),
                )))
            }
            None => fade,
        }
    }

    /// 会话标题区：横向滚动（悬停跑马灯）+ 两端渐隐。key 为 title_scrolls
    /// 的键：普通会话行用会话 id，置顶区行用 "pinned-{id}"（同一会话在两
    /// 种行的滚动状态互不干扰）
    fn render_title_scroll(
        &self,
        key: &str,
        element_id: impl Into<ElementId>,
        title: &str,
        fade_base: Hsla,
        fade_tint: Option<Hsla>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let title_handle = self
            .title_scrolls
            .borrow_mut()
            .entry(key.to_string())
            .or_insert_with(ScrollHandle::new)
            .clone();
        let hover_key = key.to_string();
        // 显式真实宽度（+2px 余量防字宽取整误差），把溢出撑给 ScrollHandle
        let title_width = Self::measure_title_width(title, window, cx) + px(2.);
        // 渐隐显隐跟滚动位置（thread_view 思考滚动行同款）：右端还有未露出
        // 的文字才渐隐，跑马灯滚出开头后左端也渐隐
        let max = title_handle.max_offset().x;
        let offset = title_handle.offset().x;
        let hides_leading = max > px(1.) && offset < px(-1.);
        let hides_trailing = max > px(1.) && offset > px(1.) - max;
        div()
            .relative()
            .flex_1()
            .min_w_0()
            .child(
                div()
                    .id(element_id)
                    .text_sm()
                    .w_full()
                    // 双轴滚动而非 overflow_x_scroll：gpui 对单轴滚动容器会把另一轴的
                    // 滚轮增量折进来，双轴下纵向滚轮原样冒泡给会话列表，互不影响
                    .overflow_scroll()
                    .whitespace_nowrap()
                    .track_scroll(&title_handle)
                    .on_hover(cx.listener(move |this, hovered, _, cx| {
                        if *hovered {
                            this.begin_title_marquee(&hover_key, cx);
                        } else {
                            this.end_title_marquee(&hover_key, cx);
                        }
                    }))
                    .child(div().w(title_width).child(title.to_string())),
            )
            .when(hides_leading, |this| {
                this.child(Self::title_fade(true, fade_base, fade_tint))
            })
            .when(hides_trailing, |this| {
                this.child(Self::title_fade(false, fade_base, fade_tint))
            })
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
        // 状态指示：待审批黄点；运行中不画点，转圈占用行尾时间槽（见下）
        let status_color = if session.waiting_approval {
            Some(cx.theme().warning)
        } else {
            None
        };
        let renaming = self.renaming == Some(RenameTarget::Session(session.id.clone()));
        let hovered = self.hovered_session.as_deref() == Some(session.id.as_str());
        // 渐隐底色 = 行背景：选中与悬停同为 sidebar + accent 60%，常态 sidebar
        let (fade_base, fade_tint) = if hovered || active {
            (cx.theme().sidebar, Some(cx.theme().accent.opacity(0.6)))
        } else {
            (cx.theme().sidebar, None)
        };

        let hover_id = session.id.clone();
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
            .when(active, |this| this.bg(cx.theme().accent.opacity(0.6)))
            .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.emit(SidebarEvent::Select(id.clone()));
            }))
            .on_hover(cx.listener(move |this, is_hovered: &bool, _, cx| {
                let was = this.hovered_session.as_deref() == Some(hover_id.as_str());
                if *is_hovered != was {
                    this.hovered_session = (*is_hovered).then(|| hover_id.clone());
                    cx.notify();
                }
            }));
        if renaming {
            // 行内重命名：只留输入框（回车/失焦提交，空值取消）
            row = row.child(div().flex_1().child(Input::new(&self.rename_input).small()));
            return row.into_any_element();
        }
        row = row
            .child(self.render_title_scroll(
                &session.id,
                ("session-title", ix),
                &session.title,
                fade_base,
                fade_tint,
                window,
                cx,
            ))
            .when_some(status_color, |this, color| {
                this.child(div().size_2().rounded_full().bg(color))
            });
        if session.running && !show_time {
            // 分组视图无时间槽：运行中的转圈跟在标题后（原绿点位）
            row = row.child(Spinner::new().xsmall().color(cx.theme().muted_foreground));
        }
        if show_time && !hovered {
            // 悬停时行尾让位给置顶/归档按钮；运行中时间换成转圈
            row = row.child(if session.running {
                Spinner::new()
                    .xsmall()
                    .color(cx.theme().muted_foreground)
                    .into_any_element()
            } else {
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(session.updated_at.relative())
                    .into_any_element()
            });
        }
        if hovered {
            // 悬停才渲染置顶/归档按钮：让出的宽度归标题，此时才裁减文字
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
        // 右键菜单：重命名 / 置顶 / 归档 / 删除
        let menu = Self::session_menu(&cx.entity().downgrade(), session);
        row.context_menu(menu).into_any_element()
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

    /// 工作区视图置顶区的会话行：标题 + 时间一行，所属工作区一行（置顶会话
    /// 跨工作区集中展示，需标注归属）
    fn render_workspace_pinned_row(
        &self,
        window: &Window,
        ix: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let session = &self.sessions[ix];
        let id = session.id.clone();
        let active = self.active.as_deref() == Some(session.id.as_str());
        let hovered = self.hovered_session.as_deref() == Some(session.id.as_str());
        // 状态指示：待审批黄点；运行中不画点，转圈占用行尾时间槽（见下）
        let status_color = if session.waiting_approval {
            Some(cx.theme().warning)
        } else {
            None
        };
        let (fade_base, fade_tint) = if hovered || active {
            (cx.theme().sidebar, Some(cx.theme().accent.opacity(0.6)))
        } else {
            (cx.theme().sidebar, None)
        };
        let marquee_key = format!("pinned-{}", session.id);
        let workspace_name = self.workspace_name(&session.cwd.display().to_string());

        let mut line1 = h_flex()
            .gap_2()
            .child(self.render_title_scroll(
                &marquee_key,
                ("pinned-title", ix),
                &session.title,
                fade_base,
                fade_tint,
                window,
                cx,
            ))
            .when_some(status_color, |this, color| {
                this.child(div().size_2().rounded_full().bg(color))
            });
        if hovered {
            // 悬停才渲染置顶/归档按钮（与分组视图一致），时间让位不显示，
            // 此时标题才让宽裁减
            let pin_id = session.id.clone();
            let pinned = session.pinned;
            let archive_id = session.id.clone();
            let archived = session.archived;
            line1 = line1
                .child(
                    Button::new(("pin", ix))
                        .ghost()
                        .xsmall()
                        .icon(if pinned {
                            IconName::StarFill
                        } else {
                            IconName::Star
                        })
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(SidebarEvent::SetPinned(pin_id.clone(), !pinned));
                        })),
                )
                .child(
                    Button::new(("archive", ix))
                        .ghost()
                        .xsmall()
                        .icon(if archived {
                            IconName::Undo2
                        } else {
                            IconName::Inbox
                        })
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(SidebarEvent::SetArchived(archive_id.clone(), !archived));
                        })),
                );
        } else if session.running {
            // 运行中：时间槽显示转圈
            line1 = line1.child(Spinner::new().xsmall().color(cx.theme().muted_foreground));
        } else {
            line1 = line1.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(session.updated_at.relative()),
            );
        }

        let hover_id = session.id.clone();
        let row = div()
            .id(("pinned-session", ix))
            .mx_2()
            .px_2()
            .py_1()
            .rounded(cx.theme().radius)
            .when(active, |this| this.bg(cx.theme().accent.opacity(0.6)))
            .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.emit(SidebarEvent::Select(id.clone()));
            }))
            .on_hover(cx.listener(move |this, is_hovered: &bool, _, cx| {
                let was = this.hovered_session.as_deref() == Some(hover_id.as_str());
                if *is_hovered != was {
                    this.hovered_session = (*is_hovered).then(|| hover_id.clone());
                    cx.notify();
                }
            }))
            .child(
                v_flex()
                    .gap_0p5()
                    .child(line1)
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
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .child(workspace_name),
                            ),
                    ),
            );
        // 右键菜单：重命名 / 置顶 / 归档 / 删除
        let menu = Self::session_menu(&cx.entity().downgrade(), session);
        row.context_menu(menu).into_any_element()
    }

    fn render_workspace_view(&self, window: &Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let query = self.query(cx);
        let mut out: Vec<AnyElement> = vec![];

        // 置顶区：跨工作区集中展示置顶会话（归档的不显示），行内标注所属工作区
        let mut pinned: Vec<usize> = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| s.pinned && !s.archived && self.matches(&query, &s.title))
            .map(|(ix, _)| ix)
            .collect();
        pinned.sort_by_key(|ix| std::cmp::Reverse(self.sessions[*ix].updated_at));
        if !pinned.is_empty() {
            out.push(
                div()
                    .px_3()
                    .py_1()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("置顶")
                    .into_any_element(),
            );
            out.extend(
                pinned
                    .iter()
                    .map(|ix| self.render_workspace_pinned_row(window, *ix, cx)),
            );
        }

        out.push(
            div()
                .px_3()
                .py_1()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child("工作区")
                .into_any_element(),
        );

        for (p_ix, workspace) in self.workspaces.iter().enumerate() {
            let name = self.workspace_name(workspace);
            if !self.matches(&query, &name) && !self.matches(&query, workspace) {
                continue;
            }
            let expanded = self.expanded.contains(workspace);
            let renaming = self.renaming == Some(RenameTarget::Workspace(workspace.to_string()));
            let workspace_path = workspace.clone();

            // 该工作区下的会话：置顶（入置顶区）与归档的不在此列，按 updated 倒序
            let mut sessions: Vec<usize> = self
                .sessions
                .iter()
                .enumerate()
                .filter(|(_, s)| {
                    s.cwd.display().to_string() == *workspace && !s.pinned && !s.archived
                })
                .map(|(ix, _)| ix)
                .collect();
            sessions.sort_by_key(|ix| std::cmp::Reverse(self.sessions[*ix].updated_at));

            let hovered = self.hovered_workspace.as_deref() == Some(workspace.as_str());
            let mut row = h_flex()
                .id(("workspace", p_ix))
                .relative()
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
                let hover_path = workspace.clone();
                let mut row = row
                    .on_hover(cx.listener(move |this, is_hovered: &bool, _, cx| {
                        let was = this.hovered_workspace.as_deref() == Some(hover_path.as_str());
                        if *is_hovered != was {
                            this.hovered_workspace = (*is_hovered).then(|| hover_path.clone());
                            cx.notify();
                        }
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.expanded.remove(&workspace_path) {
                            this.expanded.insert(workspace_path.clone());
                        }
                        cx.notify();
                    }))
                    .context_menu(menu.clone());
                if hovered {
                    // 悬停才渲染行尾浮层：名字右端渐隐 + 选项（...）/新建任务（+）。
                    // 浮层绝对定位不占位，未悬停时名字用满行宽不被裁减；渐隐与
                    // 按钮托底各叠两层底色（sidebar + accent 60%），合成结果与行
                    // .hover() 背景一致，浮层盖住文字处无色差
                    let sidebar_bg = cx.theme().sidebar;
                    let hover_tint = cx.theme().accent.opacity(0.6);
                    row = row.child(
                        h_flex()
                            .absolute()
                            .right_2()
                            .top_0()
                            .bottom_0()
                            .child(
                                div()
                                    .w(px(20.))
                                    .h_full()
                                    .bg(linear_gradient(
                                        90.,
                                        linear_color_stop(sidebar_bg.opacity(0.), 0.),
                                        linear_color_stop(sidebar_bg, 1.),
                                    ))
                                    .child(div().size_full().bg(linear_gradient(
                                        90.,
                                        linear_color_stop(hover_tint.opacity(0.), 0.),
                                        linear_color_stop(hover_tint, 1.),
                                    ))),
                            )
                            .child(
                                div().h_full().bg(sidebar_bg).child(
                                    div().h_full().bg(hover_tint).child(
                                        h_flex()
                                            .h_full()
                                            .child(
                                                Button::new(("workspace-menu", p_ix))
                                                    .ghost()
                                                    .xsmall()
                                                    .icon(IconName::Ellipsis)
                                                    .dropdown_menu(menu.clone()),
                                            )
                                            .child(
                                                Button::new(("workspace-add", p_ix))
                                                    .ghost()
                                                    .xsmall()
                                                    .icon(IconName::Plus)
                                                    .on_click(cx.listener(
                                                        move |_, _, _, cx| {
                                                            cx.emit(
                                                                SidebarEvent::NewTaskInWorkspace(
                                                                    new_task_path.clone(),
                                                                ),
                                                            );
                                                        },
                                                    )),
                                            ),
                                    ),
                                ),
                            ),
                    );
                }
                out.push(row.into_any_element());
            }

            if expanded {
                // 分页：默认一页（5 条），展开更多每次 +1 页，收起回到一页
                let shown = self
                    .workspace_shown
                    .get(workspace)
                    .copied()
                    .unwrap_or(WORKSPACE_PAGE_SIZE);
                let total = sessions.len();
                for ix in sessions.iter().take(shown) {
                    out.push(
                        div()
                            // 缩进 24px：会话文字与工作区名字对齐（行 mx+px 16 +
                            // 图标 16 + gap 8 = 40）
                            .pl_6()
                            .child(self.render_session_row(window, *ix, true, cx))
                            .into_any_element(),
                    );
                }
                let can_more = shown < total;
                let can_collapse = shown > WORKSPACE_PAGE_SIZE;
                if can_more || can_collapse {
                    let mut controls = h_flex()
                        .mx_2()
                        .px_2()
                        .py_1()
                        .gap_3()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground);
                    if can_more {
                        let path = workspace.clone();
                        controls = controls.child(
                            h_flex()
                                .id(("workspace-more", p_ix))
                                .gap_1()
                                .px_1()
                                .rounded(cx.theme().radius)
                                .cursor_pointer()
                                .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    *this
                                        .workspace_shown
                                        .entry(path.clone())
                                        .or_insert(WORKSPACE_PAGE_SIZE) += WORKSPACE_PAGE_SIZE;
                                    cx.notify();
                                }))
                                .child(Icon::new(IconName::ChevronDown).size_3())
                                .child("展开更多"),
                        );
                    }
                    if can_more && can_collapse {
                        controls = controls.child("·");
                    }
                    if can_collapse {
                        let path = workspace.clone();
                        controls = controls.child(
                            h_flex()
                                .id(("workspace-collapse", p_ix))
                                .gap_1()
                                .px_1()
                                .rounded(cx.theme().radius)
                                .cursor_pointer()
                                .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.workspace_shown.remove(&path);
                                    cx.notify();
                                }))
                                .child(Icon::new(IconName::ChevronUp).size_3())
                                .child("收起"),
                        );
                    }
                    out.push(div().pl_6().child(controls).into_any_element());
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

        // 开合动画锚定层：dock_frame 自带 overflow_hidden，开合补间期间 dock 实
        // 宽小于内容宽；内容固定 panel_width 并右锚贴分隔线，收拢时整体左滑被
        // 裁而非压缩重排。稳态实宽 == panel_width，绝对定位子层正好铺满
        div()
            .relative()
            .size_full()
            .overflow_hidden()
            .child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .right_0()
                    .w(px(self.panel_width))
                    .child(
                        v_flex()
                            .size_full()
                            .bg(cx.theme().sidebar)
                            // 分隔线由 dock 把手自带线绘制：侧栏自画 border_r 会画在把手命中区
                            // 右侧（gpui-base 的 Side::Left 把手命中区停在分界线左侧），线上不可拖
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
                            ),
                    ),
            )
    }
}

// dock 面板能力：侧栏自绘全部 chrome，不要 dock 的标题栏/内边距
impl Focusable for Sidebar {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<gpui_kit::component::dock::PanelEvent> for Sidebar {}

impl gpui_kit::component::dock::BasePanel for Sidebar {
    fn panel_name(&self) -> &'static str {
        "sidebar"
    }
}

impl gpui_kit::component::dock::Panel for Sidebar {
    fn title_bar(&self, _: &App) -> bool {
        false
    }

    fn inner_padding(&self, _: &App) -> bool {
        false
    }
}
