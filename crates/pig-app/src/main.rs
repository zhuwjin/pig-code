// Hide the console window in release builds (kept in debug for reading logs).
// With no console under the GUI subsystem, stdout/stderr writes are silently
// ignored by std (verified not to panic); the real log goes to
// {data_dir}/logs (logging.rs, tracing)
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod agent_client;
mod anim;
mod assets;
mod clipboard;
mod code_view;
mod composer;
mod errors;
mod file_panel;
mod font;
mod i18n;
mod logging;
mod review_panel;
mod search_popup;
mod settings;
mod sidebar;
mod subagent_panel;
mod task_output_panel;
mod terminal;
mod thread_view;

// i18n registry for GUI strings (locales/; t! fetches strings, fallback = en
// — English is the fallback primary language). Model-facing prompts do not
// go through here (all English inside pig-core); core is language-neutral,
// and the localized strings for its structured errors/status (CoreError etc.)
// also live in this registry (errors.yml), mapped at render points via
// errors.rs.
rust_i18n::i18n!("locales", fallback = "en");

use std::cell::Cell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;

use gpui_kit::InteractiveElement as _;
use gpui_kit::assets::IconName as AssetsIconName;
use gpui_kit::base::GlobalState;
use gpui_kit::base::{Align, Dialog, ElementExt as _, Placement, Positioner};
use gpui_kit::component::button::{Button, ButtonVariants as _, DropdownButton};
use gpui_kit::component::dock::{DockPlacement, panel_handle};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::menu::PopupMenuItem;
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Root, Sizable as _, StyledExt as _, Theme, ThemeMode,
    TitleBar, h_flex, text::TextViewState, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use pig_protocol::{Event, ExecMode, SessionMeta};

gpui_kit::actions!(
    pig_app,
    [
        NewTask,
        FocusSearch,
        SearchPrev,
        SearchNext,
        FocusThreadSearch,
        CloseThreadSearch,
        CloseSettings,
        ToggleSidebar,
        ToggleChanges,
        ToggleBrowser,
        ToggleSideChat,
        ToggleTerminal,
        ComposerNavUp,
        ComposerNavDown,
        ComposerNavNext,
        ComposerNavPrev,
        ComposerPopupClose
    ]
);

/// Whether the theme follows system appearance: follows by default; after a
/// manual light/dark switch it stays pinned to the chosen mode for the rest
/// of this run.
pub struct ThemeFollowSystem(pub bool);

impl Global for ThemeFollowSystem {}

/// Relative time display (just now / N minutes / N hours / N days / N weeks /
/// N months; i18n via t! interpolation)
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
            0..=59 => rust_i18n::t!("time.just_now").to_string(),
            60..=3599 => rust_i18n::t!("time.minutes", n = diff / 60).to_string(),
            3600..=86399 => rust_i18n::t!("time.hours", n = diff / 3600).to_string(),
            _ => {
                let days = diff / 86400;
                if days < 7 {
                    rust_i18n::t!("time.days", n = days).to_string()
                } else if days < 30 {
                    rust_i18n::t!("time.weeks", n = days / 7).to_string()
                } else {
                    rust_i18n::t!("time.months", n = days / 30).to_string()
                }
            }
        }
    }
}

use crate::agent_client::AgentClient;
use crate::composer::{Composer, ComposerEvent, PendingApproval, PendingQuestion};
use crate::file_panel::FileViewPanel;
use crate::review_panel::{ReviewEvent, ReviewPanel};
use crate::settings::{SettingsEvent, SettingsView};
use crate::sidebar::{Sidebar, SidebarEvent, SidebarSession};
use crate::subagent_panel::SubagentPanel;
use crate::task_output_panel::TaskOutputPanel;
use crate::terminal::{TerminalPanel, TerminalPanelEvent};
use crate::thread_view::{ThreadEvent, ThreadView};
use crate::trajectory::TrajectoryState;

struct SessionViews {
    thread: Entity<ThreadView>,
    review: Entity<ReviewPanel>,
}

/// Right panel tabs: "Changes" and "Trajectory" are built-in pages;
/// "Subagent" is one per agent_id (opened by clicking a notification card);
/// "File" is one per absolute path (opened by clicking the path on a Read
/// card); "CompactSummary" shows the compact divider's summary (one shared
/// tab, its content swapped per click).
/// Browser/terminal/side chat to come later.
#[derive(Clone, PartialEq, Eq)]
enum RightTab {
    Changes,
    Trajectory,
    Subagent {
        agent_id: String,
    },
    /// File viewer; path = the normalized absolute path (the key in file_tabs)
    File {
        path: String,
    },
    /// Background Bash task output; id = the task id (the key in task_tabs)
    TaskOutput {
        id: String,
    },
    CompactSummary,
}

impl RightTab {
    /// Stable unique key for element ids
    fn key(&self) -> String {
        match self {
            Self::Changes => "changes".to_string(),
            Self::Trajectory => "trajectory".to_string(),
            Self::Subagent { agent_id } => format!("subagent-{agent_id}"),
            Self::File { path } => format!("file-{path}"),
            Self::TaskOutput { id } => format!("task-{id}"),
            Self::CompactSummary => "compact-summary".to_string(),
        }
    }
}

/// Truncates "Subagent" tab titles (12 chars + ellipsis; the tab bar has
/// limited width)
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

/// The dock skin wraps panel views with `cached()`: the cache invalidates
/// only when the panel itself notifies. Notifications from child entities
/// (thread/review/composer) travel up the dispatch tree, dirtying the panel
/// ancestor and invalidating it automatically; but pure AppView state changes
/// (hero↔session switch, right tab open/close) happen on the ancestor and do
/// not propagate down — so observe AppView and turn its notifies into the
/// panel's own.
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

        // All chrome off: we draw the tab bar/title bar ourselves
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

/// Default height of the bottom terminal panel (px); users can adjust it by
/// dragging the dock handle (upstream clamps to [PANEL_MIN_SIZE, area
/// height-100]), and the open/close tween interpolates between 0↔current
/// height
pub(crate) const TERMINAL_PANEL_DEFAULT_H: f32 = 300.;

struct AppView {
    sidebar: Entity<Sidebar>,
    composer: Entity<Composer>,
    views: HashMap<String, SessionViews>,
    current: Option<String>,
    metas: Vec<SessionMeta>,
    running: HashSet<String>,
    /// Deleted session ids: used to filter late events (ids embed a timestamp
    /// and are never reused, no cleanup needed)
    deleted_sessions: HashSet<String>,
    approval_pending: HashSet<String>,
    /// Per-session pending approval queues (the approval bar shows the head;
    /// concurrent approvals are answered and dequeued one by one, so a later
    /// request never displaces an earlier one — even multiple subagents
    /// awaiting approval in the same session are not lost)
    pending_approvals: HashMap<String, VecDeque<PendingApproval>>,
    /// Structured question awaiting an answer (question bar content): cleared
    /// on submit/skip/turn end
    pending_questions: HashMap<String, PendingQuestion>,
    /// Per-session TodoList/background task snapshots (core-pushed cache,
    /// synced to the composer on session switch)
    todos_by_session: HashMap<String, Vec<pig_protocol::TodoItem>>,
    tasks_by_session: HashMap<String, Vec<pig_protocol::TaskSummary>>,
    agent: AgentClient,
    cwd: PathBuf,
    config_path: Option<PathBuf>,
    exec_mode: pig_protocol::ExecMode,
    /// Plan mode toggle (orthogonal to exec_mode; synced by
    /// SessionConfigured/PlanModeChanged)
    plan_enabled: bool,
    git_branch: Option<String>,
    /// Branch list for the title bar branch switcher (local branches of the
    /// current session's cwd)
    title_branches: Vec<String>,
    /// Whether the title bar branch menu is open
    title_branch_menu_open: bool,
    /// Press position when the branch menu collapsed via an outside click
    /// (that same click is swallowed to prevent collapse-then-reopen)
    title_branch_outside_close: Option<Point<Pixels>>,
    /// Bounds of the branch chip (recorded in on_prepaint, used to anchor
    /// the menu)
    title_branch_btn_bounds: Rc<Cell<Bounds<Pixels>>>,
    /// Title bar session menu (three dots) open/close; the trio follows the
    /// same pattern as the branch menu
    session_menu_open: bool,
    session_menu_outside_close: Option<Point<Pixels>>,
    session_menu_btn_bounds: Rc<Cell<Bounds<Pixels>>>,
    /// Real Finder icon for the "Open in Finder" button (NSWorkspace icon
    /// fetched in the background on macOS; None before it arrives or on other
    /// platforms → falls back to the Lucide folder icon)
    fm_icon: Option<std::sync::Arc<Image>>,
    /// Real terminal app icon for the "open in terminal" menu row (background
    /// extraction on Windows/macOS; None elsewhere or before it arrives →
    /// falls back to the Lucide terminal glyph)
    terminal_icon: Option<std::sync::Arc<Image>>,
    /// Trajectory popup (None = closed): persisted model-io records of the
    /// current session
    trajectory: Option<TrajectoryState>,
    /// Workspace directory chosen on the hero page; None = not chosen (shows
    /// "choose workspace", falling back to the launch directory on send)
    hero_cwd: Option<PathBuf>,
    hero_branch: Option<String>,
    hero_branches: Vec<String>,
    hero_is_git: bool,
    /// Startup/core error shown on the hero page; stored structured so the
    /// localized text is built at draw time
    hero_error: Option<pig_protocol::CoreError>,
    pending_first_send: Option<(
        String,
        Vec<String>,
        Vec<pig_protocol::PendingImage>,
        ExecMode,
    )>,
    /// The user explicitly picked a model in hero mode: apply_hero_defaults
    /// no longer overwrites it with the workspace seed (otherwise "switch
    /// model → choose workspace → send" would reset the pick back to the
    /// workspace's old model)
    hero_model_dirty: bool,
    workspaces: Vec<String>,
    /// Removed (hidden) workspace paths: session cwds can no longer bring
    /// them back into the list
    hidden_workspaces: std::collections::HashSet<String>,
    /// Workspace path → user-defined display name
    workspace_aliases: std::collections::HashMap<String, String>,
    /// Global search popup (Ctrl+K, quick switcher over sessions and
    /// workspaces; see search_popup.rs)
    search_open: bool,
    search_input: Entity<InputState>,
    /// Selected result row (flattened: workspaces then sessions)
    search_selected: usize,
    search_scroll: ScrollHandle,
    settings: Entity<SettingsView>,
    settings_open: bool,
    /// "Enable unregulated mode?" confirmation dialog (pops on every switch
    /// to Yolo; the choice is not remembered)
    yolo_confirm_open: bool,
    /// Confirmation dialog focus (for Esc cancel; steals focus when opened,
    /// with the cancel button focused by default)
    yolo_confirm_focus: FocusHandle,
    sidebar_collapsed: bool,
    /// Whether the right panel is expanded (collapsed by default: entering a
    /// session does not auto-show changes)
    right_open: bool,
    /// Sidebar / right panel expanded target widths (AppView-side copies,
    /// doubling as the content anchor widths for open/close animations). In
    /// the final phase of a collapse tween the dock's actual width is
    /// interpolated to 0, so the expansion target must never read dock_size —
    /// always take the copy; the compensation clamps for width drags and
    /// window resizes also act on the copies
    sidebar_w: f32,
    right_w: f32,
    /// In-progress left/right dock open/close tweens (None = steady state;
    /// both panes can each run one at the same time)
    left_dock_anim: Option<DockSizeAnim>,
    right_dock_anim: Option<DockSizeAnim>,
    /// In-progress edge phase (first/last 100px overlay slide; a single slot
    /// — when replaced across sides, the old side is finalized first)
    dock_edge: Option<DockEdgePhase>,
    /// Edge phase generation counter (pairs with DockEdgePhase::generation)
    dock_edge_generation: u32,
    /// A next-frame callback for dock open/close is already queued (prevents
    /// duplicate registrations at tween start or chain self-continuation from
    /// doubling the callback chain — both panes starting on the same frame
    /// and chain self-continuation re-enter the registration path)
    dock_anim_frames_scheduled: bool,
    /// Open tabs of the right panel (in open order); preserved while
    /// collapsed
    right_tabs: Vec<RightTab>,
    /// Currently active right panel tab (None = show the panel home/menu
    /// page)
    right_active: Option<RightTab>,
    /// Content panels of "Subagent" tabs (agent_id → panel entity; removed
    /// when the tab closes)
    subagent_tabs: HashMap<String, Entity<SubagentPanel>>,
    /// Content panels of "File" tabs (normalized absolute path → panel
    /// entity; removed when the tab closes)
    file_tabs: HashMap<String, Entity<FileViewPanel>>,
    /// Content panels of "Task output" tabs (task id → panel entity; removed
    /// when the tab closes)
    task_tabs: HashMap<String, Entity<TaskOutputPanel>>,
    /// Markdown render state of the "compact summary" tab (rebuilt on each
    /// divider-link click; cleared when the tab closes)
    compact_summary: Option<Entity<TextViewState>>,
    /// Whether the bottom terminal panel is expanded (collapsed by default)
    terminal_open: bool,
    /// Terminal panel entity (lazily created; collapsing only hides it, tabs
    /// and the shell process are kept)
    terminal: Option<Entity<TerminalPanel>>,
    /// User-chosen panel height (synced from the actual height by a
    /// steady-state render after a dock handle drag; defaults to 300,
    /// preserved while collapsed)
    terminal_h: f32,
    /// Bottom dock open/close tween (same step_dock_anim system as the
    /// left/right docks)
    bottom_dock_anim: Option<DockSizeAnim>,
    /// Bottom dock panel entity (created by install_dock and resident; the
    /// bottom dock fully hidden = removed, and remounted only on expansion,
    /// so the panel entity must not live and die with the dock)
    dock_bottom_panel: Option<Entity<DockBottomPanel>>,
    /// Whether the tab bar "+" add-panel menu is open
    right_menu_open: bool,
    /// Press position when the menu collapsed via an outside click: swallow
    /// the button click fired by the same press, avoiding collapse-then-
    /// reopen
    right_menu_outside_close: Option<Point<Pixels>>,
    /// Screen bounds of the tab bar "+" button (recorded in on_prepaint,
    /// used to anchor the menu)
    tab_add_btn_bounds: Rc<Cell<Bounds<Pixels>>>,
    config: Option<pig_protocol::AppConfig>,
    /// (provider_id, model_id)
    current_model: Option<(String, String)>,
    reasoning_level: Option<String>,
    /// Three-pane layout engine (dock): left dock = sidebar, center = session
    /// area, right dock = changes panel; set_locked(true) locks out
    /// drag-rearranging, keeping only width adjustment
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
        // Global search popup input: Change resets the selection (rows are
        // recomputed on every render); Enter (PressEnter) confirms the selected
        // row. set_value on close emits no Change (upstream emit_events=false),
        // so the selection is reset explicitly in close_search_popup
        let search_input = cx
            .new(|cx| InputState::new(window, cx).placeholder(rust_i18n::t!("search.placeholder")));
        let (dock, dock_skin) =
            gpui_kit::component::dock::DockSkin::dock_area("pig-dock", None, window, cx);
        // The dock toggle button in the tab bar is unneeded (title bar
        // buttons exist), and we draw our own tab bar
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
            plan_enabled: false,
            git_branch: None,
            title_branches: vec![],
            title_branch_menu_open: false,
            title_branch_outside_close: None,
            title_branch_btn_bounds: Rc::new(Cell::new(Bounds::default())),
            session_menu_open: false,
            session_menu_outside_close: None,
            session_menu_btn_bounds: Rc::new(Cell::new(Bounds::default())),
            fm_icon: None,
            terminal_icon: None,
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
            search_open: false,
            search_input,
            search_selected: 0,
            search_scroll: ScrollHandle::new(),
            settings,
            settings_open: false,
            yolo_confirm_open: false,
            yolo_confirm_focus: cx.focus_handle(),
            sidebar_collapsed: false,
            right_open: false,
            // Single source of initial widths: install_dock's set_dock_size
            // reads from here
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
            file_tabs: HashMap::new(),
            task_tabs: HashMap::new(),
            compact_summary: None,
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
            cx.subscribe_in(&app.search_input, window, |this, _, event, window, cx| {
                match event {
                    InputEvent::Change => {
                        this.search_selected = 0;
                        cx.notify();
                    }
                    // Single-line input emits PressEnter for Enter/Shift+Enter
                    // alike (consumed at the input layer, never bubbles up)
                    InputEvent::PressEnter { .. } => this.confirm_search(window, cx),
                    _ => {}
                }
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
                    SettingsEvent::RestoreSession(id) => this.agent.set_archived(id, false),
                    SettingsEvent::DeleteSession(id) => this.delete_session(id, cx),
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
        // Title-bar "open in …" icons: the embedded Win11-style colored folder
        // for the file-manager button immediately, replaced by the platform's
        // real app icons once the background fetches land (macOS: NSWorkspace
        // Finder/Terminal icons; Windows: PrivateExtractIconsW on
        // explorer.exe / the Windows Terminal alias)
        app.fm_icon = crate::assets::folder_icon();
        // The two cfg blocks below differ only in the extraction fn and the
        // target field; spelled out for clarity over a generic helper
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
            let task = cx
                .background_executor()
                .spawn(async move { pig_core::files::terminal_icon_png() });
            cx.spawn(async move |this: WeakEntity<AppView>, cx| {
                if let Some(png) = task.await {
                    let _ = this.update(cx, |app, cx| {
                        app.terminal_icon = Some(std::sync::Arc::new(Image::from_bytes(
                            ImageFormat::Png,
                            png,
                        )));
                        cx.notify();
                    });
                }
            })
            .detach();
        }
        #[cfg(target_os = "windows")]
        {
            let task = cx
                .background_executor()
                .spawn(async move { pig_core::files::file_manager_icon_png() });
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
            let task = cx
                .background_executor()
                .spawn(async move { pig_core::files::terminal_icon_png() });
            cx.spawn(async move |this: WeakEntity<AppView>, cx| {
                if let Some(png) = task.await {
                    let _ = this.update(cx, |app, cx| {
                        app.terminal_icon = Some(std::sync::Arc::new(Image::from_bytes(
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

    /// Simulated restart: close the old manager, clear in-memory views,
    /// re-spawn (for selftest).
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

    /// Working directory of the current session (SessionMeta.cwd)
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
        // Thumbnail source for user message image attachments:
        // {data}/sessions/{id}.media (resolved the same way as in core)
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
                    feedback,
                } => {
                    this.agent
                        .approval_reply(request_id.clone(), *decision, feedback.clone());
                    // Remove only the answered entry; if another remains in
                    // the queue keep showing it; only when all are answered is
                    // the approval state cleared and the composer restored
                    // (core wakes every waiter sharing the same merge key)
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
                ThreadEvent::CancelQueued(text) => {
                    if let Some(sid) = this.current.clone() {
                        this.agent.cancel_queued(sid, text.clone());
                    }
                }
                ThreadEvent::OpenSubagent { agent_id, title } => {
                    // The notification card's session = this ThreadView's
                    // session (sid captured at subscribe time)
                    this.open_subagent_tab(sid.clone(), agent_id.clone(), title.clone(), cx);
                }
                ThreadEvent::OpenFile { path, line } => {
                    this.open_file_tab(&sid, path.clone(), *line, cx);
                }
                ThreadEvent::RetryNow => {
                    this.agent.retry_now(sid.clone());
                }
                ThreadEvent::OpenCompactSummary { text } => {
                    this.open_compact_summary_tab(text.clone(), cx);
                }
                ThreadEvent::Fork { turns } => {
                    // Fork source = this ThreadView's session; once core
                    // creates it, the OpenSession cold path returns
                    // SessionConfigured and the existing flow switches to the
                    // new session automatically
                    this.agent.fork_session(&sid, *turns);
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

    /// Model chip display name: look up the provider name by provider_id in
    /// config, degrading to model_id when not found
    /// Fetch the model's reasoning level list from config
    fn model_reasoning_levels(&self, provider_id: &str, model_id: &str) -> Vec<String> {
        self.config
            .as_ref()
            .and_then(|c| c.providers.iter().find(|p| p.id == provider_id))
            .and_then(|p| p.models.iter().find(|m| m.id == model_id))
            .map(|m| m.reasoning_levels.clone())
            .unwrap_or_default()
    }

    /// The model's configured default reasoning level (returned only after
    /// checking it is still in the level list)
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

    /// Landing spot when the current level is unavailable after a model
    /// switch: prefer high (the middle tier of most level lists), otherwise
    /// the first non-off level, otherwise the first level; no levels = off
    /// (None)
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
            prompt: Some(rust_i18n::t!("main.pick_directory_prompt").into()),
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
                        // Workspace changed: re-lay defaults from the new
                        // workspace's most recently active session
                        this.apply_hero_defaults(cx);
                        cx.notify();
                    });
                })
                .ok()?;
            Some(())
        })
        .detach();
    }

    /// Update the current session's entry in the metas cache in place
    /// (consistent with core's write-through; core's Set* write-through no
    /// longer emits SessionList, and a stale cache would make session
    /// switches read old values)
    fn update_current_meta(&mut self, f: impl FnOnce(&mut SessionMeta)) {
        if let Some(sid) = &self.current
            && let Some(meta) = self.metas.iter_mut().find(|m| &m.id == sid)
        {
            f(meta);
        }
    }

    /// Apply an exec mode: local cache + send to core + composer check state
    /// (the direct-selection path is an idempotent re-set; the Yolo
    /// confirmation path relies on it — when intercepted, the composer's
    /// index was never touched)
    fn apply_exec_mode(&mut self, mode: ExecMode, cx: &mut Context<Self>) {
        self.exec_mode = mode;
        self.update_current_meta(|m| m.exec_mode = mode);
        self.composer
            .update(cx, |composer, cx| composer.set_exec_mode(mode, cx));
        if let Some(sid) = &self.current {
            self.agent.set_exec_mode(sid.clone(), mode);
        }
    }

    /// Apply the plan mode toggle (orthogonal to exec_mode): local cache +
    /// send to core + composer chip
    fn apply_plan_mode(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.plan_enabled = enabled;
        self.update_current_meta(|m| m.plan_enabled = enabled);
        self.composer
            .update(cx, |composer, cx| composer.set_plan_enabled(enabled, cx));
        if let Some(sid) = &self.current {
            self.agent.set_plan_mode(sid.clone(), enabled);
        }
    }

    /// For selftest.
    pub fn debug_config(&self) -> Option<&pig_protocol::AppConfig> {
        self.config.as_ref()
    }

    /// For selftest: (title, loaded item count) of the currently active
    /// "Subagent" tab; None when no subagent tab is active or content is not
    /// loaded
    pub fn debug_subagent_tab(&self, cx: &App) -> Option<(String, usize)> {
        let RightTab::Subagent { agent_id } = self.right_active.as_ref()? else {
            return None;
        };
        self.subagent_tabs.get(agent_id)?.read(cx).debug_state()
    }

    /// For selftest: (path, loaded line count) of the currently active
    /// "File" tab; None when inactive/not loaded
    pub fn debug_file_tab(&self, cx: &App) -> Option<(String, usize)> {
        let RightTab::File { path } = self.right_active.as_ref()? else {
            return None;
        };
        self.file_tabs
            .get(path)?
            .read(cx)
            .debug_state()
            .map(|lines| (path.clone(), lines))
    }

    /// For selftest: (running, line count including buffer, cumulative
    /// activity item count) of the "Subagent" tab for the given agent_id;
    /// None when no tab exists
    pub fn debug_subagent_live(&self, agent_id: &str, cx: &App) -> Option<(bool, usize, usize)> {
        self.subagent_tabs
            .get(agent_id)
            .map(|panel| panel.read(cx).debug_live())
    }

    /// For selftest: (following, at_bottom) of the "Subagent" tab for the
    /// given agent_id; None when no tab exists
    pub fn debug_subagent_scroll(&self, agent_id: &str, cx: &App) -> Option<(bool, bool)> {
        self.subagent_tabs
            .get(agent_id)
            .map(|panel| panel.read(cx).debug_scroll())
    }

    /// Return focus to the composer (called when the terminal panel
    /// collapses).
    /// Done via Entity::update to avoid clashing a read borrow with &mut App
    fn refocus_composer(&self, window: &mut Window, cx: &mut Context<Self>) {
        let composer = self.composer.clone();
        composer.update(cx, |composer, cx| composer.focus_input(window, cx));
    }

    /// Bottom terminal panel toggle (title bar button / ctrl-`): expand/
    /// collapse, tabs and the shell process are kept. Lazily created; a new
    /// tab's working directory is the current session's cwd (falling back to
    /// the launch directory with no session); on expand, focus goes into the
    /// terminal (a fresh panel is focused via focus_active inside
    /// TerminalPanel::new, a reopened one is re-focused here), on collapse
    /// focus returns to the composer. The open/close animation is registered
    /// and stepped in render by step_dock_anim(Bottom).
    pub(crate) fn toggle_terminal_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.terminal_open {
            // Sync the dock's actual height before closing: a height set by
            // handle drags is not lost on reopen
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
                    // Tab bar collapse button: collapse the panel and return
                    // focus to the composer
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

    /// Center area content (called by the dock center panel): hero or the
    /// session column or an empty hint, plus page transition animation
    fn render_center(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let hero = self.is_hero(cx) && self.pending_first_send.is_none();
        let current_views = self.current.as_ref().and_then(|id| self.views.get(id));

        let center: AnyElement = if hero {
            self.render_hero(cx)
        } else if let Some(views) = current_views {
            // The bottom terminal panel has moved into the bottom dock
            // (render_bottom_dock_content); it is not mounted in the center
            // v_flex
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
                        .child(rust_i18n::t!("main.empty_hint")),
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
            // Opaque base: edge phase overlay slides and page fade-ins all
            // stack on this layer
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
        // Dock open/close sync: the flags (sidebar_collapsed / right_open)
        // are the sole source of truth (the dock persists no visibility
        // state); after a flip, step_dock_anim tweens the transition (see its
        // docs)
        self.left_dock_anim =
            self.step_dock_anim(DockPlacement::Left, self.left_dock_anim, window, cx);
        self.right_dock_anim =
            self.step_dock_anim(DockPlacement::Right, self.right_dock_anim, window, cx);
        // Bottom terminal dock open/close tween (same system as the left/
        // right docks)
        self.bottom_dock_anim =
            self.step_dock_anim(DockPlacement::Bottom, self.bottom_dock_anim, window, cx);

        // Upstream handle width drags bypass AppView: in the steady state (no
        // tween/edge phase) render first aligns the target-width copies with
        // the dock's actual width, so the drag result is not clobbered by the
        // compensation-clamp write-back below; during a tween/edge phase the
        // actual width is a transition value and is not synced
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
            // Bottom terminal dock actual-height sync (official handle height
            // drags bypass AppView): with no tween the copy aligns to the
            // actual height so the drag result is not clobbered by the next
            // open/close tween using a stale target height
            if self.bottom_dock_anim.is_none()
                && self.dock.read(cx).is_dock_open(DockPlacement::Bottom)
                && let Some(size) = self.dock.read(cx).dock_size(DockPlacement::Bottom)
                && size > px(0.)
            {
                self.terminal_h = f32::from(size).round();
            }
        }

        // Three-pane minimum width compensation clamp: applied to the expanded
        // target-width copies (out-of-range widths from drags/window resizes
        // are pulled back before paint); collapsed panes skip the budget and
        // keep their stored widths as-is; a tweening dock's actual width is
        // owned by the tween and not clamped. No-op when the area width is 0
        // (unmeasured first frame)
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
        // In the steady state (no tween) write the clamped widths back to the
        // dock; drag results were already synced into the copies above, so
        // this only catches passive out-of-range cases like window resizes
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

        // Push the sidebar content width: during open/close animations the
        // content must hold the target width and anchor to the divider side
        // (see Sidebar::render) to slide rather than compress; in steady
        // state the two values are equal, notify fires only on change, and
        // animation frames do not needlessly disturb sidebar re-renders
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
                // Ctrl+K toggles the global search popup (quick switcher over
                // sessions and workspaces)
                if this.search_open {
                    this.close_search_popup(window, cx);
                } else {
                    this.open_search_popup(window, cx);
                }
            }))
            // In-session search: forwarded to the current session's thread
            // view; Close is triggered by Esc in the search bar's
            // thread-search context (the composer's Escape action propagates
            // through to that context)
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
            // Self-healing fallback: ending a selection gesture depends on
            // receiving MouseUpEvent, which some system-level presses
            // (HTCAPTION, border resizing) never deliver. A move with no
            // button pressed means the gesture ended long ago.
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
                    // The edge phase overlay renders last = topmost (the dock is
                    // closed, no handle bar conflict)
                    .when_some(self.render_dock_edge(window, cx), ParentElement::child)
                    .into_any_element()
            }))
            // Tab bar "+" add-panel menu: deferred to the window layer,
            // anchored directly below the "+"
            .when(self.right_menu_open, |this| {
                this.child(self.render_right_menu_dropdown(window, cx))
            })
            // Title bar three-dots session menu (deferred popup)
            .when(self.session_menu_open, |this| {
                this.child(self.render_session_menu(cx))
            })
            // Yolo confirmation dialog: rendered last = topmost (covers all
            // settings/dock/hero content)
            .when(self.yolo_confirm_open, |this| {
                this.child(self.render_yolo_confirm(cx))
            })
            // Global search popup (Ctrl+K): the Dialog component gates itself
            // on the open flag (renders an empty div when closed)
            .child(self.render_search_dialog(cx))
    }
}

fn main() {
    let selftest = std::env::var_os("PIG_SELFTEST").is_some();
    let setup = selftest.then(setup_selftest);
    // After the selftest's data-dir isolation so logs land in the temp dir;
    // before everything else so early diagnostics are captured
    logging::init();

    // PIG_NET_TEST=1: skip opening a window and test each provider's
    // connectivity with the real config (for network troubleshooting)
    // PIG_NET_TEST=full: additionally run the full message-sending flow
    // (including system prompt and tools), printing the event stream
    if let Some(mode) = std::env::var_os("PIG_NET_TEST") {
        if mode == "full" {
            pig_core::net_test_full_turn(None);
        } else {
            pig_core::net_test::net_test_blocking(&pig_core::config::default_path());
        }
        return;
    }

    let cwd = setup
        .as_ref()
        .map(|s| s.cwd.clone())
        .unwrap_or_else(|| std::env::current_dir().expect("cwd"));
    let config_path = setup.map(|s| s.config_path);

    gpui_kit::application()
        // Look up pig's own assets first (provider icons), falling back
        // to gpui-kit official component assets on a miss
        .with_assets(crate::assets::ChainedAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            // Record the platform default font (the restore value for the
            // "system default" option in font settings)
            font::capture_defaults(cx);
            cx.set_global(ThemeFollowSystem(true));

            cx.bind_keys([
                KeyBinding::new("ctrl-n", NewTask, None),
                KeyBinding::new("ctrl-k", FocusSearch, None),
                // In-session search (plain inputs do not consume ctrl-f: the
                // upstream Search action cx.propagate()s through for
                // non-searchable inputs, reaching the app layer)
                KeyBinding::new("ctrl-f", FocusThreadSearch, None),
                KeyBinding::new("ctrl-b", ToggleSidebar, None),
                // Right panel: Changes is available; browser/side chat keys
                // are bound just to show shortcuts in the menu, features to
                // come
                KeyBinding::new("ctrl-shift-g", ToggleChanges, None),
                KeyBinding::new("ctrl-t", ToggleBrowser, None),
                KeyBinding::new("alt-ctrl-b", ToggleSideChat, None),
                // Bottom terminal panel (matching the ctrl-` convention of
                // terminal apps)
                KeyBinding::new("ctrl-`", ToggleTerminal, None),
                // Global search popup ↑/↓ selection: bound on the popup's
                // "search" context (the wrapper around the query input), so no
                // other input is affected; Esc rides the Dialog's own Cancel
                KeyBinding::new("up", SearchPrev, Some("search")),
                KeyBinding::new("down", SearchNext, Some("search")),
                KeyBinding::new("escape", CloseThreadSearch, Some("thread-search")),
                KeyBinding::new("escape", CloseSettings, Some("settings")),
                // Keyboard navigation for the composer's / and @ popups: same
                // context as the composer's own bindings but registered later
                // (at equal depth the later registration wins); when a popup
                // is closed, Composer's handlers cx.propagate() and fall back
                // to the input's native behavior (caret movement/indent/Esc)
                KeyBinding::new("up", ComposerNavUp, Some("Input")),
                KeyBinding::new("down", ComposerNavDown, Some("Input")),
                KeyBinding::new("tab", ComposerNavNext, Some("Input")),
                KeyBinding::new("shift-tab", ComposerNavPrev, Some("Input")),
                KeyBinding::new("escape", ComposerPopupClose, Some("Input")),
            ]);

            // Keep the initial window inside the display's usable area: GPUI
            // sizes are logical pixels, so under scaling 1280x800 can be
            // larger than the physical screen and the bottom would be hidden
            // by the taskbar; leave headroom for the frame and taskbar.
            let mut window_size = size(px(1280.), px(800.));
            if let Some(display) = cx.primary_display() {
                let bounds = display.bounds();
                window_size = size(
                    window_size.width.min(bounds.size.width - px(32.)),
                    window_size.height.min(bounds.size.height - px(96.)),
                );
            }
            // Pin the selftest window to the bottom-right corner: to offset
            // it from a same-sized instance already running on this machine
            // (both would be centered) — a fully covered window is judged
            // occluded by macOS and the draw loop stalls, and without
            // prepaint running the selftest's bounds assertions would all be
            // 0
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
                    // selftest relies on real drawing (bounds are recorded in
                    // prepaint): a window opened in the background may be
                    // occluded or inactive, stalling the render loop —
                    // explicitly bring it to front
                    window.activate_window();
                    // If the window lands on a non-current Space the system
                    // judges it occluded and the draw loop stalls (prepaint
                    // does not run, and the selftest's bounds assertions
                    // would all be 0); the activation brings the app to
                    // front
                    cx.activate(true);
                    // gpui-kit init pins light mode; override by system
                    // appearance when opening the window
                    Theme::sync_system_appearance(Some(window), cx);
                    let view =
                        cx.new(|cx| AppView::new(window, cx, config_path.clone(), cwd.clone()));
                    // Install the dock layout once the AppView entity is in
                    // place (panels hold weak references to AppView)
                    view.update(cx, |app, cx| app.install_dock(window, cx));
                    if selftest {
                        let view = view.clone();
                        // Toggling the terminal panel needs &mut Window
                        // (spawn PTY/focus), so carry the window handle into
                        // the selftest coroutine and return to the window
                        // context via update_window
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
