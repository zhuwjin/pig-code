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

mod marquee;
mod menus;
mod rename;
mod views;

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
        div().relative().size_full().overflow_hidden().child(
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
