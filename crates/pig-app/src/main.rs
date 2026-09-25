mod agent_client;
mod composer;
mod review_panel;
mod settings;
mod sidebar;
mod thread_view;

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;

use gpui_kit::InteractiveElement as _;
use gpui_kit::assets::IconName as AssetsIconName;
use gpui_kit::base::GlobalState;
use gpui_kit::base::{Align, ElementExt as _, Placement, Positioner};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dock::{DockPlacement, panel_handle};
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Root, Sizable as _, StyledExt as _, Theme, ThemeMode,
    TitleBar, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use pig_protocol::{Event, ExecMode, SessionMeta};

gpui_kit::actions!(
    pig_app,
    [
        NewTask,
        FocusSearch,
        CloseSearch,
        CloseSettings,
        ToggleSidebar,
        ToggleChanges,
        ToggleBrowser,
        ToggleSideChat
    ]
);

/// 主题是否跟随系统外观：默认跟随；手动切换亮/暗后本次运行内固定为所选模式。
pub struct ThemeFollowSystem(pub bool);

impl Global for ThemeFollowSystem {}

/// 相对时间显示（刚刚 / N分钟 / N小时 / N天）
pub trait RelativeTime {
    fn relative(&self) -> String;
}

impl RelativeTime for u64 {
    fn relative(&self) -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let diff = now.saturating_sub(*self);
        match diff {
            0..=59 => "刚刚".to_string(),
            60..=3599 => format!("{} 分钟", diff / 60),
            3600..=86399 => format!("{} 小时", diff / 3600),
            _ => format!("{} 天", diff / 86400),
        }
    }
}

use crate::agent_client::AgentClient;
use crate::composer::{Composer, ComposerEvent, PendingApproval, PendingQuestion};
use crate::review_panel::{ReviewEvent, ReviewPanel};
use crate::settings::{SettingsEvent, SettingsView};
use crate::sidebar::{Sidebar, SidebarEvent, SidebarSession};
use crate::thread_view::{ThreadEvent, ThreadView};

struct SessionViews {
    thread: Entity<ThreadView>,
    review: Entity<ReviewPanel>,
}

/// 右侧面板 tab：当前只有"改动"，浏览器/终端/侧边聊天后续加。
#[derive(Clone, Copy, PartialEq, Eq)]
enum RightTab {
    Changes,
}

impl RightTab {
    fn label(self) -> &'static str {
        match self {
            Self::Changes => "改动",
        }
    }

    fn icon(self) -> AssetsIconName {
        match self {
            Self::Changes => AssetsIconName::GitBranch,
        }
    }
}

/// 三栏最小宽度（px）：侧栏 / 中心区 / 右面板。三者之和 = 窗口最小宽 960，
/// 保证钳制区间恒非空（gpui-base 的拖拽钳制只有 PANEL_MIN_SIZE(100) 一档，
/// 没有自定义区间 API，故在 render 里补钳）。
const SIDEBAR_MIN_W: f32 = 200.;
const CENTER_MIN_W: f32 = 480.;
const RIGHT_PANEL_MIN_W: f32 = 280.;

/// 三栏宽度钳制：展开的栏不低于各自最小值，且为中心区保留 CENTER_MIN_W
///（封顶 = 区域宽 - 中心最小值 - 对侧栏当前占位）。收起的栏占位为 0 不参与
/// 预算，其存储宽度原样保留，重开后由后续 render 再钳。区域宽为 0（首帧
/// 未测量）时不动作。顺序钳制——左先按右的当前占位钳，右再按钳后的左钳：
/// 单侧越界只拉回单侧，两侧同时越界（窗口缩到最小）左栏先让位，一遍收敛。
fn clamp_dock_widths(
    area: f32,
    left: f32,
    right: f32,
    left_open: bool,
    right_open: bool,
) -> (f32, f32) {
    if area <= 0. {
        return (left, right);
    }
    let cap = |min: f32, opposite_extent: f32| (area - CENTER_MIN_W - opposite_extent).max(min);
    let new_left = if left_open {
        left.clamp(
            SIDEBAR_MIN_W,
            cap(SIDEBAR_MIN_W, if right_open { right } else { 0. }),
        )
    } else {
        left
    };
    let new_right = if right_open {
        right.clamp(
            RIGHT_PANEL_MIN_W,
            cap(RIGHT_PANEL_MIN_W, if left_open { new_left } else { 0. }),
        )
    } else {
        right
    };
    (new_left, new_right)
}

/// dock 中心区面板：内容回读 AppView 构建（hero 或会话列）。
/// dock 持有 panel 实体，渲染时经 weak 引用调 AppView 的 render 辅助。
struct DockCenterPanel {
    app: WeakEntity<AppView>,
    focus_handle: FocusHandle,
    _app_observer: Subscription,
}

/// dock 右侧 dock 面板：tab 栏 + 改动/菜单页内容，同样回读 AppView。
struct DockRightPanel {
    app: WeakEntity<AppView>,
    focus_handle: FocusHandle,
    _app_observer: Subscription,
}

/// dock 皮肤用 `cached()` 包裹面板视图：缓存只在面板自身 notify 时失效。
/// 子实体（thread/review/composer）的 notify 会沿 dispatch 树把面板祖先标脏，
/// 自动失效；但纯 AppView 状态变化（hero↔会话切换、右侧 tab 开合）发生在祖先上，
/// 传不下来——观察 AppView，把它的 notify 转成面板自己的。
fn observe_app_notify<T: 'static>(
    app: &WeakEntity<AppView>,
    cx: &mut Context<T>,
) -> Option<Subscription> {
    app.upgrade()
        .map(|app| cx.observe(&app, |_, _, cx| cx.notify()))
}

macro_rules! impl_dock_panel {
    ($ty:ty, $name:literal) => {
        impl Focusable for $ty {
            fn focus_handle(&self, _: &App) -> FocusHandle {
                self.focus_handle.clone()
            }
        }

        impl EventEmitter<gpui_kit::component::dock::PanelEvent> for $ty {}

        impl gpui_kit::component::dock::BasePanel for $ty {
            fn panel_name(&self) -> &'static str {
                $name
            }
        }

        // chrome 全关：tab 栏/标题栏由我们自己画
        impl gpui_kit::component::dock::Panel for $ty {
            fn title_bar(&self, _: &App) -> bool {
                false
            }

            fn inner_padding(&self, _: &App) -> bool {
                false
            }
        }
    };
}

impl_dock_panel!(DockCenterPanel, "center");
impl_dock_panel!(DockRightPanel, "right-dock");

impl Render for DockCenterPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.app
            .update(cx, |app, cx| app.render_center(cx))
            .unwrap_or_else(|_| div().into_any_element())
    }
}

impl Render for DockRightPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.app
            .update(cx, |app, cx| app.render_right_dock_content(window, cx))
            .unwrap_or_else(|_| div().into_any_element())
    }
}

struct AppView {
    sidebar: Entity<Sidebar>,
    composer: Entity<Composer>,
    views: HashMap<String, SessionViews>,
    current: Option<String>,
    metas: Vec<SessionMeta>,
    running: HashSet<String>,
    /// 已删除的会话 id：迟到事件过滤用（id 含时间戳不复用，无需清理）
    deleted_sessions: HashSet<String>,
    approval_pending: HashSet<String>,
    /// 待审批详情（审批条内容）：决议/回合结束时清除
    pending_approvals: HashMap<String, PendingApproval>,
    /// 待回答的结构化提问（问题条内容）：提交/跳过/回合结束时清除
    pending_questions: HashMap<String, PendingQuestion>,
    /// 各会话的 TodoList/后台任务快照（core 推送缓存，切会话时同步给 composer）
    todos_by_session: HashMap<String, Vec<pig_protocol::TodoItem>>,
    tasks_by_session: HashMap<String, Vec<pig_protocol::TaskSummary>>,
    agent: AgentClient,
    cwd: PathBuf,
    config_path: Option<PathBuf>,
    exec_mode: pig_protocol::ExecMode,
    git_branch: Option<String>,
    /// 标题栏分支切换器的分支列表（当前会话 cwd 的本地分支）
    title_branches: Vec<String>,
    /// 标题栏分支菜单是否打开
    title_branch_menu_open: bool,
    /// 分支菜单因点击外部收起时的按下位置（吞掉同一次 click，防收起又弹开）
    title_branch_outside_close: Option<Point<Pixels>>,
    /// 分支 chip 的 bounds（on_prepaint 记录，菜单锚定用）
    title_branch_btn_bounds: Rc<Cell<Bounds<Pixels>>>,
    /// hero 页选择的工作区目录；None = 未选择（显示"选择工作区"，发送时回落到启动目录）
    hero_cwd: Option<PathBuf>,
    hero_branch: Option<String>,
    hero_branches: Vec<String>,
    hero_is_git: bool,
    hero_error: Option<String>,
    pending_first_send: Option<(String, Vec<String>, ExecMode)>,
    /// hero 态用户已显式选过模型：apply_hero_defaults 不再用工作区种子覆盖
    ///（否则「切模型 → 选工作区 → 发送」会把选择冲回工作区旧模型）
    hero_model_dirty: bool,
    workspaces: Vec<String>,
    /// 已移除（隐藏）的工作区路径：会话 cwd 不再让它们回到列表
    hidden_workspaces: std::collections::HashSet<String>,
    /// 工作区路径 → 用户自定义显示名
    workspace_aliases: std::collections::HashMap<String, String>,
    settings: Entity<SettingsView>,
    settings_open: bool,
    /// 「开启无管制模式？」确认框（每次切 Yolo 都弹，不记住选择）
    yolo_confirm_open: bool,
    /// 确认框焦点（Esc 取消用；打开时抢焦，取消键默认聚焦）
    yolo_confirm_focus: FocusHandle,
    sidebar_collapsed: bool,
    /// 右侧面板是否展开（默认收起：进会话不自动显示改动）
    right_open: bool,
    /// 右侧面板打开的 tab（按打开顺序）；收起时保留
    right_tabs: Vec<RightTab>,
    /// 右侧面板当前激活的 tab（None = 显示面板首页/菜单页）
    right_active: Option<RightTab>,
    /// 标签页栏 "+" 的加面板菜单是否打开
    right_menu_open: bool,
    /// 菜单因点击外部收起时的按下位置：吞掉同一次按压触发的按钮 click，避免收起又弹开
    right_menu_outside_close: Option<Point<Pixels>>,
    /// 标签页栏 "+" 按钮的屏幕 bounds（on_prepaint 记录，菜单锚定用）
    tab_add_btn_bounds: Rc<Cell<Bounds<Pixels>>>,
    config: Option<pig_protocol::AppConfig>,
    /// (provider_id, model_id)
    current_model: Option<(String, String)>,
    reasoning_level: Option<String>,
    /// 三栏布局引擎（dock）：左 dock=侧栏、center=会话区、右 dock=改动面板；
    /// set_locked(true) 锁定防拖拽重排、只保留调宽
    dock: Entity<gpui_kit::component::dock::DockArea>,
    /// 正在拖宽的 dock（自绘把手热区按下时置位；松手后的首个未按键 move 清除）
    dock_resizing: Option<DockPlacement>,
    _agent_handle: pig_core::AgentHandle,
    _subscriptions: Vec<Subscription>,
}

impl AppView {
    fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        config_path: Option<PathBuf>,
        cwd: PathBuf,
    ) -> Self {
        let sidebar = cx.new(|cx| Sidebar::new(window, cx));
        let composer = cx.new(|cx| Composer::new(window, cx));
        let settings = cx.new(|cx| SettingsView::new(window, cx));
        let (dock, dock_skin) =
            gpui_kit::component::dock::DockSkin::dock_area("pig-dock", None, window, cx);
        // tab 栏里的 dock 开合按钮不需要（有标题栏按钮），且我们的 tab 栏自绘
        dock_skin.set_toggle_button_visible(false, cx);
        let handle = pig_core::spawn_agent(config_path.clone(), cwd.clone());
        let agent = AgentClient::new(handle.ops.clone());

        let mut app = Self {
            sidebar,
            composer,
            views: HashMap::new(),
            current: None,
            metas: vec![],
            running: HashSet::new(),
            deleted_sessions: HashSet::new(),
            approval_pending: HashSet::new(),
            pending_approvals: HashMap::new(),
            pending_questions: HashMap::new(),
            todos_by_session: HashMap::new(),
            tasks_by_session: HashMap::new(),
            agent,
            hero_cwd: None,
            cwd,
            config_path,
            exec_mode: pig_protocol::ExecMode::AutoEdit,
            git_branch: None,
            title_branches: vec![],
            title_branch_menu_open: false,
            title_branch_outside_close: None,
            title_branch_btn_bounds: Rc::new(Cell::new(Bounds::default())),
            hero_branch: None,
            hero_branches: vec![],
            hero_is_git: false,
            hero_error: None,
            pending_first_send: None,
            hero_model_dirty: false,
            workspaces: vec![],
            hidden_workspaces: std::collections::HashSet::new(),
            workspace_aliases: std::collections::HashMap::new(),
            settings,
            settings_open: false,
            yolo_confirm_open: false,
            yolo_confirm_focus: cx.focus_handle(),
            sidebar_collapsed: false,
            right_open: false,
            right_tabs: vec![],
            right_active: None,
            right_menu_open: false,
            right_menu_outside_close: None,
            tab_add_btn_bounds: Rc::new(Cell::new(Bounds::default())),
            config: None,
            current_model: None,
            reasoning_level: None,
            dock,
            dock_resizing: None,
            _agent_handle: handle,
            _subscriptions: vec![],
        };
        app._subscriptions = vec![
            cx.subscribe_in(&app.composer, window, |this, _, event, window, cx| {
                this.on_composer_event(event, window, cx);
            }),
            cx.subscribe_in(&app.sidebar, window, |this, _, event, window, cx| {
                this.on_sidebar_event(event, window, cx);
            }),
            cx.subscribe(
                &app.settings,
                |this, _, event: &SettingsEvent, cx| match event {
                    SettingsEvent::Save(config) => {
                        this.config = Some(config.clone());
                        this.agent.save_config(config.clone());
                        this.apply_config_to_composer(cx);
                    }
                    SettingsEvent::TestProvider(id) => this.agent.test_provider(id.clone()),
                    SettingsEvent::LookupModel(id) => this.agent.model_lookup(id.clone()),
                    SettingsEvent::Close => {
                        this.settings_open = false;
                        cx.notify();
                    }
                },
            ),
            cx.observe_window_appearance(window, |_, window, cx| {
                if cx
                    .try_global::<ThemeFollowSystem>()
                    .is_some_and(|flag| flag.0)
                {
                    Theme::sync_system_appearance(Some(window), cx);
                }
            }),
        ];
        app.spawn_event_pump(app._agent_handle.events.clone(), cx);
        app.agent.list_sessions();
        app.agent.list_workspaces();
        app.agent.get_config();
        if let Some(cwd) = app.hero_cwd.clone() {
            app.agent.git_info(cwd);
        }
        app.push_hero_info(cx);
        app.refresh_git_branch(None, cx);
        app
    }

    fn spawn_event_pump(&mut self, events: async_channel::Receiver<Event>, cx: &mut Context<Self>) {
        cx.spawn(async move |this: WeakEntity<AppView>, cx| {
            while let Ok(event) = events.recv().await {
                let alive = this
                    .update(cx, |app, cx| {
                        app.route_event(event, cx);
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();
    }

    /// 模拟重启：关旧 manager、清内存视图、重 spawn（自测用）。
    fn restart_agent(&mut self, cx: &mut Context<Self>) {
        let handle = pig_core::spawn_agent(self.config_path.clone(), self.cwd.clone());
        self.agent = AgentClient::new(handle.ops.clone());
        self.views.clear();
        self.current = None;
        self.running.clear();
        self.approval_pending.clear();
        self.pending_approvals.clear();
        self.pending_questions.clear();
        self._agent_handle = handle;
        self.spawn_event_pump(self._agent_handle.events.clone(), cx);
        self.agent.list_sessions();
        cx.notify();
    }

    /// 当前会话的工作目录（SessionMeta.cwd）
    fn current_cwd(&self) -> Option<PathBuf> {
        let sid = self.current.as_ref()?;
        self.metas
            .iter()
            .find(|m| &m.id == sid)
            .map(|m| m.cwd.clone())
    }

    fn ensure_views(&mut self, session_id: &str, cx: &mut Context<Self>) {
        if self.views.contains_key(session_id) {
            return;
        }
        let thread = cx.new(|cx| ThreadView::new(cx));
        let review = cx.new(|cx| ReviewPanel::new(cx));
        let sid = session_id.to_string();
        self._subscriptions.push(
            cx.subscribe(&thread, move |this, _, event, cx| match event {
                ThreadEvent::ApprovalReply {
                    request_id,
                    decision,
                } => {
                    this.agent.approval_reply(request_id.clone(), *decision);
                    // 决议后立即撤掉审批条（不等回合结束），恢复输入框
                    this.approval_pending.remove(&sid);
                    this.pending_approvals.remove(&sid);
                    this.sync_composer_state(cx);
                }
                ThreadEvent::ExecutePlan => {
                    this.on_execute_plan(cx);
                }
                ThreadEvent::CancelQueued(text) => {
                    if let Some(sid) = this.current.clone() {
                        this.agent.cancel_queued(sid, text.clone());
                    }
                }
            }),
        );
        self._subscriptions.push(cx.subscribe(
            &review,
            |this, _, event: &ReviewEvent, _| match event {
                ReviewEvent::RefreshGit => {
                    if let Some(cwd) = this.current_cwd() {
                        this.agent.git_status(cwd);
                    }
                }
                ReviewEvent::OpenGitDiff { path, staged } => {
                    if let Some(cwd) = this.current_cwd() {
                        this.agent.git_diff(cwd, path.clone(), *staged);
                    }
                }
            },
        ));
        self.views
            .insert(session_id.to_string(), SessionViews { thread, review });
    }

    fn route_event(&mut self, event: Event, cx: &mut Context<Self>) {
        // 已删除会话的迟到事件（删除前已入队的流式/收尾事件）直接丢弃，
        // 防止 ensure_views 给已删会话重建僵尸视图
        if let Some(sid) = event_session_id(&event)
            && self.deleted_sessions.contains(&sid)
        {
            return;
        }
        match &event {
            Event::SessionConfigured {
                session_id,
                cwd,
                model,
                provider_name,
                provider_id,
                model_id,
                reasoning_level,
                exec_mode,
                fs_read_outside,
                fs_write_outside,
            } => {
                let session_id = session_id.clone();
                eprintln!(
                    "[model] SessionConfigured {session_id} → provider={provider_id:?} model={model_id:?} 思考={reasoning_level:?}"
                );
                self.ensure_views(&session_id, cx);
                self.current = Some(session_id.clone());
                // 换了会话：先清掉上一个会话的上下文水位（有数据的会话随后会收到补发）
                self.composer.update(cx, |composer, cx| {
                    composer.clear_context_usage(cx);
                });
                let label = if provider_name.is_empty() {
                    model.clone()
                } else {
                    format!("{provider_name}/{model}")
                };
                // 恢复会话的模型/模式/思考等级（core 持久化在 sessions 表）
                self.exec_mode = *exec_mode;
                self.reasoning_level = reasoning_level.clone();
                self.current_model = match (provider_id, model_id) {
                    (Some(p), Some(m)) => Some((p.clone(), m.clone())),
                    _ => None,
                };
                self.composer.update(cx, |composer, cx| {
                    composer.set_model_name(label, cx);
                    composer.set_hero_mode(false, cx);
                    composer.set_exec_mode(*exec_mode, cx);
                    composer.set_reasoning_level(reasoning_level.clone(), cx);
                    composer.set_fs_access(*fs_read_outside, *fs_write_outside, cx);
                });
                // 切换/新建会话：用缓存快照同步进度/任务面板（无快照则清空）
                let todos = self
                    .todos_by_session
                    .get(&session_id)
                    .cloned()
                    .unwrap_or_default();
                let tasks = self
                    .tasks_by_session
                    .get(&session_id)
                    .cloned()
                    .unwrap_or_default();
                self.composer.update(cx, |composer, cx| {
                    composer.set_todos(todos, cx);
                    composer.set_tasks(tasks, cx);
                });
                if let Some((text, files, mode)) = self.pending_first_send.take() {
                    self.agent.send_message(session_id, text, files, mode);
                }
                // 切换/新建会话：标题栏分支跟随会话 cwd；拉工作区 git 状态（Review 面板）
                self.refresh_git_branch(Some(cwd.clone()), cx);
                self.agent.git_status(cwd.clone());
            }
            Event::ModelInfo { id, info } => {
                // 回填 InputState::set_value 需要 window，经 AnyWindowHandle 进入窗口上下文
                let (id, info) = (id.clone(), info.clone());
                let settings = self.settings.clone();
                if let Some(handle) = cx.windows().into_iter().next() {
                    let _ = handle.update(cx, |_, window, cx| {
                        settings.update(cx, |settings, cx| {
                            settings.apply_model_info(&id, info, window, cx);
                        });
                    });
                }
            }
            Event::ConfigSnapshot { config } => {
                self.config = Some(config.clone());
                let config = config.clone();
                self.settings
                    .update(cx, |settings, cx| settings.set_config(config, cx));
                self.apply_config_to_composer(cx);
            }
            Event::TestResult {
                provider_id,
                ok,
                message,
            } => {
                self.settings.update(cx, |settings, cx| {
                    settings.set_test_result(provider_id, *ok, message.clone(), cx);
                });
            }
            Event::WorkspaceList { workspaces } => {
                self.workspaces = workspaces
                    .iter()
                    .filter(|p| !p.hidden)
                    .map(|p| p.path.display().to_string())
                    .collect();
                self.hidden_workspaces = workspaces
                    .iter()
                    .filter(|p| p.hidden)
                    .map(|p| p.path.display().to_string())
                    .collect();
                self.workspace_aliases = workspaces
                    .iter()
                    .filter_map(|p| {
                        p.alias
                            .clone()
                            .map(|alias| (p.path.display().to_string(), alias))
                    })
                    .collect();
            }
            Event::SessionList { sessions } => {
                self.metas = sessions.clone();
                if self.current.is_none() && !self.metas.is_empty() {
                    if let Some(meta) = self.metas.iter().find(|s| !s.archived) {
                        self.agent.open_session(meta.id.clone());
                    }
                }
                self.push_hero_info(cx);
            }
            Event::SessionTitleChanged { session_id, title } => {
                // 自动命名 sidecar 完成：只补丁标题，不重发整个列表
                if let Some(meta) = self.metas.iter_mut().find(|m| &m.id == session_id) {
                    meta.title = title.clone();
                }
                self.refresh_sidebar(cx);
            }
            Event::ExecModeChanged {
                session_id, mode, ..
            } => {
                // core 侧主动切了模式（ExitPlanMode 确认后）：chip/缓存同步
                self.exec_mode = *mode;
                if let Some(meta) = self.metas.iter_mut().find(|m| &m.id == session_id) {
                    meta.exec_mode = *mode;
                }
                let mode = *mode;
                self.composer.update(cx, |composer, cx| {
                    composer.set_exec_mode(mode, cx);
                });
                cx.notify();
            }
            Event::Error {
                session_id: None,
                message,
                ..
            } => {
                let message = message.clone();
                if self.is_hero(cx) {
                    self.hero_error = Some(message);
                } else if let Some(sid) = self.current.clone() {
                    self.ensure_views(&sid, cx);
                    self.views[&sid].thread.update(cx, |thread, cx| {
                        thread.add_system_note(&format!("⚠ {message}"), cx);
                    });
                }
            }
            Event::FileSearchResults { query, results, .. } => {
                let results = results.clone();
                let _ = query;
                self.composer
                    .update(cx, |composer, cx| composer.set_mention_results(results, cx));
            }
            Event::GitInfo {
                cwd,
                current_branch,
                branches,
            } => {
                if self.hero_cwd.as_ref() == Some(cwd) {
                    self.hero_branch = current_branch.clone();
                    self.hero_branches = branches.clone();
                    self.hero_is_git = current_branch.is_some();
                    self.push_hero_info(cx);
                }
            }
            Event::BranchChanged { cwd, branch } => {
                if self.hero_cwd.as_ref() == Some(cwd) {
                    self.hero_branch = Some(branch.clone());
                    self.agent.git_info(cwd.clone());
                }
                // 标题栏分支切换器：当前会话目录切了分支 → 更新 chip + 重拉分支列表，
                // 并刷新工作区 git 状态（切分支后改动面板内容全变）
                if self.current_cwd().as_ref() == Some(cwd) {
                    self.git_branch = Some(branch.clone());
                    self.refresh_git_branch(Some(cwd.clone()), cx);
                    self.agent.git_status(cwd.clone());
                }
            }
            Event::GitStatus {
                cwd,
                is_git,
                unstaged,
                staged,
            } => {
                // 工作区口径：同 cwd 的所有会话面板都更新
                let (is_git, unstaged, staged) = (*is_git, unstaged.clone(), staged.clone());
                let sids: Vec<String> = self
                    .metas
                    .iter()
                    .filter(|m| m.cwd == *cwd)
                    .map(|m| m.id.clone())
                    .collect();
                for sid in &sids {
                    if let Some(views) = self.views.get(sid) {
                        let (unstaged, staged) = (unstaged.clone(), staged.clone());
                        views.review.update(cx, |review, cx| {
                            review.set_git_status(is_git, unstaged, staged, cx);
                        });
                    }
                }
                // 输入框上方的改动 chip：git 口径（未暂存 + 已暂存合并统计）
                if self.current.as_ref().is_some_and(|cur| sids.contains(cur)) {
                    let (adds, dels) = unstaged
                        .iter()
                        .chain(staged.iter())
                        .fold((0u32, 0u32), |(a, d), e| (a + e.additions, d + e.deletions));
                    let files: Vec<(String, u32, u32)> = unstaged
                        .iter()
                        .chain(staged.iter())
                        .map(|e| (e.path.clone(), e.additions, e.deletions))
                        .collect();
                    self.composer.update(cx, |composer, cx| {
                        composer.set_changes(adds, dels, files, cx);
                    });
                }
            }
            Event::GitDiff {
                cwd, path, diff, ..
            } => {
                let (path, diff) = (path.clone(), diff.clone());
                let sids: Vec<String> = self
                    .metas
                    .iter()
                    .filter(|m| m.cwd == *cwd)
                    .map(|m| m.id.clone())
                    .collect();
                for sid in sids {
                    if let Some(views) = self.views.get(&sid) {
                        let (path, diff) = (path.clone(), diff.clone());
                        views.review.update(cx, |review, cx| {
                            review.set_git_diff(path, diff, cx);
                        });
                    }
                }
            }
            Event::ContextUsage {
                session_id,
                used,
                total,
                cache_read_total,
                input_total,
                ..
            } => {
                if self.current.as_deref() == Some(session_id.as_str()) {
                    let (used, total, cache_read_total, input_total) =
                        (*used, *total, *cache_read_total, *input_total);
                    self.composer.update(cx, |composer, cx| {
                        composer.set_context_usage(used, total, cache_read_total, input_total, cx);
                    });
                }
            }
            Event::TodoListChanged {
                session_id, items, ..
            } => {
                self.todos_by_session
                    .insert(session_id.clone(), items.clone());
                if self.current.as_deref() == Some(session_id.as_str()) {
                    let items = items.clone();
                    self.composer
                        .update(cx, |composer, cx| composer.set_todos(items, cx));
                }
            }
            Event::TaskListChanged {
                session_id, tasks, ..
            } => {
                self.tasks_by_session
                    .insert(session_id.clone(), tasks.clone());
                if self.current.as_deref() == Some(session_id.as_str()) {
                    let tasks = tasks.clone();
                    self.composer
                        .update(cx, |composer, cx| composer.set_tasks(tasks, cx));
                }
            }
            Event::TurnComplete {
                session_id,
                duration_ms,
                ..
            } => {
                // duration_ms=0 是会话回放，不触发计划模式待执行标记
                if *duration_ms > 0 && self.exec_mode == pig_protocol::ExecMode::Plan {
                    let session_id = session_id.clone();
                    if let Some(views) = self.views.get(&session_id) {
                        views.thread.update(cx, |thread, cx| {
                            thread.set_plan_pending(true, cx);
                        });
                    }
                }
                self.refresh_git_branch(self.current_cwd(), cx);
                // 回合结束（agent 写文件已落定）：刷新工作区 git 状态
                if let Some(meta) = self.metas.iter().find(|m| &m.id == session_id) {
                    self.agent.git_status(meta.cwd.clone());
                }
            }
            Event::ContextCompacted {
                session_id, note, ..
            } => {
                let session_id = session_id.clone();
                self.ensure_views(&session_id, cx);
                let note = note.clone();
                self.views[&session_id].thread.update(cx, |thread, cx| {
                    thread.add_system_note(&note, cx);
                });
            }
            _ => {}
        }

        // 运行状态跟踪（侧栏状态点 + 当前会话的 composer 状态）
        let sid = event_session_id(&event);
        if let Some(sid) = &sid {
            match &event {
                Event::TurnStarted { .. } => {
                    self.running.insert(sid.clone());
                }
                Event::TurnComplete { .. } | Event::TurnAborted { .. } => {
                    self.running.remove(sid);
                    self.approval_pending.remove(sid);
                    self.pending_approvals.remove(sid);
                    self.pending_questions.remove(sid);
                }
                Event::ApprovalRequested { tool, detail, .. } => {
                    self.approval_pending.insert(sid.clone());
                    let cwd = self
                        .metas
                        .iter()
                        .find(|m| &m.id == sid)
                        .map(|m| m.cwd.display().to_string())
                        .unwrap_or_default();
                    self.pending_approvals.insert(
                        sid.clone(),
                        PendingApproval {
                            tool: tool.clone(),
                            detail: detail.clone(),
                            cwd,
                        },
                    );
                }
                Event::QuestionRequested {
                    request_id,
                    questions,
                    ..
                } => {
                    self.pending_questions.insert(
                        sid.clone(),
                        PendingQuestion {
                            request_id: request_id.clone(),
                            questions: questions.clone(),
                        },
                    );
                }
                Event::Error { .. } => {
                    self.running.remove(sid);
                    self.approval_pending.remove(sid);
                    self.pending_approvals.remove(sid);
                    self.pending_questions.remove(sid);
                }
                _ => {}
            }
        }

        // 路由到 per-session 视图
        if let Some(sid) = &sid {
            self.ensure_views(sid, cx);
            if let Some(views) = self.views.get(sid) {
                match &event {
                    Event::FileChanged {
                        path,
                        unified_diff,
                        additions,
                        deletions,
                        ..
                    } => {
                        let (path, diff, adds, dels) =
                            (path.clone(), unified_diff.clone(), *additions, *deletions);
                        views.review.update(cx, |review, cx| {
                            review.upsert(path, diff, adds, dels, cx);
                        });
                    }
                    Event::FileReverted { path, .. } => {
                        let path = path.clone();
                        views.review.update(cx, |review, cx| {
                            review.remove(&path, cx);
                        });
                        // 撤销改变了工作区内容：刷新 git 状态
                        if let Some(meta) = self.metas.iter().find(|m| &m.id == sid) {
                            self.agent.git_status(meta.cwd.clone());
                        }
                    }
                    _ => {
                        views.thread.update(cx, |thread, cx| {
                            thread.reduce_event(event.clone(), cx);
                        });
                    }
                }
            }
            if self.current.as_ref() == Some(sid) {
                self.sync_composer_state(cx);
            }
        }
        self.refresh_sidebar(cx);
        self.sync_hero_mode(cx);
        cx.notify();
    }

    fn sync_composer_state(&self, cx: &mut Context<Self>) {
        let Some(sid) = &self.current else { return };
        let streaming = self.running.contains(sid);
        let approval = self.pending_approvals.get(sid).cloned();
        let question = self.pending_questions.get(sid).cloned();
        self.composer.update(cx, |composer, cx| {
            composer.set_streaming(streaming, cx);
            composer.set_approval(approval, cx);
            composer.set_question(question, cx);
        });
    }

    /// 工作区列表 = 可见手动工作区 ∪ 会话 cwd（排除已移除/隐藏的工作区）；
    /// 按最近活跃/添加时间倒序。
    fn compute_workspaces(&self) -> Vec<String> {
        let mut latest: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        for meta in &self.metas {
            let key = meta.cwd.display().to_string();
            if self.hidden_workspaces.contains(&key) {
                continue;
            }
            let entry = latest.entry(key).or_insert(0);
            *entry = (*entry).max(meta.updated_at);
        }
        for path in &self.workspaces {
            latest.entry(path.clone()).or_insert(0);
        }
        let mut all: Vec<(String, u64)> = latest.into_iter().collect();
        all.sort_by_key(|(_, ts)| std::cmp::Reverse(*ts));
        all.into_iter().map(|(path, _)| path).collect()
    }

    fn refresh_sidebar(&self, cx: &mut Context<Self>) {
        let sessions: Vec<SidebarSession> = self
            .metas
            .iter()
            .map(|meta| SidebarSession {
                id: meta.id.clone(),
                title: meta.title.clone(),
                cwd: meta.cwd.clone(),
                updated_at: meta.updated_at,
                pinned: meta.pinned,
                archived: meta.archived,
                running: self.running.contains(&meta.id),
                waiting_approval: self.approval_pending.contains(&meta.id),
            })
            .collect();
        let workspaces = self.compute_workspaces();
        let aliases = self.workspace_aliases.clone();
        let active = self.current.clone();
        self.sidebar.update(cx, |sidebar, cx| {
            sidebar.set_state(sessions, workspaces, aliases, active, cx);
        });
    }

    /// 模型 chip 显示名：config 里按 provider_id 查供应商名，查不到退化为 model_id
    /// 从 config 取模型的思考等级列表
    fn model_reasoning_levels(&self, provider_id: &str, model_id: &str) -> Vec<String> {
        self.config
            .as_ref()
            .and_then(|c| c.providers.iter().find(|p| p.id == provider_id))
            .and_then(|p| p.models.iter().find(|m| m.id == model_id))
            .map(|m| m.reasoning_levels.clone())
            .unwrap_or_default()
    }

    /// 模型配置的默认思考等级（已校验仍在等级表内才返回）
    fn model_default_reasoning_level(&self, provider_id: &str, model_id: &str) -> Option<String> {
        self.config
            .as_ref()
            .and_then(|c| c.providers.iter().find(|p| p.id == provider_id))
            .and_then(|p| p.models.iter().find(|m| m.id == model_id))
            .and_then(|m| {
                m.default_reasoning_level
                    .clone()
                    .filter(|lv| m.reasoning_levels.contains(lv))
            })
    }

    /// 切换模型后当前等级不可用时的落点：high 优先（多数等级表的中间档），
    /// 否则首个非关档，再否则首档；无等级 = 关（None）
    fn fallback_reasoning_level(levels: &[String]) -> Option<String> {
        if levels.is_empty() {
            return None;
        }
        if levels.iter().any(|l| l == "high") {
            return Some("high".to_string());
        }
        levels
            .iter()
            .find(|l| l.as_str() != "none" && l.as_str() != "disabled")
            .cloned()
            .or_else(|| levels.first().cloned())
    }

    fn model_display_label(&self, provider_id: &str, model_id: &str) -> String {
        let pname = self
            .config
            .as_ref()
            .and_then(|c| c.providers.iter().find(|pr| pr.id == provider_id))
            .map(|pr| pr.name.clone())
            .unwrap_or_default();
        if pname.is_empty() {
            model_id.to_string()
        } else {
            format!("{pname}/{model_id}")
        }
    }

    fn switch_session(&mut self, session_id: String, cx: &mut Context<Self>) {
        if self.views.contains_key(&session_id) {
            self.current = Some(session_id.clone());
            // 快速路径不发 SessionConfigured：清上一个会话的水位，
            // core 的 OpenSession 补发（有数据时）随后到达
            self.composer.update(cx, |composer, cx| {
                composer.clear_context_usage(cx);
            });
            // 已打开过的会话走这条快速路径，core 不会再发 SessionConfigured——
            // 必须按 meta 恢复会话级的模型/模式/思考等级，否则会带着上一个会话的值
            if let Some(meta) = self.metas.iter().find(|m| m.id == session_id).cloned() {
                self.exec_mode = meta.exec_mode;
                self.reasoning_level = meta.reasoning_level.clone();
                let label = match (&meta.provider_id, &meta.model_id) {
                    (Some(p), Some(m)) => {
                        self.current_model = Some((p.clone(), m.clone()));
                        Some(self.model_display_label(p, m))
                    }
                    _ => {
                        self.current_model = None;
                        None
                    }
                };
                self.composer.update(cx, |composer, cx| {
                    composer.set_exec_mode(meta.exec_mode, cx);
                    composer.set_reasoning_level(meta.reasoning_level.clone(), cx);
                    composer.set_fs_access(meta.fs_read_outside, meta.fs_write_outside, cx);
                    if let Some(label) = label {
                        composer.set_model_name(label, cx);
                    }
                });
            }
            self.sync_composer_state(cx);
            self.refresh_git_branch(self.current_cwd(), cx);
            self.refresh_sidebar(cx);
            cx.notify();
            // 仍要通知 core：它按当前会话补发面板快照与上下文水位
            //（SessionConfigured 的处理是幂等的，重复恢复无害）
            self.agent.open_session(session_id);
        } else {
            self.agent.open_session(session_id);
        }
    }

    /// hero 态：当前无会话，或当前会话没有任何消息
    fn is_hero(&self, cx: &App) -> bool {
        match &self.current {
            None => true,
            Some(sid) => self
                .views
                .get(sid)
                .map(|views| views.thread.read(cx).is_empty())
                .unwrap_or(true),
        }
    }

    fn push_hero_info(&self, cx: &mut Context<Self>) {
        let label = self
            .hero_cwd
            .as_ref()
            .map(|cwd| {
                cwd.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| cwd.display().to_string())
            })
            .unwrap_or_else(|| "选择工作区".to_string());
        let mut cwds: Vec<String> = self
            .metas
            .iter()
            .map(|m| m.cwd.display().to_string())
            .collect();
        if let Some(cwd) = &self.hero_cwd {
            let current = cwd.display().to_string();
            if !cwds.contains(&current) {
                cwds.insert(0, current);
            }
        }
        let mut seen = std::collections::HashSet::new();
        cwds.retain(|c| seen.insert(c.clone()));
        let (branch, branches, is_git) = (
            self.hero_branch.clone(),
            self.hero_branches.clone(),
            self.hero_is_git,
        );
        let cwd = self.hero_cwd.as_ref().map(|c| c.display().to_string());
        self.composer.update(cx, |composer, cx| {
            composer.set_hero_info(cwd, label, cwds, branch, branches, is_git, cx);
        });
    }

    /// hero 默认值：把「工作区最近活跃会话」的模型/模式/思考等级铺到 composer，
    /// 作为下次新建会话的默认值（用户可再改；hero_send 时按当前选择创建）。
    /// 无种子（新工作区）则不动，保留 app 默认/上次选择。
    fn apply_hero_defaults(&mut self, cx: &mut Context<Self>) {
        let cwd = self.hero_cwd.clone().unwrap_or_else(|| self.cwd.clone());
        let Some(seed) = self
            .metas
            .iter()
            .filter(|m| !m.archived && m.cwd == cwd)
            .max_by_key(|m| m.updated_at)
            .cloned()
        else {
            return;
        };
        self.exec_mode = seed.exec_mode;
        self.reasoning_level = seed.reasoning_level.clone();
        // 模型显示名：config 里按 provider_id 查供应商名，查不到退化为 model_id。
        // hero 态用户已显式选过模型时不覆盖（模式/思考等级仍铺种子）
        let label = match (&seed.provider_id, &seed.model_id) {
            (Some(p), Some(m)) if !self.hero_model_dirty => {
                self.current_model = Some((p.clone(), m.clone()));
                self.model_display_label(p, m)
            }
            // 种子没有模型选择：模型展示不动，只铺模式/思考等级
            _ => String::new(),
        };
        self.composer.update(cx, |composer, cx| {
            composer.set_exec_mode(seed.exec_mode, cx);
            composer.set_reasoning_level(seed.reasoning_level.clone(), cx);
            composer.set_fs_access(seed.fs_read_outside, seed.fs_write_outside, cx);
            if !label.is_empty() {
                composer.set_model_name(label, cx);
            }
        });
        cx.notify();
    }

    fn enter_hero(&mut self, cx: &mut Context<Self>) {
        self.current = None;
        self.git_branch = None;
        self.title_branches = vec![];
        self.title_branch_menu_open = false;
        self.hero_error = None;
        // 新的 hero 周期：显式模型选择标记复位（工作区种子重新生效）
        self.hero_model_dirty = false;
        self.composer.update(cx, |composer, cx| {
            composer.clear_context_usage(cx);
            // 进度/任务/改动 chip 同属上个会话的状态，一并清掉（setter 会收起对应弹层）
            composer.set_todos(vec![], cx);
            composer.set_tasks(vec![], cx);
            composer.set_changes(0, 0, vec![], cx);
        });
        if let Some(cwd) = self.hero_cwd.clone() {
            self.agent.git_info(cwd);
        } else {
            self.hero_branch = None;
            self.hero_branches = vec![];
            self.hero_is_git = false;
        }
        self.composer.update(cx, |composer, cx| {
            composer.set_hero_mode(true, cx);
            composer.set_streaming(false, cx);
        });
        self.push_hero_info(cx);
        self.refresh_sidebar(cx);
        self.apply_hero_defaults(cx);
        cx.notify();
    }

    fn hero_send(
        &mut self,
        text: String,
        files: Vec<String>,
        mode: ExecMode,
        cx: &mut Context<Self>,
    ) {
        self.pending_first_send = Some((text, files, mode));
        let cwd = self.hero_cwd.clone().unwrap_or_else(|| self.cwd.clone());
        // 带上 UI 当前选择：新建会话用它们（而不是工作区种子）初始化，
        // 避免 SessionConfigured 回来把用户刚选的模式/思考等级覆盖掉
        let (provider_id, model_id) = match self.current_model.clone() {
            Some((p, m)) => (Some(p), Some(m)),
            None => (None, None),
        };
        eprintln!(
            "[model] hero_send 新建会话：cwd={} 模型={:?} 思考={:?}",
            cwd.display(),
            provider_id.as_deref().zip(model_id.as_deref()),
            self.reasoning_level
        );
        self.agent.new_session(
            cwd,
            provider_id,
            model_id,
            self.reasoning_level.clone(),
            Some(mode),
        );
        self.composer.update(cx, |composer, cx| {
            composer.set_hero_mode(false, cx);
        });
        cx.notify();
    }

    fn pick_directory(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("选择工作目录".into()),
        });
        let view = cx.entity();
        cx.spawn_in(window, async move |_, window| {
            let picked = rx.await.ok()?.ok()??.into_iter().next()?;
            if !picked.is_dir() {
                return None;
            }
            window
                .update(|_, cx| {
                    view.update(cx, |this, cx| {
                        this.hero_error = None;
                        this.hero_branch = None;
                        this.hero_branches = vec![];
                        this.agent.git_info(picked.clone());
                        this.hero_cwd = Some(picked);
                        this.push_hero_info(cx);
                        // 换了工作区：按新工作区最近活跃会话重铺默认值
                        this.apply_hero_defaults(cx);
                        cx.notify();
                    });
                })
                .ok()?;
            Some(())
        })
        .detach();
    }

    fn on_execute_plan(&mut self, cx: &mut Context<Self>) {
        let Some(sid) = self.current.clone() else {
            return;
        };
        self.exec_mode = pig_protocol::ExecMode::ConfirmBeforeEdit;
        self.update_current_meta(|m| {
            m.exec_mode = pig_protocol::ExecMode::ConfirmBeforeEdit;
        });
        self.composer.update(cx, |composer, cx| {
            composer.set_exec_mode(pig_protocol::ExecMode::ConfirmBeforeEdit, cx);
        });
        self.agent
            .set_exec_mode(sid.clone(), pig_protocol::ExecMode::ConfirmBeforeEdit);
        let text = "计划已确认，请按计划开始执行".to_string();
        if let Some(views) = self.views.get(&sid) {
            views.thread.update(cx, |thread, cx| {
                thread.append_user_message(text.clone(), vec![], cx);
            });
        }
        self.agent
            .send_message(sid, text, vec![], pig_protocol::ExecMode::ConfirmBeforeEdit);
    }

    /// 同步更新 metas 缓存中当前会话的条目（与 core 写穿保持一致；
    /// core 的 Set* 写穿不再发 SessionList，缓存不更新会导致切会话读到旧值）
    fn update_current_meta(&mut self, f: impl FnOnce(&mut SessionMeta)) {
        if let Some(sid) = &self.current
            && let Some(meta) = self.metas.iter_mut().find(|m| &m.id == sid)
        {
            f(meta);
        }
    }

    /// 应用执行模式：本地缓存 + core 下发 + composer 勾选态（直接选中路径是幂等重设，
    /// Yolo 确认框路径靠它补上——拦截时 composer 的下标没动过）
    fn apply_exec_mode(&mut self, mode: ExecMode, cx: &mut Context<Self>) {
        self.exec_mode = mode;
        self.update_current_meta(|m| m.exec_mode = mode);
        self.composer
            .update(cx, |composer, cx| composer.set_exec_mode(mode, cx));
        if let Some(sid) = &self.current {
            self.agent.set_exec_mode(sid.clone(), mode);
        }
    }

    /// 关闭 Yolo 确认框并回焦输入框
    fn close_yolo_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.yolo_confirm_open = false;
        self.composer.update(cx, |composer, cx| {
            composer.focus_input(window, cx);
        });
        cx.notify();
    }

    /// Yolo 确认框「开启无管制模式」：应用模式并关闭
    fn confirm_yolo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_yolo_confirm(window, cx);
        self.apply_exec_mode(ExecMode::Yolo, cx);
    }

    /// 「开启无管制模式？」确认框（ModelDialog 同款覆盖层：遮罩 + 居中卡片）。
    /// 取消/点遮罩/Esc 不生效；确认才切 Yolo。每次切换都弹，不记住选择。
    fn render_yolo_confirm(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("yolo-confirm-overlay")
            .absolute()
            .inset_0()
            .bg(gpui_kit::black().opacity(0.5))
            .flex()
            .items_center()
            .justify_center()
            .on_click(cx.listener(|this, _, window, cx| {
                this.close_yolo_confirm(window, cx);
            }))
            .child(
                v_flex()
                    .id("yolo-confirm")
                    .track_focus(&self.yolo_confirm_focus)
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                        if event.keystroke.key == "escape" {
                            this.close_yolo_confirm(window, cx);
                        }
                    }))
                    .on_click(|_, _, cx| cx.stop_propagation()) // 点卡片不触发遮罩取消
                    .w(px(420.))
                    .gap_3()
                    .p_4()
                    .rounded(cx.theme().radius_lg)
                    .bg(cx.theme().popover)
                    .border_1()
                    .border_color(cx.theme().border)
                    .child(
                        div()
                            .text_lg()
                            .font_semibold()
                            .text_color(cx.theme().danger)
                            .child("开启无管制模式？"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child("此模式下所有操作直接执行：不弹任何确认，危险命令也不再拦截。仅建议在容器、虚拟机等隔离环境中使用。")
                            .child("注意：敏感文件（.env / 私钥 / 云凭据）仍会拦截。"),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .justify_end()
                            .child(
                                Button::new("yolo-cancel")
                                    .label("取消")
                                    .outline()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.close_yolo_confirm(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("yolo-confirm")
                                    .label("开启无管制模式")
                                    .danger()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.confirm_yolo(window, cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn on_composer_event(
        &mut self,
        event: &ComposerEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            ComposerEvent::Send { text, files, mode } => {
                if self.is_hero(cx) {
                    self.hero_send(text.clone(), files.clone(), *mode, cx);
                    return;
                }
                let Some(sid) = self.current.clone() else {
                    return;
                };
                // 用户消息由 core 的 Event::UserMessage 统一上屏（含排队出队路径）
                self.agent
                    .send_message(sid, text.clone(), files.clone(), *mode);
            }
            ComposerEvent::PickDirectory => self.pick_directory(window, cx),
            ComposerEvent::SelectCwd(cwd) => {
                let cwd = PathBuf::from(cwd);
                self.hero_error = None;
                self.agent.git_info(cwd.clone());
                self.hero_cwd = Some(cwd);
                self.push_hero_info(cx);
                // 换了工作区：按新工作区最近活跃会话重铺默认值
                self.apply_hero_defaults(cx);
            }
            ComposerEvent::ClearCwd => {
                self.hero_cwd = None;
                self.hero_error = None;
                self.hero_branch = None;
                self.hero_branches = vec![];
                self.hero_is_git = false;
                self.push_hero_info(cx);
                cx.notify();
            }
            ComposerEvent::CheckoutBranch(branch) => {
                if let Some(cwd) = self.hero_cwd.clone() {
                    self.agent.checkout_branch(cwd, branch.clone());
                }
            }
            ComposerEvent::Stop => {
                if let Some(sid) = &self.current {
                    self.agent.interrupt(sid.clone());
                }
            }
            ComposerEvent::Clear => {
                if let Some(sid) = &self.current {
                    if let Some(views) = self.views.get(sid) {
                        views.thread.update(cx, |thread, cx| thread.clear(cx));
                    }
                }
            }
            ComposerEvent::Compact => {
                if let Some(sid) = &self.current {
                    self.agent.compact(sid.clone());
                }
            }
            ComposerEvent::SetModel {
                provider_id,
                model_id,
            } => {
                eprintln!(
                    "[model] 收到切换 → {provider_id}/{model_id}（hero={}，原思考={:?}）",
                    self.current.is_none(),
                    self.reasoning_level
                );
                self.current_model = Some((provider_id.clone(), model_id.clone()));
                if self.current.is_none() {
                    // hero 态的显式选择：此后的工作区种子默认值不再覆盖模型
                    self.hero_model_dirty = true;
                }
                // 切模型的思考等级落点（优先级从高到低）：
                // 1. 新模型配置的默认档
                // 2. 继承上个模型的等级（需新模型支持；None=关原样继承）
                // 3. 继承档不被支持（Some 但不在表内）→ 启发式兜底（high → 首个非关档）
                let new_levels = self.model_reasoning_levels(provider_id, model_id);
                let target = self
                    .model_default_reasoning_level(provider_id, model_id)
                    .or_else(|| {
                        self.reasoning_level
                            .clone()
                            .filter(|lv| new_levels.contains(lv))
                    })
                    .or_else(|| {
                        self.reasoning_level
                            .is_some()
                            .then(|| Self::fallback_reasoning_level(&new_levels))
                            .flatten()
                    });
                if self.reasoning_level != target {
                    eprintln!(
                        "[model] 切换 {model_id}：思考等级 {:?} → {:?}",
                        self.reasoning_level, target
                    );
                    self.reasoning_level = target;
                    let level = self.reasoning_level.clone();
                    self.composer.update(cx, |composer, cx| {
                        composer.set_reasoning_level(level, cx);
                    });
                }
                let reasoning = self.reasoning_level.clone();
                self.update_current_meta(|m| {
                    m.provider_id = Some(provider_id.clone());
                    m.model_id = Some(model_id.clone());
                    m.reasoning_level = reasoning.clone();
                });
                if let Some(sid) = &self.current {
                    self.agent.set_model(
                        sid.clone(),
                        provider_id.clone(),
                        model_id.clone(),
                        self.reasoning_level.clone(),
                    );
                }
            }
            ComposerEvent::SetReasoning(level) => {
                self.reasoning_level = level.clone();
                // 思考等级独立于模型选择：未显式选模型时同样写穿并生效（作用于默认模型）
                self.update_current_meta(|m| {
                    m.reasoning_level = level.clone();
                });
                if let Some(sid) = &self.current {
                    self.agent.set_reasoning(sid.clone(), level.clone());
                }
            }
            ComposerEvent::SetExecMode(mode) => {
                self.apply_exec_mode(*mode, cx);
            }
            ComposerEvent::RequestYoloConfirm => {
                self.yolo_confirm_open = true;
                self.yolo_confirm_focus.focus(window, cx);
                cx.notify();
            }
            ComposerEvent::SetFsAccess {
                read_outside,
                write_outside,
            } => {
                self.update_current_meta(|m| {
                    m.fs_read_outside = *read_outside;
                    m.fs_write_outside = *write_outside;
                });
                if let Some(sid) = &self.current {
                    self.agent
                        .set_fs_access(sid.clone(), *read_outside, *write_outside);
                }
            }
            ComposerEvent::OpenSettings => self.open_settings(cx),
            ComposerEvent::DecideApproval(decision) => {
                // 走 ThreadView::decide_pending 单一路径：更新线程内审批卡状态并
                // 经 ThreadEvent::ApprovalReply 回复 core（那里同时清掉待审批态）
                if let Some(sid) = &self.current
                    && let Some(views) = self.views.get(sid)
                {
                    let decision = *decision;
                    views.thread.update(cx, |thread, cx| {
                        thread.decide_pending(decision, cx);
                    });
                }
            }
            ComposerEvent::QuestionReply {
                request_id,
                answers,
            } => {
                // 提交/跳过后立即撤掉问题条（不等回合结束），恢复输入框
                if let Some(sid) = &self.current {
                    self.pending_questions.remove(sid);
                }
                self.agent
                    .question_reply(request_id.clone(), answers.clone());
                self.sync_composer_state(cx);
            }
            ComposerEvent::SearchFiles(query) => {
                if let Some(sid) = &self.current {
                    self.agent.search_files(sid.clone(), query.clone());
                }
            }
            ComposerEvent::OpenChanges => {
                self.open_right_tab(RightTab::Changes, cx);
            }
        }
    }

    fn on_sidebar_event(
        &mut self,
        event: &SidebarEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            SidebarEvent::Select(id) => self.switch_session(id.clone(), cx),
            SidebarEvent::NewTask => {
                self.hero_cwd = None;
                self.enter_hero(cx);
            }
            SidebarEvent::NewTaskInWorkspace(path) => {
                self.hero_cwd = Some(PathBuf::from(path));
                self.enter_hero(cx);
            }
            SidebarEvent::SetPinned(id, pinned) => self.agent.set_pinned(id, *pinned),
            SidebarEvent::SetArchived(id, archived) => self.agent.set_archived(id, *archived),
            SidebarEvent::RenameSession(id, title) => self.rename_session(id, title, cx),
            SidebarEvent::DeleteSession(id) => self.delete_session(id, cx),
            SidebarEvent::RemoveWorkspace(path) => {
                self.agent.remove_workspace(PathBuf::from(path));
            }
            SidebarEvent::RenameWorkspace(path, alias) => {
                self.agent
                    .rename_workspace(PathBuf::from(path), alias.clone());
            }
            SidebarEvent::OpenSettings => self.open_settings(cx),
        }
    }

    /// 手动重命名会话：本地缓存即时更新（侧栏立刻生效），
    /// core 落库（置 title_custom，自动命名不再覆盖）后发 SessionList 再同步
    fn rename_session(&mut self, id: &str, title: &str, cx: &mut Context<Self>) {
        if let Some(meta) = self.metas.iter_mut().find(|m| m.id == id) {
            meta.title = title.to_string();
        }
        self.agent.rename_session(id, title);
        self.refresh_sidebar(cx);
        cx.notify();
    }

    /// 删除会话：本地视图/缓存清理 + 通知 core 清库与 rollout 文件。
    /// 删的是当前会话时切到最近的未归档会话，没有则回 hero
    fn delete_session(&mut self, id: &str, cx: &mut Context<Self>) {
        self.deleted_sessions.insert(id.to_string());
        self.views.remove(id);
        self.running.remove(id);
        self.approval_pending.remove(id);
        self.pending_approvals.remove(id);
        self.pending_questions.remove(id);
        self.todos_by_session.remove(id);
        self.tasks_by_session.remove(id);
        self.metas.retain(|m| m.id != id);
        self.agent.delete_session(id);
        if self.current.as_deref() == Some(id) {
            self.current = None;
            match self.metas.iter().find(|m| !m.archived) {
                Some(next) => self.switch_session(next.id.clone(), cx),
                None => self.enter_hero(cx),
            }
        }
        self.refresh_sidebar(cx);
        cx.notify();
    }

    /// 标题栏分支：cwd=None（hero 态）或非 git 目录时不显示。
    /// 一次查齐当前分支 + 本地分支列表（切换菜单用）
    fn refresh_git_branch(&mut self, cwd: Option<PathBuf>, cx: &mut Context<Self>) {
        let task = cx.background_executor().spawn(async move {
            let Some(cwd) = cwd else {
                return (None, Vec::new());
            };
            pig_core::git::git_info(&cwd)
        });
        cx.spawn(async move |this: WeakEntity<AppView>, cx| {
            let (branch, branches) = task.await;
            let _ = this.update(cx, |app, cx| {
                app.git_branch = branch;
                app.title_branches = branches;
                cx.notify();
            });
        })
        .detach();
    }

    /// ConfigSnapshot → composer 模型列表（启用供应商的启用模型，按供应商分组平铺）
    fn apply_config_to_composer(&self, cx: &mut Context<Self>) {
        let Some(config) = &self.config else { return };
        let models: Vec<crate::composer::ModelOption> = config
            .providers
            .iter()
            .filter(|p| p.enabled)
            .flat_map(|p| {
                p.models
                    .iter()
                    .filter(|m| m.enabled)
                    .map(|m| {
                        (
                            p.name.clone(),
                            p.id.clone(),
                            m.id.clone(),
                            // (等级 id, 显示名)——显示名缺省回退 id 本身
                            m.reasoning_levels
                                .iter()
                                .map(|lv| {
                                    let label = m
                                        .reasoning_labels
                                        .get(lv)
                                        .cloned()
                                        .unwrap_or_else(|| lv.clone());
                                    (lv.clone(), label)
                                })
                                .collect::<Vec<_>>(),
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        self.composer
            .update(cx, |composer, cx| composer.set_models(models, cx));
    }

    fn open_settings(&mut self, cx: &mut Context<Self>) {
        self.settings_open = true;
        self.agent.get_config();
        cx.notify();
    }

    /// 安装 dock 布局（构造后由 main 调用，此时 AppView 实体已就位，面板可持
    /// weak 引用）：左 dock = 侧栏，center = 会话区，右 dock = 改动面板。
    /// 锁定布局防拖拽重排、只保留调宽；右 dock 默认收起（toggle 一次）。
    fn install_dock(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use gpui_kit::component::dock::DockLayout;
        let app = cx.weak_entity();
        let center = cx.new(|cx| DockCenterPanel {
            app: app.clone(),
            focus_handle: cx.focus_handle(),
            _app_observer: observe_app_notify(&app, cx).expect("AppView 实体已就位"),
        });
        let right = cx.new(|cx| DockRightPanel {
            app: app.clone(),
            focus_handle: cx.focus_handle(),
            _app_observer: observe_app_notify(&app, cx).expect("AppView 实体已就位"),
        });
        self.dock.update(cx, |dock, cx| {
            dock.set_center(
                DockLayout::tabs().panel_view(panel_handle(center), cx),
                window,
                cx,
            );
            dock.set_dock(
                DockPlacement::Left,
                DockLayout::tabs().panel_view(panel_handle(self.sidebar.clone()), cx),
                window,
                cx,
            );
            dock.set_dock_size(DockPlacement::Left, px(220.), window, cx);
            dock.set_dock(
                DockPlacement::Right,
                DockLayout::tabs().panel_view(panel_handle(right), cx),
                window,
                cx,
            );
            dock.set_dock_size(DockPlacement::Right, px(300.), window, cx);
            dock.set_locked(true, window, cx);
            // 右侧面板默认收起：进会话不自动显示改动
            dock.toggle_dock(DockPlacement::Right, window, cx);
        });
    }

    /// 右侧面板开关（标题栏面板按钮）：展开/收起，tab 状态保留。
    /// 展开后没有激活 tab 时内容区显示面板首页（菜单页）。
    fn toggle_right_panel(&mut self, cx: &mut Context<Self>) {
        self.right_open = !self.right_open;
        cx.notify();
    }

    /// 右侧面板 tab 开关（快捷键用）：已激活时再次触发 = 收起面板；否则打开并激活该 tab。
    fn toggle_right_tab(&mut self, tab: RightTab, cx: &mut Context<Self>) {
        if self.right_open && self.right_active == Some(tab) {
            self.right_open = false;
        } else {
            if !self.right_tabs.contains(&tab) {
                self.right_tabs.push(tab);
            }
            self.right_active = Some(tab);
            self.right_open = true;
        }
        cx.notify();
    }

    /// 打开并激活右侧 tab（菜单点击用，纯打开不带收起语义）
    fn open_right_tab(&mut self, tab: RightTab, cx: &mut Context<Self>) {
        if !self.right_tabs.contains(&tab) {
            self.right_tabs.push(tab);
        }
        self.right_active = Some(tab);
        self.right_open = true;
        cx.notify();
    }

    /// 关闭右侧 tab：关掉激活 tab 时切到剩余最后一个；
    /// 没有 tab 了面板保持展开，回到面板首页（菜单页）。
    fn close_right_tab(&mut self, tab: RightTab, cx: &mut Context<Self>) {
        self.right_tabs.retain(|t| *t != tab);
        if self.right_active == Some(tab) {
            self.right_active = self.right_tabs.last().copied();
        }
        cx.notify();
    }

    /// 开关标签页栏 "+" 的加面板菜单。
    fn toggle_right_menu(&mut self, click: &ClickEvent, cx: &mut Context<Self>) {
        // 菜单打开时点按钮：按下先触发菜单的 outside-close（记录按下位置），
        // 紧随的 click 按同一按下位置吞掉，避免收起又马上弹开（composer 弹层同款处理）
        let down_pos = match click {
            ClickEvent::Mouse(event) => Some(event.down.position),
            _ => None,
        };
        if self
            .right_menu_outside_close
            .take()
            .is_some_and(|pos| Some(pos) == down_pos)
        {
            return;
        }
        self.right_menu_open = !self.right_menu_open;
        cx.notify();
    }

    /// 右侧面板菜单项：(名称, 图标, 快捷键 action, 占位禁用, 点击打开的 tab)。
    /// 浏览器/终端/侧边聊天为占位禁用项，快捷键先展示，功能后续加。
    fn right_menu_items() -> [(
        &'static str,
        AssetsIconName,
        Option<&'static dyn Action>,
        bool,
        Option<RightTab>,
    ); 4] {
        [
            (
                "改动",
                AssetsIconName::GitBranch,
                Some(&ToggleChanges),
                false,
                Some(RightTab::Changes),
            ),
            (
                "浏览器",
                AssetsIconName::Globe,
                Some(&ToggleBrowser),
                true,
                None,
            ),
            ("终端", AssetsIconName::SquareTerminal, None, true, None),
            (
                "侧边聊天",
                AssetsIconName::MessageCircle,
                Some(&ToggleSideChat),
                true,
                None,
            ),
        ]
    }

    /// 快捷键芯片组（ZCode 样式：每个键一个小芯片；未绑键时不显示）。
    /// page = 面板首页：带边框的大号键帽，macOS 修饰键符号逐键拆分；
    /// 否则（下拉菜单）：muted 底小芯片，macOS 符号串整体一个芯片。
    fn render_shortcut_chips(
        &self,
        action: &dyn Action,
        page: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let binding = window
            .highest_precedence_binding_for_action_in_context(action, KeyContext::default())?;
        let stroke = binding.keystrokes().first()?.as_keystroke().clone();
        let text = Kbd::format(&stroke);
        // Windows 风格 "Ctrl+Shift+G" 按 + 拆成单键芯片；macOS 符号串无 +：
        // page 模式逐修饰键拆帽（"⌃⇧G" → ⌃ | ⇧ | G），普通字符连续段合一
        let keys: Vec<String> = if text.contains('+') {
            text.split('+').map(|s| s.to_string()).collect()
        } else if page {
            let mut keys = Vec::new();
            let mut run = String::new();
            for ch in text.chars() {
                if matches!(ch, '⌃' | '⌥' | '⇧' | '⌘') {
                    if !run.is_empty() {
                        keys.push(std::mem::take(&mut run));
                    }
                    keys.push(ch.to_string());
                } else {
                    run.push(ch);
                }
            }
            if !run.is_empty() {
                keys.push(run);
            }
            keys
        } else {
            vec![text]
        };
        Some(
            h_flex()
                .gap_1()
                .flex_shrink_0()
                .children(keys.into_iter().map(|key| {
                    let chip = div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_center()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(key);
                    if page {
                        chip.min_w_5()
                            .h_5()
                            .rounded(cx.theme().radius)
                            .border_1()
                            .border_color(cx.theme().border)
                    } else {
                        chip.px_1()
                            .py_0p5()
                            .min_w_5()
                            .rounded(cx.theme().radius.half())
                            .bg(cx.theme().muted)
                    }
                    .into_any_element()
                }))
                .into_any_element(),
        )
    }

    /// 菜单行：图标 + 名称 + 快捷键芯片；disabled 为占位项（不可点）。
    /// page = 面板首页：整列居中、带边框键帽；否则（「+」下拉菜单）：
    /// 紧凑行。两种模式快捷键都贴行右缘。
    fn render_right_menu_row(
        &self,
        ix: usize,
        page: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (label, icon, shortcut, disabled, tab) = Self::right_menu_items()[ix];
        let chips =
            shortcut.and_then(|action| self.render_shortcut_chips(action, page, window, cx));
        h_flex()
            .id(("right-menu-item", ix))
            .w_full()
            .px_2()
            .gap_2()
            .map(|this| if page { this.py_2() } else { this.py_1p5() })
            .rounded(cx.theme().radius)
            .when(disabled, |this| this.opacity(0.5))
            .when(!disabled, |this| {
                this.cursor_pointer()
                    .hover(|this| this.bg(cx.theme().accent))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.right_menu_open = false;
                        if let Some(tab) = tab {
                            this.open_right_tab(tab, cx);
                        }
                    }))
            })
            .child(Icon::new(icon).size_4().text_color(if disabled {
                cx.theme().muted_foreground
            } else {
                cx.theme().foreground
            }))
            .child(
                div()
                    .when(page, |this| this.text_xs())
                    .when(!page, |this| this.text_sm())
                    .flex_1()
                    .whitespace_nowrap()
                    .text_color(if disabled {
                        cx.theme().muted_foreground
                    } else {
                        cx.theme().foreground
                    })
                    .child(label),
            )
            .when_some(chips, |this, chips| this.child(chips))
            .into_any_element()
    }

    /// 面板首页（菜单页）：展开面板但没有打开的 tab 时显示——
    /// 改动/浏览器/终端/侧边聊天四项（ZCode 同款，相当于面板的首页）。
    /// 宽松大行整列居中：行宽上限 320、名称贴左键帽贴右；上限固定，
    /// 面板拖宽时行不晃。
    fn render_right_menu_page(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .size_full()
            .justify_center()
            .items_center()
            .child(
                v_flex()
                    .w_full()
                    .max_w(px(280.))
                    .px_2()
                    .gap_1()
                    .children((0..4).map(|ix| self.render_right_menu_row(ix, true, window, cx))),
            )
            .into_any_element()
    }

    /// 标签页栏 "+" 的加面板菜单：deferred 到窗口层绘制，`Positioner::side(Bottom)`
    /// 锚定 "+" 按钮正下方（gpui-kit 的 dropdown_menu 走 corner 锚定，BottomRight
    /// 会把菜单弹到按钮上方、超出窗口顶部；且弹层盖住标题栏 HTCAPTION 拖拽区时
    /// 点击会被系统的窗口移动模态循环吞掉——故自绘，与 turn 导航条预览卡同一模式）。
    fn render_right_menu_dropdown(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let bounds = self.tab_add_btn_bounds.get();
        deferred(
            Positioner::side(bounds)
                .placement(Placement::Bottom)
                .align(Align::End)
                .offset(px(6.))
                .margin(px(8.))
                .occlude()
                .child(
                    v_flex()
                        .id("right-menu")
                        .w(px(220.))
                        .p_1()
                        .bg(cx.theme().popover)
                        .border_1()
                        .border_color(cx.theme().border)
                        .rounded_lg()
                        .shadow_lg()
                        .on_mouse_down_out(cx.listener(|this, event: &MouseDownEvent, _, cx| {
                            this.right_menu_open = false;
                            this.right_menu_outside_close = Some(event.position);
                            cx.notify();
                        }))
                        .children(
                            (0..4).map(|ix| self.render_right_menu_row(ix, false, window, cx)),
                        ),
                ),
        )
        .with_priority(1)
        .into_any_element()
    }

    /// 右侧标签页栏的单个 tab：图标 + 名称 + 关闭按钮（点击激活，× 关闭）
    fn render_right_tab(&self, tab: RightTab, cx: &mut Context<Self>) -> AnyElement {
        let active = self.right_active == Some(tab);
        h_flex()
            .id(("right-tab", tab as usize))
            .gap_2()
            .pl_3()
            .pr_1()
            .py_1()
            .rounded(cx.theme().radius)
            .cursor_pointer()
            .when(active, |this| this.bg(cx.theme().accent))
            .when(!active, |this| {
                this.text_color(cx.theme().muted_foreground)
                    .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
            })
            .child(Icon::new(tab.icon()).size_3p5())
            .child(div().text_sm().child(tab.label()))
            .child(
                div()
                    .id(("right-tab-close", tab as usize))
                    .p(px(1.))
                    .rounded(cx.theme().radius)
                    .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
                    .child(
                        Icon::new(IconName::Close)
                            .size_3()
                            .text_color(cx.theme().muted_foreground),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.close_right_tab(tab, cx);
                    })),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.right_active = Some(tab);
                this.right_open = true;
                cx.notify();
            }))
            .into_any_element()
    }

    /// 右侧面板顶部的标签页栏：tab 列表 + 末尾 "+"（加 tab 菜单）与收起按钮
    fn render_right_tab_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .w_full()
            .flex_shrink_0()
            .h(px(36.))
            .pl_2()
            .pr_1()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .children(
                self.right_tabs
                    .iter()
                    .map(|tab| self.render_right_tab(*tab, cx)),
            )
            .child(div().flex_1())
            .child(
                div()
                    .id("right-tab-add-btn")
                    .on_prepaint({
                        let cell = self.tab_add_btn_bounds.clone();
                        move |bounds, _, _| cell.set(bounds)
                    })
                    .child(
                        Button::new("right-tab-add")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Plus)
                            .on_click(cx.listener(|this, event: &ClickEvent, _, cx| {
                                this.toggle_right_menu(event, cx);
                            })),
                    ),
            )
            .child(
                Button::new("right-panel-collapse")
                    .ghost()
                    .xsmall()
                    .icon(IconName::PanelRightClose)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.right_open = false;
                        cx.notify();
                    })),
            )
    }

    /// 自测用。
    pub fn debug_config(&self) -> Option<&pig_protocol::AppConfig> {
        self.config.as_ref()
    }

    fn sync_hero_mode(&mut self, cx: &mut Context<Self>) {
        let hero = self.is_hero(cx) && self.pending_first_send.is_none();
        self.composer
            .update(cx, |composer, cx| composer.set_hero_mode(hero, cx));
        if hero {
            self.push_hero_info(cx);
        }
    }

    /// 自测用。
    pub fn debug_is_hero(&self, cx: &App) -> bool {
        self.is_hero(cx)
    }

    fn render_hero(&self, cx: &mut Context<Self>) -> AnyElement {
        let hour = time::OffsetDateTime::now_local()
            .map(|t| t.hour() as i64)
            .unwrap_or_else(|_| {
                let secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0) as i64;
                (secs / 3600 + 8) % 24
            });
        let greeting = match hour {
            5..=11 => "上午好呀",
            12..=17 => "下午好呀",
            _ => "晚上好呀",
        };

        let chips: Vec<(&'static str, &'static str)> = vec![
            (
                "总结这个工作区",
                "请阅读 README 并总结这个工作区的结构和主要模块。",
            ),
            ("修复一个报错", "我遇到了一个报错："),
            ("写单元测试", "请为主要模块写单元测试。"),
        ];

        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_5()
            .child(
                div()
                    .text_xl()
                    .font_semibold()
                    .child(format!("{greeting}，接下来交给我吧")),
            )
            .child(
                div()
                    .w_full()
                    .max_w(px(720.))
                    .px_4()
                    .child(self.composer.clone()),
            )
            .child(
                h_flex()
                    .gap_2()
                    .children(chips.into_iter().map(|(label, fill)| {
                        let composer = self.composer.clone();
                        h_flex()
                            .id(("chip", label.len()))
                            .px_3()
                            .py_1()
                            .rounded_full()
                            .border_1()
                            .border_color(cx.theme().border)
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .cursor_pointer()
                            .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
                            .child(label)
                            .on_click(move |_, window, cx| {
                                composer.update(cx, |composer, cx| {
                                    composer.fill_text(fill, window, cx);
                                });
                            })
                    })),
            )
            .when_some(self.hero_error.clone(), |this, error| {
                this.child(div().text_xs().text_color(cx.theme().danger).child(error))
            })
            .into_any_element()
    }

    /// 中心区内容（dock center 面板调用）：hero / 会话列 / 空提示 + 换页动画
    fn render_center(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let hero = self.is_hero(cx) && self.pending_first_send.is_none();
        let current_views = self.current.as_ref().and_then(|id| self.views.get(id));

        let center: AnyElement = if hero {
            self.render_hero(cx)
        } else if let Some(views) = current_views {
            v_flex()
                .size_full()
                .child(div().flex_1().min_h_0().child(views.thread.clone()))
                .child(self.composer.clone())
                .into_any_element()
        } else {
            v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("新建任务或从左侧选择会话"),
                )
                .into_any_element()
        };

        let page_tag = if hero {
            "hero".to_string()
        } else {
            self.current.clone().unwrap_or_default()
        };
        div()
            .size_full()
            // 不透明底：盖住左 dock 把手自带线的跑偏——它画在分界线右 2px 的
            // 中心区里（把手内容区被 padding 挤到元素外），中心区后绘制直接覆盖
            .bg(cx.theme().background)
            .with_animation(
                format!("page-{page_tag}"),
                Animation::new(std::time::Duration::from_millis(150)).with_easing(ease_out_quint()),
                |el, delta| el.opacity(delta),
            )
            .child(center)
            .into_any_element()
    }

    /// 右 dock 面板内容：tab 栏 +（有激活 tab 显示其内容，没有则显示面板首页/菜单页）
    fn render_right_dock_content(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let current_views = self.current.as_ref().and_then(|id| self.views.get(id));
        let content: AnyElement = match self.right_active {
            Some(RightTab::Changes) => match current_views {
                Some(views) => views.review.clone().into_any_element(),
                None => v_flex()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child("开始会话后，这里会显示工作区改动"),
                    )
                    .into_any_element(),
            },
            None => self.render_right_menu_page(window, cx),
        };
        v_flex()
            .size_full()
            // 分隔线由 dock 把手自带线绘制（与侧栏一致，不再自画 border_l）
            .child(self.render_right_tab_bar(cx))
            .child(div().flex_1().min_h_0().child(content))
            .into_any_element()
    }

    /// dock 拖宽把手：gpui-base 自带把手的命中区只有 1px 宽（`w(HANDLE_SIZE)`
    /// 是 border-box，4px padding 吃掉内容区），左 dock（Side::Left 特例）还
    /// 左偏 1px 压不到线上，且左把手的可见线被 dock 框架 overflow_hidden 裁掉
    /// （右把手线恰好落在分界线上）——左右一有一无，不对称。自绘 8px 热区
    /// 骑跨分界线 + 居中 1px 分隔线（静止 border 色 / hover 提亮 / 拖拽高亮），
    /// 按下后由根容器的 on_mouse_move 驱动 set_dock_size；宽度仍受
    /// clamp_dock_widths 约束。0.6.7 把手渲染重做（#3175/#3200）后复核移除。
    fn render_dock_resize_strip(
        &self,
        placement: DockPlacement,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let dock = self.dock.read(cx);
        if !dock.is_dock_open(placement) {
            return None;
        }
        let size = dock.dock_size(placement)?;
        let area_w = dock.bounds().size.width;
        // 取整到整像素：拖动产生的小数位置会让 1px 分隔线抗锯齿发虚显粗
        let left = match placement {
            DockPlacement::Left => size - px(4.),
            DockPlacement::Right => area_w - size - px(4.),
            _ => return None,
        };
        let left = px(f32::from(left).round());
        if left < px(0.) {
            return None; // 首帧未测量/极窄
        }
        let active = self.dock_resizing == Some(placement);
        let group = match placement {
            DockPlacement::Left => "dock-resize-left",
            _ => "dock-resize-right",
        };
        // 上游把手自带线会跑偏（左 dock 的线落在缝左 1~2px 的侧栏里），热区用
        // 两侧面板底色铺满把它整个盖住，只留中间我们自己的 1px 线
        let (cover_l, cover_r) = match placement {
            DockPlacement::Left => (cx.theme().sidebar, cx.theme().background),
            _ => (cx.theme().background, cx.theme().background),
        };
        Some(
            div()
                .id(("dock-resize", placement as usize))
                .absolute()
                .top_0()
                .bottom_0()
                .left(left)
                .w(px(8.))
                .cursor_col_resize()
                .occlude()
                .group(group)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        this.dock_resizing = Some(placement);
                        cx.notify();
                    }),
                )
                .child(
                    h_flex()
                        .size_full()
                        .child(div().w(px(4.)).h_full().bg(cover_l))
                        .child(
                            div()
                                .w(px(1.))
                                .h_full()
                                .bg(if active {
                                    cx.theme().ring
                                } else {
                                    cx.theme().border
                                })
                                // hover/拖拽用 ring（焦点环色）：暗色下 accent 比
                                // border 还暗，hover 会像"变更暗/没效果"
                                .when(!active, |this| {
                                    this.group_hover(group, |this| {
                                        this.bg(cx.theme().ring.opacity(0.7))
                                    })
                                }),
                        )
                        .child(div().w(px(3.)).h_full().bg(cover_r)),
                )
                .into_any_element(),
        )
    }

    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_dark = cx.theme().mode.is_dark();
        let title = self
            .current
            .as_ref()
            .and_then(|id| self.metas.iter().find(|m| &m.id == id))
            .map(|m| m.title.clone())
            .unwrap_or_else(|| "pig-code".to_string());

        // Windows 上标题栏命中 HTCAPTION：左键按下仍会派发 MouseDownEvent，但抬起被
        // OS 的窗口移动模态循环吞掉，窗口级文本选择一旦开始手势就收不到结束，
        // 之后移动鼠标会变成拖选。按下标题栏时抑制选择，手势便永不开始。
        div()
            .id("title-bar-selection-guard")
            .w_full()
            .flex_shrink_0()
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                GlobalState::suppress_text_selection(cx);
            })
            .child(
                TitleBar::new()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("toggle-sidebar")
                                    .ghost()
                                    .small()
                                    .occlude()
                                    .icon(IconName::PanelLeft)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.sidebar_collapsed = !this.sidebar_collapsed;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .font_semibold()
                                    .child(format!("pig-code · {title}")),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .px_2()
                            .when(self.git_branch.is_some(), |this| {
                                this.child(
                                    div()
                                        .on_prepaint({
                                            let cell = self.title_branch_btn_bounds.clone();
                                            move |bounds, _, _| cell.set(bounds)
                                        })
                                        .child(
                                            Button::new("title-branch")
                                                .ghost()
                                                .small()
                                                .occlude()
                                                .label(format!(
                                                    "⎇ {}",
                                                    self.git_branch.as_deref().unwrap_or_default()
                                                ))
                                                .on_click(cx.listener(
                                                    |this, event: &ClickEvent, _, cx| {
                                                        // 菜单打开时点 chip：按下先触发
                                                        // outside-close（记录位置），同一次按压
                                                        // 的 click 按位置吞掉，避免收起又弹开
                                                        let down_pos = match event {
                                                            ClickEvent::Mouse(e) => {
                                                                Some(e.down.position)
                                                            }
                                                            _ => None,
                                                        };
                                                        if this
                                                            .title_branch_outside_close
                                                            .take()
                                                            .is_some_and(|pos| {
                                                                Some(pos) == down_pos
                                                            })
                                                        {
                                                            return;
                                                        }
                                                        this.title_branch_menu_open =
                                                            !this.title_branch_menu_open;
                                                        cx.notify();
                                                    },
                                                )),
                                        ),
                                )
                            })
                            .child(
                                Button::new("toggle-theme")
                                    .ghost()
                                    .small()
                                    .occlude()
                                    .icon(if is_dark {
                                        IconName::Sun
                                    } else {
                                        IconName::Moon
                                    })
                                    .on_click(move |_, _, cx| {
                                        Theme::change(
                                            if is_dark {
                                                ThemeMode::Light
                                            } else {
                                                ThemeMode::Dark
                                            },
                                            None,
                                            cx,
                                        );
                                        cx.set_global(ThemeFollowSystem(false));
                                    }),
                            )
                            .child(
                                Button::new("open-settings")
                                    .ghost()
                                    .small()
                                    .occlude()
                                    .icon(IconName::Settings)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.open_settings(cx);
                                    })),
                            )
                            .child(
                                Button::new("right-panel-menu")
                                    .ghost()
                                    .small()
                                    .occlude()
                                    .icon(if self.right_open {
                                        IconName::PanelRightClose
                                    } else {
                                        IconName::PanelRight
                                    })
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.toggle_right_panel(cx);
                                    })),
                            ),
                    ),
            )
            .when(self.title_branch_menu_open, |this| {
                this.child(self.render_branch_menu(cx))
            })
    }

    /// 标题栏分支切换菜单：deferred 到窗口层，锚定分支 chip 正下方
    /// （与标签页 "+" 菜单同一模式）。当前分支高亮，点击其他分支 checkout。
    fn render_branch_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let current = self.git_branch.clone().unwrap_or_default();
        deferred(
            Positioner::side(self.title_branch_btn_bounds.get())
                .placement(Placement::Bottom)
                .align(Align::End)
                .offset(px(6.))
                .margin(px(8.))
                .occlude()
                .child(
                    v_flex()
                        .id("title-branch-menu")
                        .w(px(240.))
                        .max_h(px(360.))
                        .overflow_y_scroll()
                        .p_1()
                        .bg(cx.theme().popover)
                        .border_1()
                        .border_color(cx.theme().border)
                        .rounded_lg()
                        .shadow_lg()
                        .on_mouse_down_out(cx.listener(|this, event: &MouseDownEvent, _, cx| {
                            this.title_branch_menu_open = false;
                            this.title_branch_outside_close = Some(event.position);
                            cx.notify();
                        }))
                        .child(
                            div()
                                .px_2()
                                .py_1()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("切换分支"),
                        )
                        .children(self.title_branches.iter().map(|branch| {
                            let branch = branch.clone();
                            let is_current = branch == current;
                            div()
                                .id(gpui_kit::SharedString::from(branch.clone()))
                                .px_2()
                                .py_1()
                                .rounded(cx.theme().radius)
                                .text_sm()
                                .cursor_pointer()
                                .when(is_current, |this| this.bg(cx.theme().accent))
                                .when(!is_current, |this| {
                                    this.hover(|h| h.bg(cx.theme().accent.opacity(0.6)))
                                })
                                .child(
                                    h_flex()
                                        .w_full()
                                        .justify_between()
                                        .gap_2()
                                        .child(
                                            div()
                                                .text_color(if is_current {
                                                    cx.theme().primary
                                                } else {
                                                    cx.theme().foreground
                                                })
                                                .child(branch.clone()),
                                        )
                                        .when(is_current, |this| {
                                            this.child(
                                                Icon::new(IconName::Check)
                                                    .size_3()
                                                    .text_color(cx.theme().primary),
                                            )
                                        }),
                                )
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.title_branch_menu_open = false;
                                    if let Some(cwd) = this.current_cwd() {
                                        this.agent.checkout_branch(cwd, branch.clone());
                                    }
                                    cx.notify();
                                }))
                        })),
                ),
        )
        .with_priority(1)
        .into_any_element()
    }
}

fn event_session_id(event: &Event) -> Option<String> {
    match event {
        Event::SessionConfigured { session_id, .. }
        | Event::UserMessage { session_id, .. }
        | Event::TurnStarted { session_id, .. }
        | Event::ReasoningDelta { session_id, .. }
        | Event::TextDelta { session_id, .. }
        | Event::TextDone { session_id, .. }
        | Event::ToolCallBegin { session_id, .. }
        | Event::ToolCallEnd { session_id, .. }
        | Event::ContextUsage { session_id, .. }
        | Event::ContextCompacted { session_id, .. }
        | Event::ApprovalRequested { session_id, .. }
        | Event::QuestionRequested { session_id, .. }
        | Event::FileChanged { session_id, .. }
        | Event::FileReverted { session_id, .. }
        | Event::TurnComplete { session_id, .. }
        | Event::TurnAborted { session_id, .. }
        | Event::TurnFileChanges { session_id, .. }
        | Event::MessageQueued { session_id, .. }
        | Event::TodoListChanged { session_id, .. }
        | Event::TaskListChanged { session_id, .. }
        | Event::ExecModeChanged { session_id, .. }
        | Event::FileSearchResults { session_id, .. } => Some(session_id.clone()),
        Event::SessionList { .. }
        | Event::SessionTitleChanged { .. }
        | Event::GitInfo { .. }
        | Event::BranchChanged { .. }
        | Event::GitStatus { .. }
        | Event::GitDiff { .. }
        | Event::ConfigSnapshot { .. }
        | Event::TestResult { .. }
        | Event::ModelInfo { .. }
        | Event::WorkspaceList { .. } => None,
        Event::Error { session_id, .. } => session_id.clone(),
    }
}

impl Render for AppView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // dock 开合同步：AppView 的标志位是唯一事实源（dock 不持久化显隐状态）
        let left_open = self.dock.read(cx).is_dock_open(DockPlacement::Left);
        if left_open == self.sidebar_collapsed {
            self.dock.update(cx, |dock, cx| {
                dock.toggle_dock(DockPlacement::Left, window, cx)
            });
        }
        let right_open = self.dock.read(cx).is_dock_open(DockPlacement::Right);
        if right_open != self.right_open {
            self.dock.update(cx, |dock, cx| {
                dock.toggle_dock(DockPlacement::Right, window, cx)
            });
        }

        // 三栏最小宽度补钳：拖拽/window 缩放得越界宽度在 paint 前拉回。
        // 开合同步刚执行完，is_dock_open 读的已是新值
        let (area_w, left_w, right_w, left_open, right_open) = {
            let dock = self.dock.read(cx);
            (
                f32::from(dock.bounds().size.width),
                dock.dock_size(DockPlacement::Left)
                    .map(f32::from)
                    .unwrap_or(0.),
                dock.dock_size(DockPlacement::Right)
                    .map(f32::from)
                    .unwrap_or(0.),
                dock.is_dock_open(DockPlacement::Left),
                dock.is_dock_open(DockPlacement::Right),
            )
        };
        let (new_left, new_right) =
            clamp_dock_widths(area_w, left_w, right_w, left_open, right_open);
        if new_left != left_w || new_right != right_w {
            self.dock.update(cx, |dock, cx| {
                if new_left != left_w {
                    dock.set_dock_size(DockPlacement::Left, px(new_left), window, cx);
                }
                if new_right != right_w {
                    dock.set_dock_size(DockPlacement::Right, px(new_right), window, cx);
                }
            });
        }

        v_flex()
            .id("app-root")
            .key_context("app")
            .on_action(cx.listener(|this, _: &NewTask, _, cx| {
                this.hero_cwd = None;
                this.enter_hero(cx);
            }))
            .on_action(cx.listener(|this, _: &FocusSearch, window, cx| {
                this.sidebar
                    .update(cx, |sidebar, cx| sidebar.open_search(window, cx));
            }))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| {
                this.sidebar_collapsed = !this.sidebar_collapsed;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleChanges, _, cx| {
                this.toggle_right_tab(RightTab::Changes, cx);
            }))
            // 自愈兜底：选择手势的结束依赖收到 MouseUpEvent，而某些系统级按压
            // （HTCAPTION、边框缩放）收不到。未按键的移动说明手势早已结束。
            // dock 拖宽同理：松手后没收着 up 时，首个未按键 move 清掉 dock_resizing。
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                if event.pressed_button.is_none() {
                    gpui_kit::base::TextSelection::end(window, cx);
                    this.dock_resizing = None;
                }
                if let Some(placement) = this.dock_resizing {
                    let area = this.dock.read(cx).bounds();
                    let size = match placement {
                        DockPlacement::Left => event.position.x - area.left(),
                        DockPlacement::Right => area.right() - event.position.x,
                        _ => return,
                    };
                    // 取整：小数宽度会让分界线和面板内容抗锯齿发虚
                    let size = px(f32::from(size).round());
                    this.dock.update(cx, |dock, cx| {
                        dock.set_dock_size(placement, size, window, cx);
                    });
                }
            }))
            .size_full()
            .child(self.render_title_bar(cx))
            .child(div().flex_1().min_h_0().child(if self.settings_open {
                self.settings.clone().into_any_element()
            } else {
                div()
                    .size_full()
                    .relative()
                    .child(self.dock.clone())
                    .children(
                        [
                            self.render_dock_resize_strip(DockPlacement::Left, cx),
                            self.render_dock_resize_strip(DockPlacement::Right, cx),
                        ]
                        .into_iter()
                        .flatten(),
                    )
                    .into_any_element()
            }))
            // 标签页栏 "+" 的加面板菜单：deferred 到窗口层，锚定 "+" 正下方
            .when(self.right_menu_open, |this| {
                this.child(self.render_right_menu_dropdown(window, cx))
            })
            // Yolo 确认框：最后渲染 = 最顶层（覆盖 settings/dock/hero 全部内容）
            .when(self.yolo_confirm_open, |this| {
                this.child(self.render_yolo_confirm(cx))
            })
    }
}

/// 自测环境：mock provider + 临时配置/工作目录 + 隔离数据目录。
struct SelftestEnv {
    config_path: PathBuf,
    cwd: PathBuf,
}

fn setup_selftest() -> SelftestEnv {
    let port = pig_core::mock::start_mock_server();
    let dir = std::env::temp_dir().join(format!("pig-app-selftest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create selftest dir");
    std::fs::write(
        dir.join(pig_core::mock::MOCK_FILE_NAME),
        pig_core::mock::MOCK_FILE_CONTENT,
    )
    .expect("write mock file");
    let data_dir = dir.join("data");
    std::fs::create_dir_all(&data_dir).expect("create data dir");
    // agent 通过 PIG_DATA_DIR 找到隔离数据目录
    unsafe { std::env::set_var("PIG_DATA_DIR", &data_dir) };
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"default_provider = "mock"
default_model = "mock-model"

[[providers]]
id = "mock"
name = "Mock"
base_url = "http://127.0.0.1:{port}/v1"
api_key = "mock-key"
api_format = "OpenAiChat"
enabled = true

[[providers.models]]
id = "mock-model"
context_window = 128000
max_output_tokens = 8192
reasoning_levels = ["low", "high"]
default_reasoning_level = "low"

[[providers]]
id = "anthropic"
name = "AnthropicMock"
base_url = "http://127.0.0.1:{port}/v1"
api_key = "mock-key"
api_format = "AnthropicMessages"
enabled = true

[[providers.models]]
id = "mock-model"
context_window = 200000
max_output_tokens = 8192
reasoning_levels = ["high", "max"]
"#
        ),
    )
    .expect("write config");
    SelftestEnv {
        config_path,
        cwd: dir,
    }
}

fn main() {
    // PIG_NET_TEST=1：不开窗口，用真实配置逐个测试供应商连通性（网络排障用）
    // PIG_NET_TEST=full：再走一遍完整发消息链路（含系统提示词与工具），打印事件流
    if let Some(mode) = std::env::var_os("PIG_NET_TEST") {
        if mode == "full" {
            pig_core::net_test_full_turn(None);
        } else {
            pig_core::provider::net_test_blocking(&pig_core::config::default_path());
        }
        return;
    }

    let selftest = std::env::var_os("PIG_SELFTEST").is_some();
    let setup = selftest.then(setup_selftest);
    let cwd = setup
        .as_ref()
        .map(|s| s.cwd.clone())
        .unwrap_or_else(|| std::env::current_dir().expect("cwd"));
    let config_path = setup.map(|s| s.config_path);

    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            cx.set_global(ThemeFollowSystem(true));

            cx.bind_keys([
                KeyBinding::new("ctrl-n", NewTask, None),
                KeyBinding::new("ctrl-k", FocusSearch, None),
                KeyBinding::new("ctrl-b", ToggleSidebar, None),
                // 右侧面板：改动可用；浏览器/侧边聊天先绑键让菜单展示快捷键，功能后续加
                KeyBinding::new("ctrl-shift-g", ToggleChanges, None),
                KeyBinding::new("ctrl-t", ToggleBrowser, None),
                KeyBinding::new("alt-ctrl-b", ToggleSideChat, None),
                KeyBinding::new("escape", CloseSearch, Some("search")),
                KeyBinding::new("escape", CloseSettings, Some("settings")),
            ]);

            // 初始窗口不超出显示器可用区域：GPUI 的尺寸是逻辑像素，缩放下
            // 1280x800 可能比实际屏幕还大，底部会被任务栏挡住；给边框和任务栏留余量。
            let mut window_size = size(px(1280.), px(800.));
            if let Some(display) = cx.primary_display() {
                let bounds = display.bounds();
                window_size = size(
                    window_size.width.min(bounds.size.width - px(32.)),
                    window_size.height.min(bounds.size.height - px(96.)),
                );
            }
            let window_bounds = WindowBounds::centered(window_size, cx);

            cx.spawn(async move |cx| {
                let options = WindowOptions {
                    window_bounds: Some(window_bounds),
                    window_min_size: Some(size(px(960.), px(600.))),
                    ..TitleBar::window_options()
                };

                cx.open_window(options, |window, cx| {
                    // gpui-kit init 固定为亮色，开窗时按系统外观覆盖
                    Theme::sync_system_appearance(Some(window), cx);
                    let view =
                        cx.new(|cx| AppView::new(window, cx, config_path.clone(), cwd.clone()));
                    // AppView 实体就位后安装 dock 布局（面板持有 AppView 的 weak 引用）
                    view.update(cx, |app, cx| app.install_dock(window, cx));
                    if selftest {
                        let view = view.clone();
                        cx.spawn(async move |cx| {
                            run_selftest(view, cx).await;
                        })
                        .detach();
                    }
                    cx.new(|cx| Root::new(view, window, cx))
                })
                .expect("Failed to open window");
            })
            .detach();
        });
}

/// PIG_SELFTEST=1：会话A完整修改链 → 会话B并行对话 → 切回A → 模拟重启 resume → @搜索。
async fn run_selftest(view: Entity<AppView>, cx: &mut AsyncApp) {
    use std::time::Duration;
    macro_rules! timer {
        ($ms:expr) => {
            cx.background_executor().timer(Duration::from_millis($ms))
        };
    }
    macro_rules! app {
        ($f:expr) => {
            view.update(cx, $f)
        };
    }

    timer!(800).await;

    // hero 态断言：无会话、hero 展示
    let is_hero = app!(|app: &mut AppView, cx| app.debug_is_hero(cx));
    assert!(is_hero, "启动应进入 hero 态");
    println!("[selftest] hero 态 OK");

    // hero 发送首条消息 → 自动建会话
    app!(|app: &mut AppView, cx| {
        app.exec_mode = pig_protocol::ExecMode::ConfirmBeforeEdit;
        app.hero_send(
            format!(
                "{} 创建并修改文件，然后跑个命令",
                pig_core::mock::SCENARIO_B_TRIGGER
            ),
            vec![],
            pig_protocol::ExecMode::ConfirmBeforeEdit,
            cx,
        );
    });
    let session_a = loop {
        timer!(200).await;
        let current = app!(|app: &mut AppView, _| app.current.clone());
        if let Some(id) = current {
            break id;
        }
    };
    println!("[selftest] hero 发送 → 会话 A 建立: {session_a}");

    // 会话 A：场景 B（审批×3）
    let mut approvals = 0u32;
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 60_000, "会话 A 回合超时");
        let approved = app!(|app: &mut AppView, cx| {
            let views = app.views.get(&session_a)?;
            let pending = views.thread.read(cx).pending_approval();
            pending.map(|_| {
                views.thread.update(cx, |thread, cx| {
                    thread.decide_pending(pig_protocol::ApprovalDecision::Allow, cx);
                });
            })
        });
        if approved == Some(()) {
            approvals += 1;
            continue;
        }
        let done = app!(|app: &mut AppView, cx| {
            let views = app.views.get(&session_a)?;
            let streaming = views.thread.read(cx).is_streaming();
            let (tool_done, text, _, tool_output) = views.thread.read(cx).debug_last_assistant();
            if !streaming && waited > 1000 && tool_done {
                assert!(
                    text.contains(pig_core::mock::SCENARIO_B_MARKER),
                    "A 文本标记: {text}"
                );
                assert!(tool_output.contains(pig_core::mock::SCENARIO_B_BASH_MARKER));
                return Some(());
            }
            None
        });
        if done == Some(()) {
            break;
        }
    }
    assert_eq!(approvals, 3, "场景 B 三次审批");
    // hero → 会话态切换断言
    let is_hero = app!(|app: &mut AppView, cx| app.debug_is_hero(cx));
    assert!(!is_hero, "发送后应进入会话态");
    println!("[selftest] 会话 A 场景 B 完成（审批×3），输入框已沉底");

    // 工作区视图：会话 cwd 应出现在工作区列表，且按工作区分组正确
    let cwd_str = app!(|app: &mut AppView, _| app.cwd.display().to_string());
    let (has_cwd, grouped) = app!(|app: &mut AppView, cx| {
        let sidebar = app.sidebar.read(cx);
        (
            sidebar.debug_workspaces().contains(&cwd_str),
            sidebar
                .debug_workspace_sessions(&cwd_str)
                .contains(&session_a),
        )
    });
    assert!(has_cwd, "工作区列表应包含会话 cwd");
    assert!(grouped, "工作区视图应按 cwd 分组会话");

    // 添加/移除工作区
    let extra = std::env::temp_dir().join(format!("pig-app-ws-{}", std::process::id()));
    std::fs::create_dir_all(&extra).unwrap();
    let extra_str = extra.display().to_string();
    app!(|app: &mut AppView, _| app.agent.add_workspace(extra.clone()));
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 10_000, "添加工作区超时");
        let has = app!(|app: &mut AppView, cx| {
            app.sidebar.read(cx).debug_workspaces().contains(&extra_str)
        });
        if has {
            break;
        }
    }
    app!(|app: &mut AppView, _| app.agent.remove_workspace(extra.clone()));
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 10_000, "移除工作区超时");
        let has = app!(|app: &mut AppView, cx| {
            app.sidebar.read(cx).debug_workspaces().contains(&extra_str)
        });
        if !has {
            break;
        }
    }
    println!("[selftest] 工作区列表 OK（会话 cwd 自动出现 + 手动增删）");

    // 会话 B：新建 + 场景 A
    app!(|app: &mut AppView, _| app
        .agent
        .new_session(app.cwd.clone(), None, None, None, None));
    let session_b = loop {
        timer!(200).await;
        let current = app!(|app: &mut AppView, _| app.current.clone());
        if let Some(id) = current {
            if id != session_a {
                break id;
            }
        }
    };
    println!("[selftest] 会话 B 就绪: {session_b}");
    app!(|app: &mut AppView, _| {
        app.agent.send_message(
            session_b.clone(),
            "读一下 README.mock.md 并总结".to_string(),
            vec![],
            pig_protocol::ExecMode::AutoEdit,
        );
    });
    timer!(300).await; // 等第一个回合开跑
    app!(|app: &mut AppView, _| {
        app.agent.send_message(
            session_b.clone(),
            "ECHO_HISTORY".to_string(),
            vec![],
            pig_protocol::ExecMode::AutoEdit,
        );
    });
    let mut saw_queued = false;
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 60_000, "排队流程超时");
        let (queued_len, streaming, text) = app!(|app: &mut AppView, cx| {
            let Some(views) = app.views.get(&session_b) else {
                return (0, false, String::new());
            };
            let thread = views.thread.read(cx);
            let (_, text, _, _) = thread.debug_last_assistant();
            (thread.debug_queued().len(), thread.is_streaming(), text)
        });
        saw_queued |= queued_len > 0;
        if !streaming && text.contains("HISTORY_COUNT:6") {
            break;
        }
    }
    assert!(saw_queued, "应出现排队芯片");
    println!("[selftest] 会话 B 完成，消息排队 OK（自动接续，第二轮历史=6）");

    // turn 导航条：会话 B 有 2 轮用户消息，面板已绘制（宽度非零）且达到断点
    let (nav_turns, nav_pane_w) = app!(|app: &mut AppView, cx| {
        let views = app.views.get(&session_b).expect("B 视图在内存");
        views.thread.read(cx).debug_nav_state()
    });
    assert!(nav_turns >= 2, "会话 B 应有 ≥2 轮用户消息");
    assert!(
        nav_pane_w >= 720.,
        "消息面板宽 {nav_pane_w} 应 ≥720（导航条断点）"
    );
    println!("[selftest] turn 导航条可见条件 OK（{nav_turns} 轮，面板宽 {nav_pane_w:.0}）");

    // 贴底时活动项应为最后一轮用户消息（回归：曾按「离视口顶最近」在贴底时
    // 高亮到更早轮次——底部视口里多条用户消息同时可见，离顶最近的偏早）
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        let (active, offset_y, max_offset_y, view_h, user_rows) = app!(|app: &mut AppView, cx| {
            let views = app.views.get(&session_b).expect("B 视图在内存");
            views.thread.read(cx).debug_nav_active_detail()
        });
        let last_ix = user_rows.last().map(|(ix, _, _)| *ix);
        if last_ix.is_some() && active == last_ix {
            break;
        }
        assert!(
            waited < 10_000,
            "贴底时活动项应为最后一轮: active={active:?} last={last_ix:?} \
             offset_y={offset_y:.1} max_offset_y={max_offset_y:.1} 视口高={view_h:.1} \
             用户行={user_rows:?}"
        );
    }
    println!("[selftest] turn 导航条活动项 OK（贴底 = 最后一轮）");

    // 切回 A：内存状态应原样保留
    app!(|app: &mut AppView, cx| app.switch_session(session_a.clone(), cx));
    let current = app!(|app: &mut AppView, _| app.current.clone());
    assert_eq!(current.as_ref(), Some(&session_a));
    let kept = app!(|app: &mut AppView, cx| {
        let views = app.views.get(&session_a).expect("A 视图在内存");
        let (_, text, _, _) = views.thread.read(cx).debug_last_assistant();
        text.contains(pig_core::mock::SCENARIO_B_MARKER)
    });
    assert!(kept, "切回 A 后内容应保留");
    println!("[selftest] 会话切换 OK");

    // 模拟重启：drop manager 重 spawn + ListSessions/OpenSession 重放
    app!(|app: &mut AppView, cx| app.restart_agent(cx));
    // 等自动打开最近会话（B，updated_at 最新），再显式切到 A 触发重放
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 10_000, "重启后自动打开会话超时");
        let ready = app!(|app: &mut AppView, _| app.current.is_some());
        if ready {
            break;
        }
    }
    app!(|app: &mut AppView, cx| app.switch_session(session_a.clone(), cx));
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 30_000, "重启后重放超时");
        let done = app!(|app: &mut AppView, cx| {
            let views = app.views.get(&session_a)?;
            let (tool_done, text, _, _) = views.thread.read(cx).debug_last_assistant();
            let review_state = views.review.read(cx).debug_state();
            (tool_done
                && text.contains(pig_core::mock::SCENARIO_B_MARKER)
                && review_state.0 == 1
                && review_state.1 == 3)
                .then_some(())
        });
        if done == Some(()) {
            break;
        }
    }
    println!("[selftest] 模拟重启 resume OK（消息+工具卡+diff 全恢复）");

    // @搜索：真实文件
    app!(|app: &mut AppView, _| {
        app.agent
            .search_files(session_a.clone(), "hello".to_string());
    });
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 10_000, "@搜索超时");
        let found = app!(|app: &mut AppView, cx| {
            app.composer
                .read(cx)
                .debug_mention_results()
                .iter()
                .any(|r| r.contains("hello.txt"))
        });
        if found {
            break;
        }
    }
    println!("[selftest] @搜索 OK");

    // compact（模型摘要）
    app!(|app: &mut AppView, _| app.agent.compact(session_a.clone()));
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 15_000, "compact 超时");
        let compacted = app!(|app: &mut AppView, cx| {
            app.views
                .get(&session_a)
                .map(|views| views.thread.read(cx).debug_system_notes())
                .unwrap_or_default()
                .iter()
                .any(|note| {
                    note.contains("模型摘要") && note.contains(pig_core::mock::SUMMARY_MARKER)
                })
        });
        if compacted {
            break;
        }
    }
    println!("[selftest] 模型摘要 compact OK");

    // 场景 C：计划模式闭环
    app!(|app: &mut AppView, _| app
        .agent
        .new_session(app.cwd.clone(), None, None, None, None));
    let session_c = loop {
        timer!(200).await;
        let current = app!(|app: &mut AppView, _| app.current.clone());
        if let Some(id) = current {
            if id != session_a {
                break id;
            }
        }
    };
    app!(|app: &mut AppView, _| {
        app.exec_mode = pig_protocol::ExecMode::Plan;
        app.agent
            .set_exec_mode(session_c.clone(), pig_protocol::ExecMode::Plan);
        app.agent.send_message(
            session_c.clone(),
            format!("{} 给我一个改造计划", pig_core::mock::SCENARIO_C_TRIGGER),
            vec![],
            pig_protocol::ExecMode::Plan,
        );
    });
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 30_000, "场景 C 计划超时");
        let ready = app!(|app: &mut AppView, cx| {
            let views = app.views.get(&session_c)?;
            let thread = views.thread.read(cx);
            let (_, text, _, _) = thread.debug_last_assistant();
            (thread.is_plan_pending() && text.contains(pig_core::mock::PLAN_MARKER)).then_some(())
        });
        if ready == Some(()) {
            break;
        }
    }
    println!("[selftest] 计划模式输出计划，执行计划按钮出现");

    // 点「执行计划」（走与按钮相同路径）
    app!(|app: &mut AppView, cx| {
        let views = app.views.get(&session_c).expect("C 视图");
        views
            .thread
            .update(cx, |thread, cx| thread.trigger_execute_plan(cx));
    });
    let mode = app!(|app: &mut AppView, cx| app.composer.read(cx).debug_exec_mode());
    assert_eq!(
        mode,
        pig_protocol::ExecMode::ConfirmBeforeEdit,
        "模式应切到变更前确认"
    );

    // 场景 B 工具链执行（ConfirmBeforeEdit → 3 次审批）
    let mut approvals = 0u32;
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 60_000, "场景 C 执行超时");
        let approved = app!(|app: &mut AppView, cx| {
            let views = app.views.get(&session_c)?;
            views.thread.read(cx).pending_approval().map(|_| {
                views.thread.update(cx, |thread, cx| {
                    thread.decide_pending(pig_protocol::ApprovalDecision::Allow, cx);
                });
            })
        });
        if approved == Some(()) {
            approvals += 1;
            continue;
        }
        let done = app!(|app: &mut AppView, cx| {
            let views = app.views.get(&session_c)?;
            let thread = views.thread.read(cx);
            let (tool_done, text, _, _) = thread.debug_last_assistant();
            (!thread.is_streaming()
                && waited > 1000
                && tool_done
                && text.contains(pig_core::mock::SCENARIO_B_MARKER))
            .then_some(())
        });
        if done == Some(()) {
            break;
        }
    }
    assert_eq!(approvals, 3, "执行计划后场景 B 三次审批");
    println!("[selftest] 计划确认 → 模式切换 → 工具链执行 OK");

    // 水位条
    let usage = app!(|app: &mut AppView, cx| app.composer.read(cx).debug_context_usage());
    assert_eq!(usage, Some((142, 128_000)), "水位条数据: {usage:?}");
    println!("[selftest] 上下文水位条 OK");

    // 设置页数据：ConfigSnapshot 已收到、composer 模型列表已填充
    let (provider_count, model_count) = app!(|app: &mut AppView, cx| {
        (
            app.debug_config().map(|c| c.providers.len()).unwrap_or(0),
            app.composer.read(cx).debug_model_count(),
        )
    });
    assert_eq!(provider_count, 2, "ConfigSnapshot 应含 2 个供应商");
    assert_eq!(model_count, 2, "composer 应列出 2 个模型");
    println!("[selftest] ConfigSnapshot + 模型列表 OK");

    // Anthropic 供应商端到端：会话 D 切到 anthropic 模型跑场景 B
    app!(|app: &mut AppView, _| app
        .agent
        .new_session(app.cwd.clone(), None, None, None, None));
    let session_d = loop {
        timer!(200).await;
        let current = app!(|app: &mut AppView, _| app.current.clone());
        if let Some(id) = current {
            if id != session_a && id != session_b {
                break id;
            }
        }
    };
    app!(|app: &mut AppView, _| {
        app.agent.set_model(
            session_d.clone(),
            "anthropic".to_string(),
            "mock-model".to_string(),
            None,
        );
        app.agent.send_message(
            session_d.clone(),
            format!(
                "{} 创建并修改文件，然后跑个命令",
                pig_core::mock::SCENARIO_B_TRIGGER
            ),
            vec![],
            pig_protocol::ExecMode::AutoEdit,
        );
    });
    let mut approvals = 0u32;
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 60_000, "Anthropic 场景 B 超时");
        let approved = app!(|app: &mut AppView, cx| {
            let views = app.views.get(&session_d)?;
            views.thread.read(cx).pending_approval().map(|_| {
                views.thread.update(cx, |thread, cx| {
                    thread.decide_pending(pig_protocol::ApprovalDecision::Allow, cx);
                });
            })
        });
        if approved == Some(()) {
            approvals += 1;
            continue;
        }
        let done = app!(|app: &mut AppView, cx| {
            let views = app.views.get(&session_d)?;
            let thread = views.thread.read(cx);
            let (tool_done, text, thinking, _) = thread.debug_last_assistant();
            let _ = thinking;
            (!thread.is_streaming()
                && waited > 1000
                && tool_done
                && text.contains(pig_core::mock::SCENARIO_B_MARKER))
            .then_some(())
        });
        if done == Some(()) {
            break;
        }
    }
    assert_eq!(approvals, 1, "AutoEdit 下仅 Bash 审批");
    println!("[selftest] Anthropic 供应商端到端 OK");

    // AskUserQuestion：会话 E 走 SCENARIO_Q → 问题条出现 → 选选项 → 提交 → marker + 工具卡
    app!(|app: &mut AppView, _| app
        .agent
        .new_session(app.cwd.clone(), None, None, None, None));
    let session_e = loop {
        timer!(200).await;
        let current = app!(|app: &mut AppView, _| app.current.clone());
        if let Some(id) = current {
            if id != session_a && id != session_b && id != session_c && id != session_d {
                break id;
            }
        }
    };
    app!(|app: &mut AppView, _| {
        app.agent.send_message(
            session_e.clone(),
            format!("{} 帮我决定实现方案", pig_core::mock::SCENARIO_Q_TRIGGER),
            vec![],
            pig_protocol::ExecMode::AutoEdit,
        );
    });
    // 等问题条出现（问题与审批互斥，问题优先，AutoEdit 下 AskUserQuestion 免审批）
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 30_000, "问题条出现超时");
        let has =
            app!(|app: &mut AppView, cx| { app.composer.read(cx).debug_question().is_some() });
        if has {
            break;
        }
    }
    println!("[selftest] AskUserQuestion 问题条出现 OK");
    // 向导分页：第 1 题选「方案 A」→ 下一题 → 第 2 题选「要」→ 提交
    let q1 = app!(|app: &mut AppView, cx| app.composer.read(cx).debug_question());
    assert_eq!(q1.as_deref(), Some("选择实现方案"), "首题题干: {q1:?}");
    app!(|app: &mut AppView, cx| {
        app.composer.update(cx, |composer, cx| {
            composer.debug_select_question_option(0, 0, cx);
            composer.debug_next_question_page(cx);
        });
    });
    let q2 = app!(|app: &mut AppView, cx| app.composer.read(cx).debug_question());
    assert_eq!(
        q2.as_deref(),
        Some("需要跑测试吗"),
        "翻页后应显示第 2 题: {q2:?}"
    );
    println!("[selftest] AskUserQuestion 翻页 OK");
    app!(|app: &mut AppView, cx| {
        app.composer.update(cx, |composer, cx| {
            composer.debug_select_question_option(1, 0, cx);
            composer.debug_submit_question(cx);
        });
    });
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 30_000, "AskUserQuestion 回合超时");
        let done = app!(|app: &mut AppView, cx| {
            let views = app.views.get(&session_e)?;
            let thread = views.thread.read(cx);
            let (tool_done, text, _, tool_output) = thread.debug_last_assistant();
            if !thread.is_streaming() && waited > 1000 && tool_done {
                assert!(
                    text.contains(pig_core::mock::MOCK_Q_MARKER),
                    "E 文本标记: {text}"
                );
                return Some(tool_output);
            }
            None
        });
        if let Some(tool_output) = done {
            assert!(
                tool_output.contains("方案 A"),
                "工具输出应含第 1 题答案: {tool_output}"
            );
            assert!(
                tool_output.contains("需要跑测试吗：要"),
                "工具输出应含第 2 题答案: {tool_output}"
            );
            break;
        }
    }
    let has_card = app!(|app: &mut AppView, cx| {
        let views = app.views.get(&session_e)?;
        Some(views.thread.read(cx).debug_has_tool_call("AskUserQuestion"))
    });
    assert_eq!(has_card, Some(true), "工具卡应显示 AskUserQuestion");
    println!("[selftest] AskUserQuestion 提问场景 OK");

    // 右侧面板：默认收起 → 面板按钮直开（无 tab 时显示菜单页）→ 开改动 tab →
    // 快捷键再触发收起（tab 保留）→ × 关尽 tab 后面板保持展开、回到菜单页
    let right_initial = app!(|app: &mut AppView, _| app.right_open);
    assert!(!right_initial, "右侧面板默认应收起");
    app!(|app: &mut AppView, cx| app.toggle_right_panel(cx));
    let (open, active) = app!(|app: &mut AppView, _| (app.right_open, app.right_active));
    assert!(open && active.is_none(), "面板展开且无 tab 时应显示菜单页");
    app!(|app: &mut AppView, cx| app.open_right_tab(RightTab::Changes, cx));
    let (open, active) = app!(|app: &mut AppView, _| (app.right_open, app.right_active));
    assert!(open && active == Some(RightTab::Changes), "改动 tab 应打开");
    app!(|app: &mut AppView, cx| app.toggle_right_tab(RightTab::Changes, cx));
    let (open, kept) = app!(|app: &mut AppView, _| {
        (app.right_open, app.right_active == Some(RightTab::Changes))
    });
    assert!(!open && kept, "再次触发应收起面板并保留 tab");
    app!(|app: &mut AppView, cx| app.close_right_tab(RightTab::Changes, cx));
    let (open, active, tabs) =
        app!(|app: &mut AppView, _| { (app.right_open, app.right_active, app.right_tabs.len()) });
    assert!(
        !open && active.is_none() && tabs == 0,
        "面板收起状态下关 tab 不改变收起状态；tab 清空"
    );
    // 面板展开时关掉最后一个 tab：面板保持展开、回到菜单页
    app!(|app: &mut AppView, cx| {
        app.open_right_tab(RightTab::Changes, cx);
        app.close_right_tab(RightTab::Changes, cx);
    });
    let (open, active) = app!(|app: &mut AppView, _| (app.right_open, app.right_active));
    assert!(open && active.is_none(), "关尽 tab 后应停在菜单页");
    app!(|app: &mut AppView, cx| app.toggle_right_panel(cx));
    println!("[selftest] 右侧面板开合 OK");

    // 加面板菜单（标签页栏 "+"）：点开打开、再点收起
    app!(|app: &mut AppView, cx| {
        app.toggle_right_menu(&ClickEvent::default(), cx);
    });
    let menu_open = app!(|app: &mut AppView, _| app.right_menu_open);
    assert!(menu_open, "菜单应打开");
    app!(|app: &mut AppView, cx| {
        app.toggle_right_menu(&ClickEvent::default(), cx);
    });
    let menu_closed = app!(|app: &mut AppView, _| !app.right_menu_open);
    assert!(menu_closed, "再点应收起菜单");
    println!("[selftest] 右侧面板菜单 OK");

    // 改动 chip：点击改为直接打开右侧改动面板（不再弹层）
    app!(|app: &mut AppView, cx| {
        app.composer
            .update(cx, |_, cx| cx.emit(ComposerEvent::OpenChanges));
    });
    let (open, active) = app!(|app: &mut AppView, _| (app.right_open, app.right_active));
    assert!(
        open && active == Some(RightTab::Changes),
        "改动 chip 应打开右侧面板并激活改动 tab"
    );
    println!("[selftest] 改动 chip → 右侧改动面板 OK");

    // 会话管理：首条消息自动命名 → 手动重命名 → 删除
    app!(|app: &mut AppView, _| app
        .agent
        .new_session(app.cwd.clone(), None, None, None, None));
    let session_f = loop {
        timer!(200).await;
        let current = app!(|app: &mut AppView, _| app.current.clone());
        if let Some(id) = current
            && id != session_a
            && id != session_b
            && id != session_c
            && id != session_d
            && id != session_e
        {
            break id;
        }
    };
    // 首条消息（≥10 字）触发自动命名 sidecar；mock 回 {"title": MOCK_TITLE}
    app!(|app: &mut AppView, _| {
        app.agent.send_message(
            session_f.clone(),
            "帮我梳理这个项目的模块结构并给出重构建议".to_string(),
            vec![],
            pig_protocol::ExecMode::AutoEdit,
        );
    });
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 30_000, "自动命名超时");
        let done = app!(|app: &mut AppView, cx| {
            let Some(views) = app.views.get(&session_f) else {
                return false;
            };
            let thread = views.thread.read(cx);
            let (_, text, _, _) = thread.debug_last_assistant();
            let title = app
                .metas
                .iter()
                .find(|m| m.id == session_f)
                .map(|m| m.title.clone());
            !thread.is_streaming()
                && waited > 1000
                && !text.is_empty()
                && title.as_deref() == Some(pig_core::mock::MOCK_TITLE)
        });
        if done {
            break;
        }
    }
    println!("[selftest] 首条消息自动命名 OK（mock 标题替换 30 字符种子）");

    // 手动重命名：core 落库（title_custom）后 SessionList 全量刷新回来仍是新名，
    // 才算真正持久化（本地补丁只管即时显示）
    app!(|app: &mut AppView, cx| app.rename_session(&session_f, "手动改名F", cx));
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 10_000, "重命名持久化超时");
        let done = app!(|app: &mut AppView, _| {
            app.metas
                .iter()
                .find(|m| m.id == session_f)
                .map(|m| m.title.as_str())
                == Some("手动改名F")
        });
        if done {
            break;
        }
    }
    println!("[selftest] 会话手动重命名 OK");

    // 删除会话：视图/列表/rollout 文件全清理；删当前会话自动切走
    let data_dir =
        std::path::PathBuf::from(std::env::var("PIG_DATA_DIR").expect("selftest 数据目录"));
    let jsonl = data_dir.join("sessions").join(format!("{session_f}.jsonl"));
    assert!(jsonl.exists(), "删除前 rollout 应存在: {}", jsonl.display());
    app!(|app: &mut AppView, cx| app.delete_session(&session_f, cx));
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 10_000, "删除会话超时");
        let gone = app!(|app: &mut AppView, _| {
            !app.metas.iter().any(|m| m.id == session_f) && !app.views.contains_key(&session_f)
        });
        if gone && !jsonl.exists() {
            break;
        }
    }
    assert!(
        app!(|app: &mut AppView, _| app.current.clone()) != Some(session_f),
        "删除当前会话后应切走"
    );
    println!("[selftest] 会话删除 OK（视图+列表+rollout 全清理）");

    // ---- 新会话模型选择不被工作区种子冲掉（回归：曾「切模型→选工作区→发送」
    // 被 apply_hero_defaults 用工作区旧模型覆盖）----
    // 前置：显式用 mock 建一个会话并完成回合，成为工作区最新种子
    app!(|app: &mut AppView, _| app.agent.new_session(
        app.cwd.clone(),
        Some("mock".to_string()),
        Some("mock-model".to_string()),
        None,
        None,
    ));
    let known: Vec<String> =
        app!(|app: &mut AppView, _| { app.metas.iter().map(|m| m.id.clone()).collect() });
    let seed_id = loop {
        timer!(200).await;
        let found = app!(|app: &mut AppView, _| {
            let Some(sid) = &app.current else { return None };
            (!known.contains(sid)).then(|| sid.clone())
        });
        if let Some(id) = found {
            break id;
        }
    };
    app!(|app: &mut AppView, _| {
        app.agent.send_message(
            seed_id.clone(),
            "种子会话打个卡".to_string(),
            vec![],
            pig_protocol::ExecMode::AutoEdit,
        );
    });
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 30_000, "模型种子会话超时");
        let done = app!(|app: &mut AppView, cx| {
            let Some(views) = app.views.get(&seed_id) else {
                return false;
            };
            !views.thread.read(cx).is_streaming() && waited > 1000
        });
        if done {
            break;
        }
    }

    // 前置断言：未显式选模型时，hero 默认值来自工作区种子（此时最新 = 刚建的 mock 会话）
    app!(|app: &mut AppView, cx| app.enter_hero(cx));
    timer!(400).await;
    let seeded = app!(|app: &mut AppView, _| app.current_model.clone());
    assert_eq!(
        seeded,
        Some(("mock".to_string(), "mock-model".to_string())),
        "未选模型时 hero 默认值应来自工作区种子: {seeded:?}"
    );

    // 变体 2：hero → 切 anthropic → 再选工作区（触发 apply_hero_defaults）→ 发送
    app!(|app: &mut AppView, cx| app.enter_hero(cx));
    app!(|app: &mut AppView, cx| {
        app.composer.update(cx, |_, cx| {
            cx.emit(ComposerEvent::SetModel {
                provider_id: "anthropic".to_string(),
                model_id: "mock-model".to_string(),
            });
        });
    });
    timer!(400).await;
    let cwd_str = app!(|app: &mut AppView, _| app.cwd.display().to_string());
    app!(|app: &mut AppView, cx| {
        app.composer.update(cx, |_, cx| {
            cx.emit(ComposerEvent::SelectCwd(cwd_str.clone()));
        });
    });
    timer!(400).await;
    let picked = app!(|app: &mut AppView, _| (app.current_model.clone(), app.hero_cwd.is_some()));
    assert_eq!(
        picked,
        (
            Some(("anthropic".to_string(), "mock-model".to_string())),
            true
        ),
        "选工作区后用户已选的模型不应被种子覆盖: {picked:?}"
    );
    app!(|app: &mut AppView, cx| {
        app.hero_send(
            "模型选择回归 v2".to_string(),
            vec![],
            pig_protocol::ExecMode::AutoEdit,
            cx,
        );
    });
    let mut waited = 0u64;
    let v2_id = loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 20_000, "v2 会话建立超时");
        let found = app!(|app: &mut AppView, cx| {
            let Some(sid) = &app.current else { return None };
            if known.contains(sid) {
                return None;
            }
            let streaming = app
                .views
                .get(sid)
                .map(|v| v.thread.read(cx).is_streaming())
                .unwrap_or(true);
            (!streaming).then(|| sid.clone())
        });
        if let Some(id) = found {
            break id;
        }
    };
    let v2 = app!(|app: &mut AppView, _| {
        app.metas
            .iter()
            .find(|m| m.id == v2_id)
            .map(|m| (m.provider_id.clone(), m.model_id.clone()))
    });
    assert_eq!(
        v2,
        Some((
            Some("anthropic".to_string()),
            Some("mock-model".to_string())
        )),
        "切模型→选工作区→发送：新会话应使用用户选择的模型: {v2:?}"
    );
    println!("[selftest] hero 切模型后选工作区，模型选择保留 OK");

    // 变体 1：hero → 切 anthropic → 直接发送（无工作区选择）
    app!(|app: &mut AppView, cx| app.enter_hero(cx));
    app!(|app: &mut AppView, cx| {
        app.composer.update(cx, |_, cx| {
            cx.emit(ComposerEvent::SetModel {
                provider_id: "anthropic".to_string(),
                model_id: "mock-model".to_string(),
            });
        });
    });
    timer!(400).await;
    app!(|app: &mut AppView, cx| {
        app.hero_send(
            "模型选择回归 v1".to_string(),
            vec![],
            pig_protocol::ExecMode::AutoEdit,
            cx,
        );
    });
    let mut waited = 0u64;
    let v1_id = loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 20_000, "v1 会话建立超时");
        let found = app!(|app: &mut AppView, cx| {
            let Some(sid) = &app.current else { return None };
            if sid == &v2_id || known.contains(sid) {
                return None;
            }
            let streaming = app
                .views
                .get(sid)
                .map(|v| v.thread.read(cx).is_streaming())
                .unwrap_or(true);
            (!streaming).then(|| sid.clone())
        });
        if let Some(id) = found {
            break id;
        }
    };
    let v1 = app!(|app: &mut AppView, _| {
        app.metas
            .iter()
            .find(|m| m.id == v1_id)
            .map(|m| (m.provider_id.clone(), m.model_id.clone()))
    });
    assert_eq!(
        v1,
        Some((
            Some("anthropic".to_string()),
            Some("mock-model".to_string())
        )),
        "hero 切模型直接发送：新会话应使用用户选择的模型: {v1:?}"
    );
    println!("[selftest] 新会话模型选择（用户选择优先/种子兜底）OK");

    // 变体 3（用户实际流程）：工作区行点 +（NewTaskInWorkspace，预设 cwd 进
    // hero）→ 切模型 → 发送。
    // 前置：再显式建一个 mock 会话并完成回合——v1/v2 的 anthropic 会话已成为
    // 工作区最新种子，会与用户选择同为 anthropic，断言无法区分「保留选择」
    // 与「还原成种子」，必须把种子刷回 mock
    app!(|app: &mut AppView, _| app.agent.new_session(
        app.cwd.clone(),
        Some("mock".to_string()),
        Some("mock-model".to_string()),
        None,
        None,
    ));
    let known3: Vec<String> =
        app!(|app: &mut AppView, _| { app.metas.iter().map(|m| m.id.clone()).collect() });
    let seed3_id = loop {
        timer!(200).await;
        let found = app!(|app: &mut AppView, _| {
            let Some(sid) = &app.current else { return None };
            (!known3.contains(sid)).then(|| sid.clone())
        });
        if let Some(id) = found {
            break id;
        }
    };
    app!(|app: &mut AppView, _| {
        app.agent.send_message(
            seed3_id.clone(),
            "v3 前置种子会话".to_string(),
            vec![],
            pig_protocol::ExecMode::AutoEdit,
        );
    });
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 30_000, "v3 种子会话超时");
        let done = app!(|app: &mut AppView, cx| {
            let Some(views) = app.views.get(&seed3_id) else {
                return false;
            };
            !views.thread.read(cx).is_streaming() && waited > 1000
        });
        if done {
            break;
        }
    }
    // 确认种子生效：进 hero 后默认模型应是 mock（工作区最新）
    app!(|app: &mut AppView, cx| {
        // SidebarEvent::NewTaskInWorkspace 的 handler 本体（直调需要 window）
        app.hero_cwd = Some(app.cwd.clone());
        app.enter_hero(cx);
    });
    timer!(400).await;
    let seeded3 = app!(|app: &mut AppView, _| app.current_model.clone());
    assert_eq!(
        seeded3,
        Some(("mock".to_string(), "mock-model".to_string())),
        "v3 前置：工作区种子应为 mock: {seeded3:?}"
    );
    // 切 anthropic → 发送
    app!(|app: &mut AppView, cx| {
        app.composer.update(cx, |_, cx| {
            cx.emit(ComposerEvent::SetModel {
                provider_id: "anthropic".to_string(),
                model_id: "mock-model".to_string(),
            });
        });
    });
    timer!(400).await;
    app!(|app: &mut AppView, cx| {
        app.hero_send(
            "模型选择回归 v3".to_string(),
            vec![],
            pig_protocol::ExecMode::AutoEdit,
            cx,
        );
    });
    let mut waited = 0u64;
    let v3_id = loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 20_000, "v3 会话建立超时");
        let found = app!(|app: &mut AppView, cx| {
            let Some(sid) = &app.current else { return None };
            if sid == &v1_id || sid == &v2_id || known.contains(sid) || sid == &seed3_id {
                return None;
            }
            let streaming = app
                .views
                .get(sid)
                .map(|v| v.thread.read(cx).is_streaming())
                .unwrap_or(true);
            (!streaming).then(|| sid.clone())
        });
        if let Some(id) = found {
            break id;
        }
    };
    let v3 = app!(|app: &mut AppView, _| {
        app.metas
            .iter()
            .find(|m| m.id == v3_id)
            .map(|m| (m.provider_id.clone(), m.model_id.clone()))
    });
    assert_eq!(
        v3,
        Some((
            Some("anthropic".to_string()),
            Some("mock-model".to_string())
        )),
        "工作区点+→切模型→发送：新会话应使用用户选择的模型: {v3:?}"
    );
    println!("[selftest] 工作区点+新建后切模型，模型选择保留 OK");

    // 切模型时思考等级落点（优先级）：模型默认档 > 继承（需新模型支持）
    // > 继承档失效时启发式。selftest 配置：mock 有默认 low，anthropic 无默认
    // 1) 默认档优先：当前 high（mock 也支持 high）→ 切 mock 仍落到默认 low
    app!(|app: &mut AppView, cx| {
        app.composer.update(cx, |_, cx| {
            cx.emit(ComposerEvent::SetReasoning(Some("high".to_string())));
        });
    });
    timer!(200).await;
    let had_level = app!(|app: &mut AppView, _| app.reasoning_level.clone());
    assert_eq!(had_level, Some("high".to_string()), "前置：等级应为 high");
    app!(|app: &mut AppView, cx| {
        app.composer.update(cx, |_, cx| {
            cx.emit(ComposerEvent::SetModel {
                provider_id: "mock".to_string(),
                model_id: "mock-model".to_string(),
            });
        });
    });
    timer!(200).await;
    let (level, meta_level) = app!(|app: &mut AppView, _| {
        let meta_level = app
            .metas
            .iter()
            .find(|m| m.id == v3_id)
            .and_then(|m| m.reasoning_level.clone());
        (app.reasoning_level.clone(), meta_level)
    });
    assert_eq!(
        level,
        Some("low".to_string()),
        "模型默认档应优先于可继承的等级: level={level:?}"
    );
    assert_eq!(
        meta_level,
        Some("low".to_string()),
        "落点等级应写穿 meta: meta_level={meta_level:?}"
    );

    // 2) 未配默认 → 继承：anthropic 无默认，max 在其等级表内 → 切过去保持 max
    app!(|app: &mut AppView, cx| {
        app.composer.update(cx, |_, cx| {
            cx.emit(ComposerEvent::SetReasoning(Some("max".to_string())));
        });
    });
    app!(|app: &mut AppView, cx| {
        app.composer.update(cx, |_, cx| {
            cx.emit(ComposerEvent::SetModel {
                provider_id: "anthropic".to_string(),
                model_id: "mock-model".to_string(),
            });
        });
    });
    timer!(200).await;
    let level = app!(|app: &mut AppView, _| app.reasoning_level.clone());
    assert_eq!(
        level,
        Some("max".to_string()),
        "未配默认且等级被支持时应继承: level={level:?}"
    );
    println!("[selftest] 切模型思考等级落点（默认档优先/继承）OK");

    // 三栏最小宽度钳制（纯函数）：侧栏 ≥200、右面板 ≥280、为中心区保留 ≥480
    assert_eq!(
        clamp_dock_widths(1280., 220., 300., true, true),
        (220., 300.),
        "区间内不动"
    );
    assert_eq!(
        clamp_dock_widths(1280., 100., 50., true, true),
        (200., 280.),
        "低于各自最小值拉回"
    );
    assert_eq!(
        clamp_dock_widths(1280., 900., 300., true, true),
        (500., 300.),
        "左栏封顶为中心区留 480，右栏不受牵连"
    );
    assert_eq!(
        clamp_dock_widths(1280., 900., 300., true, false),
        (800., 300.),
        "收起的栏不占预算"
    );
    assert_eq!(
        clamp_dock_widths(960., 400., 400., true, true),
        (200., 280.),
        "窗口最小宽时两侧同时越界：左先让位，一遍收敛到全最小"
    );
    assert_eq!(
        clamp_dock_widths(0., 220., 300., true, true),
        (220., 300.),
        "首帧未测量不动作"
    );
    println!("[selftest] 三栏最小宽度钳制 OK");

    println!("SELFTEST PASS");
    std::process::exit(0);
}
