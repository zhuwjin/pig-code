// release 下隐藏控制台窗口（debug 保留，便于看日志）。GUI 子系统下无控制台时
// stdout/stderr 写入由 std 静默忽略（已实测不 panic），pig-core 的 eprintln 安全
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod agent_client;
mod assets;
mod clipboard;
mod composer;
mod font;
mod review_panel;
mod settings;
mod sidebar;
mod subagent_panel;
mod terminal;
mod thread_view;

use std::cell::Cell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;

use gpui_kit::InteractiveElement as _;
use gpui_kit::assets::IconName as AssetsIconName;
use gpui_kit::base::GlobalState;
use gpui_kit::base::{Align, ElementExt as _, Placement, Positioner};
use gpui_kit::component::button::{Button, ButtonVariants as _, DropdownButton};
use gpui_kit::component::dock::{DockPlacement, panel_handle};
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::menu::PopupMenuItem;
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
        FocusThreadSearch,
        CloseThreadSearch,
        CloseSettings,
        ToggleSidebar,
        ToggleChanges,
        ToggleBrowser,
        ToggleSideChat,
        ToggleTerminal
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
use crate::subagent_panel::SubagentPanel;
use crate::terminal::{TerminalPanel, TerminalPanelEvent};
use crate::thread_view::{ThreadEvent, ThreadView};
use crate::trajectory::TrajectoryState;

struct SessionViews {
    thread: Entity<ThreadView>,
    review: Entity<ReviewPanel>,
}

/// 右侧面板 tab：「改动」「调用轨迹」为内置页；「子代理」每个 agent_id 一个
/// （通知卡点击打开）。浏览器/终端/侧边聊天后续加。
#[derive(Clone, PartialEq, Eq)]
enum RightTab {
    Changes,
    Trajectory,
    Subagent { agent_id: String },
}

impl RightTab {
    /// 元素 id 用的稳定唯一键
    fn key(&self) -> String {
        match self {
            Self::Changes => "changes".to_string(),
            Self::Trajectory => "trajectory".to_string(),
            Self::Subagent { agent_id } => format!("subagent-{agent_id}"),
        }
    }
}

/// 「子代理」tab 标题截断（12 字符 + 省略号，标签页栏宽度有限）
fn truncate_tab_label(title: &str) -> String {
    let mut chars = title.chars();
    let head: String = chars.by_ref().take(12).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

mod dock;
mod events;
mod hero;
mod right_panel;
mod selftest;
mod sessions;
mod title_bar;
mod trajectory;

use dock::*;
use selftest::{run_selftest, setup_selftest};

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
impl_dock_panel!(DockBottomPanel, "bottom-dock");

/// 底部终端面板的默认高度（px）；用户可经 dock 把手拖拽调整（上游钳制
/// [PANEL_MIN_SIZE, 区域高-100]），开合补间在 0↔当前高度间插值
pub(crate) const TERMINAL_PANEL_DEFAULT_H: f32 = 300.;

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
    /// 各会话的待审批队列（审批条显示队首；并发审批逐笔答复逐笔出队，
    /// 后到的请求不再顶掉先到的——同会话多个子代理同时等审批也不会丢）
    pending_approvals: HashMap<String, VecDeque<PendingApproval>>,
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
    /// 标题栏会话菜单（三个点）开合；三件套与分支菜单同模式
    session_menu_open: bool,
    session_menu_outside_close: Option<Point<Pixels>>,
    session_menu_btn_bounds: Rc<Cell<Bounds<Pixels>>>,
    /// 「在访达中打开」按钮的真实访达图标（macOS 后台取 NSWorkspace 图标，
    /// 完成前/其余平台 None → 用 Lucide 文件夹兜底）
    fm_icon: Option<std::sync::Arc<Image>>,
    /// 调用轨迹弹窗（None = 关闭）：当前会话的 model-io 落盘记录
    trajectory: Option<TrajectoryState>,
    /// hero 页选择的工作区目录；None = 未选择（显示"选择工作区"，发送时回落到启动目录）
    hero_cwd: Option<PathBuf>,
    hero_branch: Option<String>,
    hero_branches: Vec<String>,
    hero_is_git: bool,
    hero_error: Option<String>,
    pending_first_send: Option<(
        String,
        Vec<String>,
        Vec<pig_protocol::PendingImage>,
        ExecMode,
    )>,
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
    /// 侧栏 / 右面板展开目标宽（AppView 侧副本，兼作开合动画的内容锚定宽）。
    /// 收起补间末段 dock 实际宽被插值到 0，展开目标不能读 dock_size，一律取
    /// 副本；拖宽与窗口缩放的补钳也都作用在副本上
    sidebar_w: f32,
    right_w: f32,
    /// 进行中的左右 dock 开合补间（None = 稳态；两栏可同时各跑一段）
    left_dock_anim: Option<DockSizeAnim>,
    right_dock_anim: Option<DockSizeAnim>,
    /// 进行中的边缘段（首/尾 100px 覆盖层滑动；单份，跨侧替换时先替旧侧收尾）
    dock_edge: Option<DockEdgePhase>,
    /// 边缘段代次计数（配 DockEdgePhase::generation）
    dock_edge_generation: u32,
    /// dock 开合的下一帧回调已排队（防补间起步/链自续时重复注册导致回调链
    /// 翻倍——左右两栏同帧起步、链自续都会再走注册路径）
    dock_anim_frames_scheduled: bool,
    /// 右侧面板打开的 tab（按打开顺序）；收起时保留
    right_tabs: Vec<RightTab>,
    /// 右侧面板当前激活的 tab（None = 显示面板首页/菜单页）
    right_active: Option<RightTab>,
    /// 「子代理」tab 的内容面板（agent_id → 面板实体；tab 关闭时移除）
    subagent_tabs: HashMap<String, Entity<SubagentPanel>>,
    /// 底部终端面板是否展开（默认收起）
    terminal_open: bool,
    /// 终端面板实体（懒创建；收起仅隐藏，tab 与 shell 进程保留）
    terminal: Option<Entity<TerminalPanel>>,
    /// 用户选定的面板高度（dock 把手拖拽后由稳态 render 同步实高；默认 300，收起时保留）
    terminal_h: f32,
    /// 底部 dock 的开合补间（与左右 dock 同一 step_dock_anim 体系）
    bottom_dock_anim: Option<DockSizeAnim>,
    /// 底部 dock 的面板实体（install_dock 创建并常驻；底部 dock 全隐=移除，
    /// 展开时才挂回，故面板实体不能随 dock 生灭）
    dock_bottom_panel: Option<Entity<DockBottomPanel>>,
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
            session_menu_open: false,
            session_menu_outside_close: None,
            session_menu_btn_bounds: Rc::new(Cell::new(Bounds::default())),
            fm_icon: None,
            trajectory: None,
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
            // 初始宽度单一来源：install_dock 的 set_dock_size 从这里取值
            sidebar_w: 220.,
            right_w: 300.,
            left_dock_anim: None,
            right_dock_anim: None,
            dock_edge: None,
            dock_edge_generation: 0,
            dock_anim_frames_scheduled: false,
            right_tabs: vec![],
            right_active: None,
            subagent_tabs: HashMap::new(),
            terminal_open: false,
            terminal: None,
            terminal_h: TERMINAL_PANEL_DEFAULT_H,
            bottom_dock_anim: None,
            dock_bottom_panel: None,
            right_menu_open: false,
            right_menu_outside_close: None,
            tab_add_btn_bounds: Rc::new(Cell::new(Bounds::default())),
            config: None,
            current_model: None,
            reasoning_level: None,
            dock,
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
                    SettingsEvent::RefreshMcp => this.refresh_mcp(cx),
                    SettingsEvent::RefreshSkills => this.refresh_skills(cx),
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
        // macOS：后台取访达真实图标（NSWorkspace，线程安全），完成前用 Lucide 文件夹兜底
        #[cfg(target_os = "macos")]
        {
            let task = cx
                .background_executor()
                .spawn(async move { pig_core::files::finder_icon_png() });
            cx.spawn(async move |this: WeakEntity<AppView>, cx| {
                if let Some(png) = task.await {
                    let _ = this.update(cx, |app, cx| {
                        app.fm_icon = Some(std::sync::Arc::new(Image::from_bytes(
                            ImageFormat::Png,
                            png,
                        )));
                        cx.notify();
                    });
                }
            })
            .detach();
        }
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
        let thread = cx.new(ThreadView::new);
        // 用户消息图片附件的缩略图源：{data}/sessions/{id}.media（与 core 同源解析）
        thread.update(cx, |thread, _| {
            thread.set_media_dir(pig_core::rollout::media_dir(
                &pig_core::data_dir().join("sessions"),
                session_id,
            ));
        });
        let review = cx.new(ReviewPanel::new);
        let sid = session_id.to_string();
        self._subscriptions.push(
            cx.subscribe(&thread, move |this, _, event, cx| match event {
                ThreadEvent::ApprovalReply {
                    request_id,
                    decision,
                } => {
                    this.agent.approval_reply(request_id.clone(), *decision);
                    // 只摘掉答复的这笔，队列里还有下一笔就接着显示；全答完
                    // 才撤审批态恢复输入框（core 侧会把同合并键的等待者一并唤醒）
                    let mut answered_all = true;
                    if let Some(queue) = this.pending_approvals.get_mut(&sid) {
                        queue.retain(|p| p.request_id != *request_id);
                        answered_all = queue.is_empty();
                    }
                    if answered_all {
                        this.approval_pending.remove(&sid);
                        this.pending_approvals.remove(&sid);
                    }
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
                ThreadEvent::OpenSubagent { agent_id, title } => {
                    // 通知卡所在会话 = 该 ThreadView 的会话（sid 为订阅时捕获）
                    this.open_subagent_tab(sid.clone(), agent_id.clone(), title.clone(), cx);
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
        self.agent.send_message(
            sid,
            text,
            vec![],
            vec![],
            pig_protocol::ExecMode::ConfirmBeforeEdit,
        );
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

    /// 自测用。
    pub fn debug_config(&self) -> Option<&pig_protocol::AppConfig> {
        self.config.as_ref()
    }

    /// 自测用：当前激活的「子代理」tab 的 (标题, 已加载 items 数)；
    /// 无激活子代理 tab 或内容未加载为 None
    pub fn debug_subagent_tab(&self, cx: &App) -> Option<(String, usize)> {
        let RightTab::Subagent { agent_id } = self.right_active.as_ref()? else {
            return None;
        };
        self.subagent_tabs.get(agent_id)?.read(cx).debug_state()
    }

    /// 自测用：指定 agent_id 的「子代理」tab 的 (running, 行数含缓冲, 累计活动项数)；
    /// 无 tab 为 None
    pub fn debug_subagent_live(&self, agent_id: &str, cx: &App) -> Option<(bool, usize, usize)> {
        self.subagent_tabs
            .get(agent_id)
            .map(|panel| panel.read(cx).debug_live())
    }

    /// 自测用：指定 agent_id 的「子代理」tab 的 (following, at_bottom)；无 tab 为 None
    pub fn debug_subagent_scroll(&self, agent_id: &str, cx: &App) -> Option<(bool, bool)> {
        self.subagent_tabs
            .get(agent_id)
            .map(|panel| panel.read(cx).debug_scroll())
    }

    /// 焦点还给输入框（终端面板收起时调用）。
    /// 经 Entity::update 走，避免 read 借用与 &mut App 冲突
    fn refocus_composer(&self, window: &mut Window, cx: &mut Context<Self>) {
        let composer = self.composer.clone();
        composer.update(cx, |composer, cx| composer.focus_input(window, cx));
    }

    /// 底部终端面板开关（标题栏按钮 / ctrl-`）：展开/收起，tab 与 shell 进程保留。
    /// 懒创建；新 tab 的工作目录取当前会话 cwd（无会话回落启动目录）；
    /// 展开时焦点进终端（新建在 TerminalPanel::new 内 focus_active，重开在此补焦），
    /// 收起时还给输入框。开合动画由 step_dock_anim(Bottom) 在 render 里登记/步进。
    pub(crate) fn toggle_terminal_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.terminal_open {
            // 关闭前同步 dock 实高：把手拖过的高度重开时不丢
            if let Some(size) = self.dock.read(cx).dock_size(DockPlacement::Bottom) {
                self.terminal_h = f32::from(size).round();
            }
            self.terminal_open = false;
            self.refocus_composer(window, cx);
        } else {
            self.terminal_open = true;
            let cwd = self.current_cwd().unwrap_or_else(|| self.cwd.clone());
            let shell = self.config.as_ref().and_then(|c| c.terminal_shell.clone());
            match &self.terminal {
                Some(panel) => panel.update(cx, |panel, cx| {
                    panel.set_cwd(cwd.clone(), cx);
                    panel.set_shell(shell, cx);
                    panel.focus_active(window, cx);
                }),
                None => {
                    let panel = cx.new(|cx| TerminalPanel::new(cwd, shell, window, cx));
                    // 标签页栏折叠钮：收起面板，焦点还输入框
                    self._subscriptions.push(cx.subscribe_in(
                        &panel,
                        window,
                        |this, _, _: &TerminalPanelEvent, window, cx| {
                            if let Some(size) = this.dock.read(cx).dock_size(DockPlacement::Bottom)
                            {
                                this.terminal_h = f32::from(size).round();
                            }
                            this.terminal_open = false;
                            this.refocus_composer(window, cx);
                            cx.notify();
                        },
                    ));
                    self.terminal = Some(panel);
                }
            }
        }
        cx.notify();
    }

    /// 中心区内容（dock center 面板调用）：hero / 会话列 / 空提示 + 换页动画
    fn render_center(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let hero = self.is_hero(cx) && self.pending_first_send.is_none();
        let current_views = self.current.as_ref().and_then(|id| self.views.get(id));

        let center: AnyElement = if hero {
            self.render_hero(cx)
        } else if let Some(views) = current_views {
            // 底部终端面板已迁入底部 dock（render_bottom_dock_content），
            // 不在中心区 v_flex 里挂载
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
            // 不透明底：边缘段覆盖层滑动/页面淡入都叠在这层上
            .bg(cx.theme().background)
            .with_animation(
                format!("page-{page_tag}"),
                Animation::new(std::time::Duration::from_millis(150)).with_easing(ease_out_quint()),
                |el, delta| el.opacity(delta),
            )
            .child(center)
            .into_any_element()
    }
}

impl Render for AppView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // dock 开合同步：标志位（sidebar_collapsed / right_open）是唯一事实源
        //（dock 不持久化显隐状态），翻转后经 step_dock_anim 补间过渡（见其文档）
        self.left_dock_anim =
            self.step_dock_anim(DockPlacement::Left, self.left_dock_anim, window, cx);
        self.right_dock_anim =
            self.step_dock_anim(DockPlacement::Right, self.right_dock_anim, window, cx);
        // 底部终端 dock 开合补间（与左右 dock 同一体系）
        self.bottom_dock_anim =
            self.step_dock_anim(DockPlacement::Bottom, self.bottom_dock_anim, window, cx);

        // 上游把手拖宽不经过 AppView：稳态（无补间/边缘段）render 先把目标宽
        // 副本对齐 dock 实宽，拖拽结果才不会被下面的补钳写回冲掉；补间/边缘段
        // 期间实宽是过渡值，不同步
        if self.dock_edge.is_none() {
            if self.left_dock_anim.is_none()
                && let Some(size) = self.dock.read(cx).dock_size(DockPlacement::Left)
                && size > px(0.)
            {
                self.sidebar_w = f32::from(size).round();
            }
            if self.right_dock_anim.is_none()
                && let Some(size) = self.dock.read(cx).dock_size(DockPlacement::Right)
                && size > px(0.)
            {
                self.right_w = f32::from(size).round();
            }
            // 底部终端 dock 实高同步（官方把手拖高不经过 AppView）：无补间时
            // 副本对齐实高，拖拽结果才不会被下次开合补间用旧目标高冲掉
            if self.bottom_dock_anim.is_none()
                && self.dock.read(cx).is_dock_open(DockPlacement::Bottom)
                && let Some(size) = self.dock.read(cx).dock_size(DockPlacement::Bottom)
                && size > px(0.)
            {
                self.terminal_h = f32::from(size).round();
            }
        }

        // 三栏最小宽度补钳：对展开目标宽副本钳（拖拽/window 缩放得越界宽度在
        // paint 前拉回），收起的栏不参与预算、存储宽度原样保留；补间中的 dock
        // 实宽由补间接管，不受钳。区域宽为 0（首帧未测量）时不动作
        let area_w = f32::from(self.dock.read(cx).bounds().size.width);
        let (new_left, new_right) = clamp_dock_widths(
            area_w,
            self.sidebar_w,
            self.right_w,
            !self.sidebar_collapsed,
            self.right_open,
        );
        self.sidebar_w = new_left;
        self.right_w = new_right;
        // 稳态（无补间）时把钳后的宽度写回 dock；拖宽结果已在上面同步进副本，
        // 这里只兜窗口缩放等被动越界
        if self.left_dock_anim.is_none() {
            let left_actual = self
                .dock
                .read(cx)
                .dock_size(DockPlacement::Left)
                .map(f32::from)
                .unwrap_or(0.);
            if left_actual != new_left {
                self.dock.update(cx, |dock, cx| {
                    dock.set_dock_size(DockPlacement::Left, px(new_left), window, cx);
                });
            }
        }
        if self.right_dock_anim.is_none() {
            let right_actual = self
                .dock
                .read(cx)
                .dock_size(DockPlacement::Right)
                .map(f32::from)
                .unwrap_or(0.);
            if right_actual != new_right {
                self.dock.update(cx, |dock, cx| {
                    dock.set_dock_size(DockPlacement::Right, px(new_right), window, cx);
                });
            }
        }

        // 侧栏内容宽推送：开合动画期间内容固定目标宽并锚定分隔线一侧（见
        // Sidebar::render）才能滑出而非压缩；稳态两值相等，值变才 notify，
        // 动画帧不额外扰动侧栏重渲染
        let sidebar_w = self.sidebar_w;
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.set_panel_width(sidebar_w, cx));

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
            // 会话内搜索：转发给当前会话的线程视图；Close 由 Esc 在搜索条的
            // thread-search 上下文触发（输入框的 Escape action 会放行到该上下文）
            .on_action(cx.listener(|this, _: &FocusThreadSearch, window, cx| {
                if let Some(sid) = &this.current
                    && let Some(views) = this.views.get(sid)
                {
                    views.thread.update(cx, |thread, cx| {
                        thread.open_search(window, cx);
                    });
                }
            }))
            .on_action(cx.listener(|this, _: &CloseThreadSearch, window, cx| {
                if let Some(sid) = &this.current
                    && let Some(views) = this.views.get(sid)
                {
                    views.thread.update(cx, |thread, cx| {
                        thread.close_search(window, cx);
                    });
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| {
                this.sidebar_collapsed = !this.sidebar_collapsed;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleChanges, _, cx| {
                this.toggle_right_tab(RightTab::Changes, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleTerminal, window, cx| {
                this.toggle_terminal_panel(window, cx);
            }))
            // 自愈兜底：选择手势的结束依赖收到 MouseUpEvent，而某些系统级按压
            // （HTCAPTION、边框缩放）收不到。未按键的移动说明手势早已结束。
            .on_mouse_move(cx.listener(|_, event: &MouseMoveEvent, window, cx| {
                if event.pressed_button.is_none() {
                    gpui_kit::base::TextSelection::end(window, cx);
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
                    // 边缘段覆盖层最后渲染 = 最顶层（dock 已关，无把手条冲突）
                    .when_some(self.render_dock_edge(window, cx), ParentElement::child)
                    .into_any_element()
            }))
            // 标签页栏 "+" 的加面板菜单：deferred 到窗口层，锚定 "+" 正下方
            .when(self.right_menu_open, |this| {
                this.child(self.render_right_menu_dropdown(window, cx))
            })
            // 标题栏三个点的会话菜单（deferred 弹层）
            .when(self.session_menu_open, |this| {
                this.child(self.render_session_menu(cx))
            })
            // Yolo 确认框：最后渲染 = 最顶层（覆盖 settings/dock/hero 全部内容）
            .when(self.yolo_confirm_open, |this| {
                this.child(self.render_yolo_confirm(cx))
            })
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
        // 先查 pig 自带资产（供应商图标），未命中回落 gpui-kit 官方组件资产
        .with_assets(crate::assets::ChainedAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            // 记录平台默认字体（字体设置「系统默认」档的恢复值）
            font::capture_defaults(cx);
            cx.set_global(ThemeFollowSystem(true));

            cx.bind_keys([
                KeyBinding::new("ctrl-n", NewTask, None),
                KeyBinding::new("ctrl-k", FocusSearch, None),
                // 会话内搜索（普通输入框不消费 ctrl-f：上游 Search action 对
                // 非 searchable 输入 cx.propagate() 放行到应用层）
                KeyBinding::new("ctrl-f", FocusThreadSearch, None),
                KeyBinding::new("ctrl-b", ToggleSidebar, None),
                // 右侧面板：改动可用；浏览器/侧边聊天先绑键让菜单展示快捷键，功能后续加
                KeyBinding::new("ctrl-shift-g", ToggleChanges, None),
                KeyBinding::new("ctrl-t", ToggleBrowser, None),
                KeyBinding::new("alt-ctrl-b", ToggleSideChat, None),
                // 底部终端面板（对标终端类应用的 ctrl-` 惯例）
                KeyBinding::new("ctrl-`", ToggleTerminal, None),
                KeyBinding::new("escape", CloseSearch, Some("search")),
                KeyBinding::new("escape", CloseThreadSearch, Some("thread-search")),
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
            // 自检窗口贴右下角：与本机正在运行的同尺寸实例（同居中）错开——
            // 窗口被完全遮挡时 macOS 判 occluded、绘制循环停摆，
            // prepaint 不跑会让 selftest 的 bounds 断言全 0
            let window_bounds = if selftest {
                match cx.primary_display() {
                    Some(display) => {
                        let screen = display.bounds();
                        WindowBounds::Windowed(gpui_kit::Bounds {
                            origin: gpui_kit::point(
                                screen.origin.x + screen.size.width - window_size.width,
                                screen.origin.y + screen.size.height - window_size.height,
                            ),
                            size: window_size,
                        })
                    }
                    None => WindowBounds::centered(window_size, cx),
                }
            } else {
                WindowBounds::centered(window_size, cx)
            };

            cx.spawn(async move |cx| {
                let options = WindowOptions {
                    window_bounds: Some(window_bounds),
                    window_min_size: Some(size(px(960.), px(600.))),
                    ..TitleBar::window_options()
                };

                cx.open_window(options, |window, cx| {
                    // selftest 依赖真实绘制（bounds 在 prepaint 记录）：后台启动的
                    // 窗口可能被遮挡/未激活导致渲染循环停摆，显式提到前台
                    window.activate_window();
                    // 窗口若落在非当前 Space 会被系统判定遮挡、绘制循环停摆
                    //（prepaint 不跑、selftest 的 bounds 断言全 0）；激活把 app 提到前台
                    cx.activate(true);
                    // gpui-kit init 固定为亮色，开窗时按系统外观覆盖
                    Theme::sync_system_appearance(Some(window), cx);
                    let view =
                        cx.new(|cx| AppView::new(window, cx, config_path.clone(), cwd.clone()));
                    // AppView 实体就位后安装 dock 布局（面板持有 AppView 的 weak 引用）
                    view.update(cx, |app, cx| app.install_dock(window, cx));
                    if selftest {
                        let view = view.clone();
                        // 终端面板开关需要 &mut Window（spawn PTY/焦点），把窗口句柄
                        // 带进自测协程，经 update_window 回到窗口上下文
                        let window_handle = window.window_handle();
                        cx.spawn(async move |cx| {
                            run_selftest(view, window_handle, cx).await;
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
