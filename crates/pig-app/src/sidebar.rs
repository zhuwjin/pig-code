use gpui_kit::assets::IconName as AssetsIconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Side, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::RelativeTime as _;
use crate::anim::{EXPAND_ANIM_DUR, ExpandAnim};

/// Sidebar session row data (assembled by AppView from SessionMeta plus runtime
/// state, then passed in)
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

/// Display value of a session title: empty-string sentinel (new session's automatic
/// naming not yet done) → localized "new task" placeholder.
/// Rendering-only; search matching, rename initial values, etc. still use the raw
/// title (the empty-string semantics are preserved).
pub fn display_title(title: &str) -> String {
    if title.is_empty() {
        rust_i18n::t!("sidebar.untitled").to_string()
    } else {
        title.to_string()
    }
}

/// Sidebar list view: flat list (across workspaces, rows annotated with their
/// workspace) / grouped by workspace
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SidebarView {
    Flat,
    Workspace,
}

#[derive(Clone)]
pub enum SidebarEvent {
    Select(String),
    NewTask,
    /// Create a new task under the given workspace directory (hero preset cwd)
    NewTaskInWorkspace(String),
    SetPinned(String, bool),
    SetArchived(String, bool),
    /// Manual session rename (automatic naming no longer overrides afterwards)
    RenameSession(String, String),
    /// Delete the session (clears the database plus the rollout file,
    /// unrecoverable)
    DeleteSession(String),
    RemoveWorkspace(String),
    /// Rename the workspace display name; None restores the default directory name
    RenameWorkspace(String, Option<String>),
    OpenSettings,
}

/// Target of inline rename: workspace or session (sharing one input entity)
#[derive(Clone, PartialEq)]
enum RenameTarget {
    Workspace(String),
    Session(String),
}

impl EventEmitter<SidebarEvent> for Sidebar {}

/// In-progress state of the hover title marquee
struct TitleMarquee {
    session_id: String,
    position: f32,
    forward: bool,
    hold_ticks: u8,
}

/// How long the marquee holds at either end (16ms × 45 ≈ 0.7s)
const MARQUEE_HOLD_TICKS: u8 = 45;
/// Stay still briefly after hover before starting to scroll
const MARQUEE_START_TICKS: u8 = 30;
/// Sessions shown per page under an expanded workspace (one page by default, "show
/// more" adds one page each time)
const WORKSPACE_PAGE_SIZE: usize = 5;

pub struct Sidebar {
    view: SidebarView,
    sessions: Vec<SidebarSession>,
    /// Workspace list = visible manual workspaces ∪ session cwds (already sorted by
    /// AppView, hidden workspaces excluded)
    workspaces: Vec<String>,
    /// Workspace path → user-defined display name
    aliases: std::collections::HashMap<String, String>,
    /// Rename target in progress (workspace/session share one input)
    renaming: Option<RenameTarget>,
    rename_input: Entity<InputState>,
    active: Option<String>,
    search_open: bool,
    search_input: Entity<InputState>,
    expanded: std::collections::HashSet<String>,
    /// Expand/collapse animation state of a workspace's session list
    /// (crate::anim::ExpandAnim), key = workspace path
    expand_anims: std::collections::HashMap<String, ExpandAnim>,
    /// Expand/collapse animation state of the paginated "extra pages" sub-block
    /// (show more = slide open, collapse = slide shut then remove rows),
    /// key = workspace path
    paginate_anims: std::collections::HashMap<String, ExpandAnim>,
    /// Path of the hovered workspace row: the row-tail overlay (name fade + buttons)
    /// only reserves space while hovered; when not hovered the name uses the full
    /// row width, unclipped
    hovered_workspace: Option<String>,
    /// Hovered session row id: the fade base color and row-tail button visibility
    /// follow row hover
    hovered_session: Option<String>,
    /// Workspace session pagination: path → rows currently shown (default =
    /// WORKSPACE_PAGE_SIZE)
    workspace_shown: std::collections::HashMap<String, usize>,
    /// Horizontal scroll handle for session titles (used by the hover marquee),
    /// key = session id; render_session_row only holds &self, hence the RefCell
    title_scrolls: std::cell::RefCell<std::collections::HashMap<String, ScrollHandle>>,
    marquee: Option<TitleMarquee>,
    /// Anchored width of the dock sidebar content (pushed by AppView on every
    /// render): during the open/close animation the dock's actual width is tweened,
    /// while the content stays at this width anchored to the divider side, sliding
    /// out/in instead of being squeezed and re-laid out
    panel_width: f32,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

mod marquee;
mod menus;
mod rename;
#[cfg(test)]
mod tests;
mod views;

impl Sidebar {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(rust_i18n::t!("sidebar.search_placeholder"))
        });
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
            view: SidebarView::Workspace,
            sessions: vec![],
            workspaces: vec![],
            aliases: std::collections::HashMap::new(),
            renaming: None,
            rename_input,
            active: None,
            search_open: false,
            search_input,
            expanded: std::collections::HashSet::new(),
            expand_anims: std::collections::HashMap::new(),
            paginate_anims: std::collections::HashMap::new(),
            hovered_workspace: None,
            hovered_session: None,
            workspace_shown: std::collections::HashMap::new(),
            title_scrolls: std::cell::RefCell::new(std::collections::HashMap::new()),
            marquee: None,
            // Matches AppView's initial sidebar width; AppView pushes on every
            // render, this is just the fallback before the first frame
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
            // Two-line detail row keys carry a "pinned-" prefix (no clash with the
            // same id's single-line key)
            let sid = id.strip_prefix("pinned-").unwrap_or(id.as_str());
            self.sessions.iter().any(|s| s.id == sid)
        });
        self.workspace_shown
            .retain(|path, _| self.workspaces.iter().any(|w| w == path));
        self.expand_anims
            .retain(|path, _| self.workspaces.iter().any(|w| w == path));
        self.paginate_anims
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

    /// AppView pushes the sidebar's expanded target width (may change after drag or
    /// clamping): notify only when the value changes, so open/close animation frames
    /// do not needlessly disturb sidebar re-renders
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

    /// Workspace display name: prefer the user alias, else the directory name.
    fn workspace_name(&self, path: &str) -> String {
        self.aliases.get(path).cloned().unwrap_or_else(|| {
            std::path::Path::new(path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path.to_string())
        })
    }

    /// For self-test: the workspace list.
    pub fn debug_workspaces(&self) -> &[String] {
        &self.workspaces
    }

    /// For self-test: session ids under a workspace.
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
            SidebarView::Flat => self.render_flat_view(window, cx),
            SidebarView::Workspace => self.render_workspace_view(window, cx),
        };

        // Open/close animation anchoring layer: dock_frame has overflow_hidden, and
        // during the open/close tween the dock's actual width is smaller than the
        // content width; the content stays at panel_width anchored right against the
        // divider, sliding left as a whole and getting clipped when collapsing
        // rather than being squeezed and re-laid out. In steady state actual width
        // == panel_width, and the absolutely positioned sub-layer fills it exactly
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
                        // The divider is drawn by the dock handle's own line: a
                        // sidebar-drawn border_r would land to the right of the
                        // handle's hit area (gpui-base's Side::Left handle hit area
                        // stops on the left side of the boundary line), making the
                        // line undraggable
                        .child(self.render_action_rows(cx))
                        .child(self.render_list_header(cx))
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
                                        .child(
                                            div()
                                                .text_sm()
                                                .child(rust_i18n::t!("sidebar.settings")),
                                        ),
                                ),
                        ),
                ),
        )
    }
}

// Dock panel capability: the sidebar draws all of its own chrome; no dock title bar/padding
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
