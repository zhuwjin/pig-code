use gpui_kit::base::{Align, AxisExt as _, ElementExt as _, Placement, Positioner};
use gpui_kit::component::ThemeMode;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::group_box::{GroupBox, GroupBoxVariant, GroupBoxVariants as _};
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::component::select::{SearchableVec, Select, SelectEvent, SelectState};
use gpui_kit::component::setting::{
    RenderOptions, SettingField, SettingFieldElement, SettingGroup, SettingItem, SettingPage,
    Settings,
};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _, h_flex,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use pig_protocol::{
    ApiFormat, AppConfig, McpServerStatus, ModelConfig, ProviderConfig, default_reasoning_params,
};
use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;

/// Default context/output limits for a new model: fallback values when the
/// auto-fill data source lacks fields
const NEW_MODEL_CONTEXT: u64 = 128_000;
const NEW_MODEL_MAX_OUTPUT: u64 = 8_192;

/// First entry of the font dropdown (sentinel): maps the None of config's
/// ui_font/mono_font (follow platform default). Not a valid font family name, so
/// it never collides with local fonts; its label changes with the UI language, and
/// options/backfill/confirm comparisons all take the string from this function
/// (sync_appearance rebuilds options)
fn font_default_label() -> String {
    rust_i18n::t!("settings.appearance.font_default").to_string()
}

/// The three options of the theme mode dropdown (follow system = resume following
/// and immediately align with system appearance). Labels change with the UI
/// language; options/backfill/confirm comparisons all take strings from these
/// three functions
fn theme_follow_label() -> String {
    rust_i18n::t!("common.follow_system").to_string()
}

fn theme_dark_label() -> String {
    rust_i18n::t!("settings.appearance.theme_dark").to_string()
}

fn theme_light_label() -> String {
    rust_i18n::t!("settings.appearance.theme_light").to_string()
}

/// State type of the font dropdown (entries are font family name strings, searchable)
type TextSelectState = SelectState<SearchableVec<String>>;

/// Font setting slot: UI font / monospace font
#[derive(Clone, Copy)]
enum FontSlot {
    Ui,
    Mono,
}

/// Labels of the two API format dropdown options (shared by backfill and option construction)
pub(crate) const FORMAT_OPENAI_LABEL: &str = "OpenAI Chat Completions (/v1/chat/completions)";
pub(crate) const FORMAT_ANTHROPIC_LABEL: &str = "Anthropic Messages (/v1/messages)";

/// Select-style setting field (official SettingFieldElement): the same trigger
/// style as the input fields (text_sm, left-aligned, avoiding Button's 16px
/// centered look); the two font fields are searchable. SelectState is a stateful
/// Entity held by SettingsView to survive across frames (render_field is called
/// every frame and must not build it on the spot); confirm events come back to
/// this view via subscription
struct SearchSelectField {
    select: Entity<TextSelectState>,
    /// Trigger width in horizontal layout; None = follow the container's full width
    width: Option<Pixels>,
    menu_width: Pixels,
}

impl SettingFieldElement for SearchSelectField {
    type Element = Select<SearchableVec<String>>;

    fn render_field(&self, options: &RenderOptions, _: &mut Window, _: &mut App) -> Self::Element {
        Select::new(&self.select)
            .placeholder(rust_i18n::t!("settings.appearance.font_default"))
            .when(options.layout().is_vertical(), |this| this.w_full())
            .when_some(self.width, |this, w| this.w(w))
            .with_size(options.size())
            .menu_width(self.menu_width)
    }
}

#[derive(Clone)]
pub enum SettingsEvent {
    Save(AppConfig),
    TestProvider(String),
    /// Model ID input confirmed (Enter/blur): look up models.dev metadata
    LookupModel(String),
    /// MCP page refresh: AppView re-reads mcp.json and queries core for the
    /// connection list
    RefreshMcp,
    /// Skills page refresh: AppView re-reads the skill directories
    RefreshSkills,
    /// Archived page: restore a session back to the sidebar list
    RestoreSession(String),
    /// Archived page: delete a session (database + rollout, unrecoverable)
    DeleteSession(String),
    Close,
}

impl EventEmitter<SettingsEvent> for SettingsView {}

mod archived;
mod dialog;
mod mcp;
mod presets;
mod providers;
mod skills;

pub(crate) use archived::{ArchivedSessionRow, ArchivedSort};
pub(crate) use dialog::*;
pub(crate) use mcp::{
    McpConfigSnapshot, McpScope, McpSource, McpTransportKind, load_mcp_snapshot,
    workspace_display_name,
};
pub(crate) use skills::{SkillsSnapshot, load_skills_snapshot};

pub struct SettingsView {
    config: AppConfig,
    selected: Option<usize>,
    name_input: Entity<InputState>,
    base_url_input: Entity<InputState>,
    api_key_input: Entity<InputState>,
    api_key_masked: bool,
    delete_armed: bool,
    /// Detail form's API format dropdown (same trigger as the input fields; the
    /// old outline button's 16px centering clashed with the form)
    format_select: Entity<TextSelectState>,
    /// Per-provider connection-test outcome, stored structured (the settings
    /// page localizes the ok/failure text at render time)
    test_results: std::collections::HashMap<String, pig_protocol::ConnTestResult>,
    model_dialog: Option<ModelDialog>,
    /// Open/close state of the preset picker dialog for adding a provider
    preset_picker_open: bool,
    /// MCP page config snapshot (fed by AppView via the RefreshMcp event;
    /// None = not loaded yet)
    mcp_snapshot: Option<McpConfigSnapshot>,
    /// Session the connection status belongs to (None = no open session: show
    /// config only, no status)
    mcp_session: Option<String>,
    /// Status query progress: None = waiting for the reply; Some(None) = the
    /// session has not started the lazy connection yet; Some(Some(statuses)) =
    /// per-server statuses (with tool count and failure reason)
    mcp_connection: Option<Option<Vec<McpServerStatus>>>,
    /// MCP page search box (filters by name/command/URL)
    mcp_search: Entity<InputState>,
    /// MCP create/edit dialog (None = closed)
    mcp_dialog: Option<McpDialog>,
    /// mcp.json write failure message (kept until a successful write or the next
    /// refresh)
    mcp_write_error: Option<String>,
    /// MCP page help card (manual edit format + config file paths) open/close;
    /// collapsed by default
    mcp_help_open: bool,
    /// MCP page scope: user-level (default) / a specific workspace (AppView loads
    /// the snapshot accordingly)
    mcp_scope: McpScope,
    /// Workspace of the current session (governs MCP connection status
    /// applicability and the "current session" badge in both pages' scope dropdowns)
    session_cwd: Option<PathBuf>,
    /// Selectable workspace list (path + display name, same source as the sidebar:
    /// visible workspaces ∪ session cwd; shared by the MCP and skills pages'
    /// scope dropdowns)
    scope_workspaces: Vec<(PathBuf, String)>,
    /// Scope dropdown popup open/close state
    mcp_scope_popup: bool,
    /// Scope button bounds (for deferred popup anchoring, updated on every prepaint)
    mcp_scope_btn_bounds: Rc<Cell<Bounds<Pixels>>>,
    /// Press position recorded on popup outside-close (the same press's click is
    /// swallowed by position match)
    mcp_scope_outside_close: Option<Point<Pixels>>,
    /// Skills page snapshot (fed by AppView via the RefreshSkills event;
    /// None = not loaded yet)
    skills_snapshot: Option<SkillsSnapshot>,
    /// Skills page search box (filters by name/description)
    skills_search: Entity<InputState>,
    /// Skill create/edit dialog (None = closed)
    skills_dialog: Option<SkillDialog>,
    /// Skill directory write failure message (kept until a successful write or
    /// the next refresh)
    skills_write_error: Option<String>,
    /// Skills page help dialog (SKILL.md format + skill directory paths)
    /// open/close; collapsed by default
    skills_help_open: bool,
    /// Skills page scope: user-level (default) / a specific workspace
    skills_scope: McpScope,
    /// Skills page scope dropdown popup trio (same as mcp_scope_*)
    skills_scope_popup: bool,
    skills_scope_btn_bounds: Rc<Cell<Bounds<Pixels>>>,
    skills_scope_outside_close: Option<Point<Pixels>>,
    save_generation: u64,
    /// Debounce generation for the terminal shell input (separate from the
    /// provider form so they don't evict each other)
    shell_save_generation: u64,
    form_dirty: bool,
    /// Theme mode may be out of sync with global state (system appearance changed
    /// while the settings page was closed); set when the settings page opens, and
    /// the theme mode dropdown is synced before render
    pub(crate) appearance_dirty: bool,
    /// Appearance page dropdowns (options = the "system default" sentinel plus
    /// locally installed fonts, see font_default_label)
    ui_font_select: Entity<TextSelectState>,
    mono_font_select: Entity<TextSelectState>,
    /// Theme mode dropdown (follow system/dark/light, not searchable; same visual
    /// as the font dropdowns)
    theme_select: Entity<TextSelectState>,
    /// Language dropdown (follow system/Simplified Chinese/English; confirm writes
    /// config + applies immediately + saves to disk)
    language_select: Entity<TextSelectState>,
    /// "Archived sessions" page: archived session list (pushed by AppView)
    archived_sessions: Vec<ArchivedSessionRow>,
    /// Archived page search box (filters by title)
    archived_search: Entity<InputState>,
    /// Archived page workspace filter dropdown ("all workspaces" sentinel plus
    /// scope_workspaces display names)
    archived_workspace: Entity<TextSelectState>,
    /// Archived page sort (archived time/created time/alphabetical)
    archived_sort: ArchivedSort,
    /// Set when scope_workspaces changes; the archived page filter dropdown
    /// options are rebuilt before render
    archived_ws_dirty: bool,
    _subscriptions: Vec<Subscription>,
}

impl SettingsView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name_input = cx.new(|cx| InputState::new(window, cx));
        let base_url_input = cx.new(|cx| InputState::new(window, cx));
        let api_key_input = cx.new(|cx| InputState::new(window, cx).masked(true));
        let mut _subscriptions = vec![];
        for input in [&name_input, &base_url_input, &api_key_input] {
            _subscriptions.push(cx.subscribe_in(
                input,
                window,
                |this: &mut Self, _, event: &gpui_kit::component::input::InputEvent, _, cx| {
                    if matches!(event, gpui_kit::component::input::InputEvent::Change) {
                        this.schedule_save(cx);
                    }
                },
            ));
        }
        // MCP search box: any content change refilters the list
        let mcp_search = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(rust_i18n::t!("settings.mcp.search_placeholder"))
        });
        _subscriptions.push(cx.subscribe_in(
            &mcp_search,
            window,
            |_: &mut Self, _, _: &gpui_kit::component::input::InputEvent, _, cx| {
                cx.notify();
            },
        ));
        // Skills search box: any content change refilters the list
        let skills_search = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(rust_i18n::t!("settings.skills.search_placeholder"))
        });
        _subscriptions.push(cx.subscribe_in(
            &skills_search,
            window,
            |_: &mut Self, _, _: &gpui_kit::component::input::InputEvent, _, cx| {
                cx.notify();
            },
        ));
        // Archived page search box: any content change refilters the list
        let archived_search = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(rust_i18n::t!("settings.archived.search_placeholder"))
        });
        _subscriptions.push(cx.subscribe_in(
            &archived_search,
            window,
            |_: &mut Self, _, _: &gpui_kit::component::input::InputEvent, _, cx| {
                cx.notify();
            },
        ));
        // Archived page workspace filter dropdown: options rebuild with
        // scope_workspaces (synced via archived_ws_dirty); confirm refilters the list
        let archived_workspace = cx.new(|cx| {
            let mut state = SelectState::new(
                SearchableVec::new(vec![archived::all_workspaces_label()]),
                None,
                window,
                cx,
            );
            state.set_selected_value(&archived::all_workspaces_label(), window, cx);
            state
        });
        _subscriptions.push(cx.subscribe_in(
            &archived_workspace,
            window,
            |_: &mut Self, _, event: &SelectEvent<SearchableVec<String>>, _, cx| {
                if matches!(event, SelectEvent::Confirm(_)) {
                    cx.notify();
                }
            },
        ));
        // Appearance page font dropdowns: first entry "system default" plus locally
        // installed fonts (enumeration cached in-process, only ~100ms the first
        // time); confirm events go through set_font (write config + apply
        // immediately + save) since the official field has no setter channel
        let ui_font_select = Self::new_font_select(window, cx);
        let mono_font_select = Self::new_font_select(window, cx);
        for (slot, select) in [
            (FontSlot::Ui, &ui_font_select),
            (FontSlot::Mono, &mono_font_select),
        ] {
            _subscriptions.push(cx.subscribe_in(
                select,
                window,
                move |this: &mut Self, _, event: &SelectEvent<SearchableVec<String>>, _, cx| {
                    if let SelectEvent::Confirm(Some(family)) = event {
                        this.set_font(slot, family.clone(), cx);
                    }
                },
            ));
        }
        // Theme mode dropdown: three non-searchable options (same Select visual as
        // the font dropdowns); confirm switches immediately and is not persisted
        // (session-level). Backfill relies on appearance_dirty (external state is
        // mutable)
        let theme_select = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(vec![
                    theme_follow_label(),
                    theme_dark_label(),
                    theme_light_label(),
                ]),
                None,
                window,
                cx,
            )
        });
        _subscriptions.push(cx.subscribe_in(
            &theme_select,
            window,
            |_: &mut Self, _, event: &SelectEvent<SearchableVec<String>>, _, cx| {
                if let SelectEvent::Confirm(Some(mode)) = event {
                    Self::apply_theme_mode(mode, cx);
                }
            },
        ));
        // Language dropdown: follow system/Simplified Chinese/English; confirm
        // writes config + applies immediately + saves to disk (the ConfigSnapshot
        // flowing back applies it once more, idempotently). Options and backfill
        // refresh with the UI language (sync_appearance)
        let language_select = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(crate::i18n::language_options()),
                None,
                window,
                cx,
            )
        });
        _subscriptions.push(cx.subscribe_in(
            &language_select,
            window,
            |this: &mut Self, _, event: &SelectEvent<SearchableVec<String>>, _, cx| {
                if let SelectEvent::Confirm(Some(label)) = event {
                    this.set_language(label, cx);
                }
            },
        ));
        // API format dropdown (two non-searchable options): confirm writes back to
        // the currently selected provider and saves
        let format_select = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(vec![
                    FORMAT_OPENAI_LABEL.to_string(),
                    FORMAT_ANTHROPIC_LABEL.to_string(),
                ]),
                None,
                window,
                cx,
            )
        });
        _subscriptions.push(cx.subscribe_in(
            &format_select,
            window,
            |this: &mut Self, _, event: &SelectEvent<SearchableVec<String>>, _, cx| {
                if let SelectEvent::Confirm(Some(label)) = event {
                    this.set_api_format(label, cx);
                }
            },
        ));
        Self {
            config: AppConfig::default(),
            selected: None,
            name_input,
            base_url_input,
            api_key_input,
            api_key_masked: true,
            delete_armed: false,
            format_select,
            test_results: Default::default(),
            model_dialog: None,
            preset_picker_open: false,
            mcp_snapshot: None,
            mcp_session: None,
            mcp_connection: None,
            mcp_search,
            mcp_dialog: None,
            mcp_write_error: None,
            mcp_help_open: false,
            mcp_scope: McpScope::User,
            session_cwd: None,
            scope_workspaces: vec![],
            mcp_scope_popup: false,
            mcp_scope_btn_bounds: Rc::new(Cell::new(Bounds::default())),
            mcp_scope_outside_close: None,
            skills_snapshot: None,
            skills_search,
            skills_dialog: None,
            skills_write_error: None,
            skills_help_open: false,
            skills_scope: McpScope::User,
            skills_scope_popup: false,
            skills_scope_btn_bounds: Rc::new(Cell::new(Bounds::default())),
            skills_scope_outside_close: None,
            save_generation: 0,
            shell_save_generation: 0,
            form_dirty: true,
            appearance_dirty: true,
            ui_font_select,
            mono_font_select,
            theme_select,
            language_select,
            archived_sessions: vec![],
            archived_search,
            archived_workspace,
            archived_sort: ArchivedSort::ArchivedTime,
            archived_ws_dirty: true,
            _subscriptions,
        }
    }

    /// Build a font dropdown state: searchable, no preselection (placeholder is
    /// the "system default" sentinel; backfilled by sync_form after set_config)
    fn new_font_select(window: &mut Window, cx: &mut Context<Self>) -> Entity<TextSelectState> {
        let mut items = vec![font_default_label()];
        items.extend(crate::font::installed_font_names(cx).iter().cloned());
        cx.new(|cx| SelectState::new(SearchableVec::new(items), None, window, cx).searchable(true))
    }

    /// Font dropdown options = the localized "system default" sentinel plus
    /// locally installed fonts (enumeration cached in-process, only ~100ms the
    /// first time). After a language switch the sentinel label changes;
    /// sync_appearance rebuilds options and backfill from the same source
    fn rebuild_font_items(
        select: &Entity<TextSelectState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut items = vec![font_default_label()];
        items.extend(crate::font::installed_font_names(cx).iter().cloned());
        select.update(cx, |state, cx| {
            state.set_items(SearchableVec::new(items), window, cx);
        });
    }

    /// Font dropdown backfill: None is shown as the localized "system default" sentinel
    fn sync_font_selects(&self, window: &mut Window, cx: &mut Context<Self>) {
        let ui_font = self
            .config
            .ui_font
            .clone()
            .unwrap_or_else(font_default_label);
        let mono_font = self
            .config
            .mono_font
            .clone()
            .unwrap_or_else(font_default_label);
        for (select, value) in [
            (&self.ui_font_select, ui_font),
            (&self.mono_font_select, mono_font),
        ] {
            select.update(cx, |state, cx| {
                if state.selected_value() != Some(&value) {
                    state.set_selected_value(&value, window, cx);
                }
            });
        }
    }

    /// Align the theme mode dropdown with global state (system appearance may
    /// have changed it while the settings page was closed); theme/font dropdown
    /// options and the three search boxes' placeholder texts are rebuilt with the
    /// UI language (labels are localized), and so are the language dropdown
    /// options (the "follow system" label itself is localized)
    fn sync_appearance(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mode = Self::current_theme_mode(cx);
        self.theme_select.update(cx, |state, cx| {
            state.set_items(
                SearchableVec::new(vec![
                    theme_follow_label(),
                    theme_dark_label(),
                    theme_light_label(),
                ]),
                window,
                cx,
            );
            if state.selected_value() != Some(&mode) {
                state.set_selected_value(&mode, window, cx);
            }
        });
        // Font dropdowns: the "system default" sentinel changes with the UI
        // language; rebuild options, then backfill from config
        Self::rebuild_font_items(&self.ui_font_select, window, cx);
        Self::rebuild_font_items(&self.mono_font_select, window, cx);
        self.sync_font_selects(window, cx);
        // Language dropdown: options rebuilt with the UI language (the "follow
        // system" label itself is localized), selection backfilled from config
        self.language_select.update(cx, |state, cx| {
            state.set_items(
                SearchableVec::new(crate::i18n::language_options()),
                window,
                cx,
            );
            let label = crate::i18n::language_label(self.config.language.as_deref());
            if state.selected_value() != Some(&label) {
                state.set_selected_value(&label, window, cx);
            }
        });
        // Search box placeholder texts refresh with the UI language (written at
        // InputState construction, must be reset after a language switch)
        self.mcp_search.update(cx, |input, cx| {
            input.set_placeholder(rust_i18n::t!("settings.mcp.search_placeholder"), window, cx);
        });
        self.skills_search.update(cx, |input, cx| {
            input.set_placeholder(
                rust_i18n::t!("settings.skills.search_placeholder"),
                window,
                cx,
            );
        });
        self.archived_search.update(cx, |input, cx| {
            input.set_placeholder(
                rust_i18n::t!("settings.archived.search_placeholder"),
                window,
                cx,
            );
        });
    }

    /// Language dropdown confirm: write config (follow system → None) + apply
    /// immediately + save (the flow-back applies once more, idempotently)
    fn set_language(&mut self, label: &str, cx: &mut Context<Self>) {
        let Some(value) = crate::i18n::language_value_for(label) else {
            return;
        };
        if self.config.language == value {
            return;
        }
        self.config.language = value;
        crate::i18n::apply_config_language(&self.config, cx);
        // After applying, the option language has changed (the "follow system"
        // label); sync the dropdown itself immediately. The archived page workspace
        // filter dropdown's "all workspaces" sentinel also changes with the language
        self.appearance_dirty = true;
        self.archived_ws_dirty = true;
        cx.emit(SettingsEvent::Save(self.config.clone()));
        cx.notify();
    }

    /// Font dropdown confirm: normalize the sentinel entry to None, write back to
    /// config, apply immediately and save (the ConfigSnapshot flowing back after
    /// core persists applies it once more, idempotent with no side effects)
    fn set_font(&mut self, slot: FontSlot, family: String, cx: &mut Context<Self>) {
        let resolved = (family != font_default_label()).then_some(family);
        match slot {
            FontSlot::Ui => self.config.ui_font = resolved,
            FontSlot::Mono => self.config.mono_font = resolved,
        }
        crate::font::apply_config_fonts(&self.config, cx);
        cx.emit(SettingsEvent::Save(self.config.clone()));
        cx.notify();
    }

    /// API format dropdown confirm: write back to the currently selected provider
    /// and save; reasoning parameter suggestions are generated per format, not
    /// linked here (the dialog generates them based on the current format)
    fn set_api_format(&mut self, label: &str, cx: &mut Context<Self>) {
        let Some(ix) = self.selected else { return };
        let format = if label == FORMAT_ANTHROPIC_LABEL {
            ApiFormat::AnthropicMessages
        } else {
            ApiFormat::OpenAiChat
        };
        if self.config.providers[ix].api_format != format {
            self.config.providers[ix].api_format = format;
            cx.emit(SettingsEvent::Save(self.config.clone()));
            cx.notify();
        }
    }

    /// Theme mode dropdown confirm: follow system = resume syncing and
    /// immediately align with the current system appearance (window_appearance
    /// needs no window; the official SettingField setter only provides &mut App);
    /// light/dark = leave following and pin the mode. Theme is not persisted
    /// (session-level setting)
    fn apply_theme_mode(mode: &str, cx: &mut App) {
        if mode == theme_follow_label() {
            cx.set_global(crate::ThemeFollowSystem(true));
            gpui_kit::component::Theme::change(cx.window_appearance(), None, cx);
        } else if mode == theme_dark_label() {
            cx.set_global(crate::ThemeFollowSystem(false));
            gpui_kit::component::Theme::change(ThemeMode::Dark, None, cx);
        } else if mode == theme_light_label() {
            cx.set_global(crate::ThemeFollowSystem(false));
            gpui_kit::component::Theme::change(ThemeMode::Light, None, cx);
        }
    }

    /// Theme mode dropdown current value: follow system > dark > light
    fn current_theme_mode(cx: &App) -> String {
        let follow = cx
            .try_global::<crate::ThemeFollowSystem>()
            .is_some_and(|flag| flag.0);
        if follow {
            theme_follow_label()
        } else if cx.theme().mode.is_dark() {
            theme_dark_label()
        } else {
            theme_light_label()
        }
    }

    pub fn set_config(&mut self, config: AppConfig, cx: &mut Context<Self>) {
        self.config = config;
        if self.selected.is_none() && !self.config.providers.is_empty() {
            self.selected = Some(0);
        }
        self.form_dirty = true;
        cx.notify();
    }

    pub fn set_test_result(
        &mut self,
        provider_id: &str,
        result: pig_protocol::ConnTestResult,
        cx: &mut Context<Self>,
    ) {
        self.test_results.insert(provider_id.to_string(), result);
        cx.notify();
    }

    /// MCP page data feed (called when AppView refreshes): resets the connection
    /// query result from before the page switch/refresh; session_cwd is used to
    /// hide connection status when "viewed workspace ≠ session workspace"
    pub fn set_mcp_config(
        &mut self,
        session_id: Option<String>,
        session_cwd: Option<PathBuf>,
        snapshot: McpConfigSnapshot,
        cx: &mut Context<Self>,
    ) {
        self.mcp_session = session_id;
        self.session_cwd = session_cwd;
        self.mcp_snapshot = Some(snapshot);
        self.mcp_connection = None;
        cx.notify();
    }

    /// core's McpServerList reply (AppView has already filtered by the current session)
    pub fn set_mcp_status(
        &mut self,
        servers: Option<Vec<McpServerStatus>>,
        cx: &mut Context<Self>,
    ) {
        self.mcp_connection = Some(servers);
        cx.notify();
    }

    #[allow(dead_code)]
    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    fn selected_provider(&self) -> Option<&ProviderConfig> {
        self.config.providers.get(self.selected?)
    }

    /// Form backfill needs a window (set_value); marked here and applied at render time
    fn sync_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Font dropdown backfill: None is shown as the "system default" sentinel
        // (sync even with no providers, before the early return)
        self.sync_font_selects(window, cx);
        let Some(provider) = self.selected_provider().cloned() else {
            return;
        };
        let name = provider.name.clone();
        let base_url = provider.base_url.clone();
        let api_key = provider.api_key.clone();
        let format_label = if provider.api_format == ApiFormat::AnthropicMessages {
            FORMAT_ANTHROPIC_LABEL
        } else {
            FORMAT_OPENAI_LABEL
        }
        .to_string();
        self.format_select.update(cx, |state, cx| {
            if state.selected_value() != Some(&format_label) {
                state.set_selected_value(&format_label, window, cx);
            }
        });
        self.name_input.update(cx, |i, cx| {
            if i.value() != name {
                i.set_value(name, window, cx);
            }
        });
        self.base_url_input.update(cx, |i, cx| {
            if i.value() != base_url {
                i.set_value(base_url, window, cx);
            }
        });
        self.api_key_input.update(cx, |i, cx| {
            if i.value() != api_key {
                i.set_value(api_key, window, cx);
            }
        });
    }

    /// Auto-save form changes after a 500ms debounce
    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        self.save_generation += 1;
        let generation = self.save_generation;
        cx.spawn(async move |this: WeakEntity<SettingsView>, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(500))
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.save_generation == generation {
                    this.save_now(cx);
                }
            });
        })
        .detach();
    }

    /// Write form values back to config and emit Save.
    fn save_now(&mut self, cx: &mut Context<Self>) {
        let Some(ix) = self.selected else { return };
        if ix >= self.config.providers.len() {
            return;
        }
        let name = self.name_input.read(cx).value().to_string();
        let base_url = self.base_url_input.read(cx).value().to_string();
        let api_key = self.api_key_input.read(cx).value().to_string();
        let provider = &mut self.config.providers[ix];
        provider.name = name;
        provider.base_url = base_url;
        provider.api_key = api_key;
        cx.emit(SettingsEvent::Save(self.config.clone()));
    }

    /// Terminal shell path confirm: trim then normalize blank to None, write back
    /// to config and save with debounce (takes effect immediately for terminal
    /// tabs created afterwards; already-open tabs are unchanged)
    fn set_shell(&mut self, value: &str, cx: &mut Context<Self>) {
        let value = value.trim();
        let resolved = (!value.is_empty()).then(|| value.to_string());
        if self.config.terminal_shell == resolved {
            return;
        }
        self.config.terminal_shell = resolved;
        self.schedule_shell_save(cx);
    }

    /// Auto-save shell changes after a 500ms debounce (a debounce generation
    /// separate from the provider form)
    fn schedule_shell_save(&mut self, cx: &mut Context<Self>) {
        self.shell_save_generation += 1;
        let generation = self.shell_save_generation;
        cx.spawn(async move |this: WeakEntity<SettingsView>, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(500))
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.shell_save_generation == generation {
                    cx.emit(SettingsEvent::Save(this.config.clone()));
                }
            });
        })
        .detach();
    }

    /// Terminal page: shell path (blank = system default). The official input
    /// field's getter reads config directly (external changes backfill
    /// automatically, no sync_form channel needed); the setter only provides
    /// &mut App, so it returns to this view via WeakEntity to write config plus
    /// save with debounce
    fn terminal_page(weak: &WeakEntity<Self>) -> SettingPage {
        let get = {
            let weak = weak.clone();
            move |cx: &App| {
                weak.upgrade()
                    .map(|this| {
                        this.read(cx)
                            .config
                            .terminal_shell
                            .clone()
                            .unwrap_or_default()
                            .into()
                    })
                    .unwrap_or_default()
            }
        };
        let set = {
            let weak = weak.clone();
            move |value: SharedString, cx: &mut App| {
                let _ = weak.update(cx, |this, cx| this.set_shell(&value, cx));
            }
        };
        SettingPage::new(rust_i18n::t!("settings.terminal.title"))
            .icon(IconName::SquareTerminal)
            .description(rust_i18n::t!("settings.terminal.description"))
            .group(
                SettingGroup::new().title("Shell").item(
                    SettingItem::new(
                        rust_i18n::t!("settings.terminal.shell_path"),
                        SettingField::input(get, set),
                    )
                    .description(rust_i18n::t!("settings.terminal.shell_path_description").as_ref())
                    .keywords([
                        "terminal",
                        "shell",
                        rust_i18n::t!("settings.terminal.title").as_ref(),
                        rust_i18n::t!("settings.terminal.description").as_ref(),
                    ]),
                ),
            )
    }

    /// Generate a provider id that does not collide with existing ones (a
    /// "count+1" scheme collides after deletions: once collided, every id-based
    /// lookup hits the first one, breaking model resolution/labels/session meta)
    fn next_provider_id(&self) -> String {
        let mut n = 1usize;
        loop {
            let candidate = format!("custom-{n}");
            if !self.config.providers.iter().any(|p| p.id == candidate) {
                return candidate;
            }
            n += 1;
        }
    }

    fn add_provider(&mut self, cx: &mut Context<Self>) {
        let id = self.next_provider_id();
        let ix = self.config.providers.len();
        self.config.providers.push(ProviderConfig {
            id,
            name: rust_i18n::t!("settings.models.custom_provider").to_string(),
            base_url: "https://".into(),
            api_key: String::new(),
            api_format: ApiFormat::OpenAiChat,
            enabled: true,
            models: vec![],
            key_url: None,
        });
        self.selected = Some(ix);
        self.form_dirty = true;
        cx.emit(SettingsEvent::Save(self.config.clone()));
        cx.notify();
    }

    fn select_provider(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.selected = Some(ix);
        self.delete_armed = false;
        self.form_dirty = true;
        cx.notify();
    }

    fn format_ctx(context_window: u64) -> String {
        if context_window >= 1_000_000 {
            format!("{}M", context_window / 1_000_000)
        } else {
            format!("{}k", context_window / 1000)
        }
    }

    // ---------- Model dialog ----------
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.form_dirty {
            self.form_dirty = false;
            self.sync_form(window, cx);
        }
        if self.appearance_dirty {
            self.appearance_dirty = false;
            self.sync_appearance(window, cx);
        }
        if self.archived_ws_dirty {
            self.archived_ws_dirty = false;
            self.sync_archived_ws_options(window, cx);
        }

        // Official Settings component: ships a sidebar (search + page navigation,
        // selection persisted by id) plus page title/description/scrolling; fields
        // and custom content return to this view via WeakEntity to emit events
        let weak = cx.entity().downgrade();
        let settings = Settings::new("pig-settings")
            .with_group_variant(GroupBoxVariant::Fill)
            .pages(vec![
                // Appearance: all three dropdowns use Select (same 220 width and
                // same trigger/menu visual, the two font ones searchable); theme
                // mode is a stateful field, backfilled via appearance_dirty
                SettingPage::new(rust_i18n::t!("settings.appearance.title"))
                    .icon(IconName::Palette)
                    .description(rust_i18n::t!("settings.appearance.description"))
                    .group(
                        SettingGroup::new()
                            .title(rust_i18n::t!("settings.appearance.group_interface"))
                            .items(vec![
                                SettingItem::new(
                                    rust_i18n::t!("settings.appearance.theme_mode"),
                                    SettingField::element(SearchSelectField {
                                        select: self.theme_select.clone(),
                                        width: Some(px(220.)),
                                        menu_width: px(300.),
                                    }),
                                )
                                .description(
                                    rust_i18n::t!("settings.appearance.theme_mode_description")
                                        .as_ref(),
                                ),
                                SettingItem::new(
                                    rust_i18n::t!("settings.language.label").as_ref(),
                                    SettingField::element(SearchSelectField {
                                        select: self.language_select.clone(),
                                        width: Some(px(220.)),
                                        menu_width: px(300.),
                                    }),
                                )
                                .description(
                                    rust_i18n::t!("settings.language.description").as_ref(),
                                ),
                                self.font_item(FontSlot::Ui),
                            ]),
                    )
                    .group(
                        SettingGroup::new()
                            .title(rust_i18n::t!("settings.appearance.group_code"))
                            .items(vec![self.font_item(FontSlot::Mono)]),
                    ),
                Self::content_page(
                    rust_i18n::t!("settings.models.title").as_ref(),
                    IconName::Bot,
                    rust_i18n::t!("settings.models.description").as_ref(),
                    &["model", "provider"],
                    &weak,
                    Self::render_models_page,
                ),
                Self::terminal_page(&weak),
                Self::content_page(
                    rust_i18n::t!("settings.mcp.title").as_ref(),
                    IconName::Network,
                    rust_i18n::t!("settings.mcp.description").as_ref(),
                    &["mcp"],
                    &weak,
                    Self::render_mcp_page,
                ),
                Self::content_page(
                    rust_i18n::t!("settings.skills.title").as_ref(),
                    IconName::BookOpen,
                    rust_i18n::t!("settings.skills.description").as_ref(),
                    &["skill"],
                    &weak,
                    Self::render_skills_page,
                ),
                Self::content_page(
                    rust_i18n::t!("settings.websearch.title").as_ref(),
                    IconName::Search,
                    rust_i18n::t!("settings.websearch.description").as_ref(),
                    &["websearch"],
                    &weak,
                    Self::render_websearch,
                ),
                Self::content_page(
                    rust_i18n::t!("settings.archived.title").as_ref(),
                    IconName::Inbox,
                    rust_i18n::t!("settings.archived.description").as_ref(),
                    &["archive"],
                    &weak,
                    Self::render_archived_page,
                ),
            ]);

        div()
            .size_full()
            .relative()
            .bg(cx.theme().background)
            .key_context("settings")
            .on_action(cx.listener(|_, _: &crate::CloseSettings, _, cx| {
                cx.emit(SettingsEvent::Close);
            }))
            .child(settings)
            .when(self.preset_picker_open, |this| {
                this.child(
                    div()
                        .absolute()
                        .inset_0()
                        .child(self.render_preset_picker(cx)),
                )
            })
            .when(self.model_dialog.is_some(), |this| {
                this.child(
                    div()
                        .absolute()
                        .inset_0()
                        .child(self.render_model_dialog(cx)),
                )
            })
            .when(self.mcp_dialog.is_some(), |this| {
                this.child(div().absolute().inset_0().child(self.render_mcp_dialog(cx)))
            })
            .when(self.mcp_help_open, |this| {
                this.child(
                    div()
                        .absolute()
                        .inset_0()
                        .child(self.render_mcp_help_dialog(cx)),
                )
            })
            .when(self.skills_dialog.is_some(), |this| {
                this.child(
                    div()
                        .absolute()
                        .inset_0()
                        .child(self.render_skills_dialog(cx)),
                )
            })
            .when(self.skills_help_open, |this| {
                this.child(
                    div()
                        .absolute()
                        .inset_0()
                        .child(self.render_skills_help_dialog(cx)),
                )
            })
    }
}

impl SettingsView {
    // ---------- Page construction for the official Settings component ----------

    /// Font setting item: scrollable_dropdown (hundreds of local fonts, scrolling
    /// inside the popup); the "system default" sentinel maps config's None; the
    /// option list is cached in-process
    /// Font setting item: custom searchable field (FontSettingField); the state
    /// entity is held by this view, and confirm goes through set_font via
    /// subscription (write config + apply immediately + save)
    fn font_item(&self, slot: FontSlot) -> SettingItem {
        let select = match slot {
            FontSlot::Ui => self.ui_font_select.clone(),
            FontSlot::Mono => self.mono_font_select.clone(),
        };
        SettingItem::new(
            match slot {
                FontSlot::Ui => rust_i18n::t!("settings.appearance.ui_font"),
                FontSlot::Mono => rust_i18n::t!("settings.appearance.mono_font"),
            },
            SettingField::element(SearchSelectField {
                select,
                width: Some(px(220.)),
                menu_width: px(300.),
            }),
        )
        .description(match slot {
            FontSlot::Ui => rust_i18n::t!("settings.appearance.ui_font_description").to_string(),
            FontSlot::Mono => {
                rust_i18n::t!("settings.appearance.mono_font_description").to_string()
            }
        })
    }

    /// Custom content page: the existing whole-page render goes into a single item
    /// (the official component handles navigation/title/scrolling); keywords let
    /// the untitled custom item still be hit by sidebar search. The page title and
    /// description (already resolved to the current locale via t!) are appended to
    /// the keywords, so locale-specific search terms come from the translations and
    /// adding a language needs no code change; `keywords` only carries language-
    /// neutral technical synonyms.
    fn content_page(
        title: &str,
        icon: IconName,
        description: &str,
        keywords: &[&str],
        weak: &WeakEntity<Self>,
        content: fn(&mut Self, &mut Context<Self>) -> AnyElement,
    ) -> SettingPage {
        let weak = weak.clone();
        let mut all_keywords: Vec<&str> = keywords.to_vec();
        all_keywords.extend([title, description]);
        SettingPage::new(title)
            .icon(icon)
            .description(description)
            .group(
                SettingGroup::new().item(
                    SettingItem::render(move |_, _, cx: &mut App| {
                        weak.update(cx, content)
                            .unwrap_or_else(|_| div().into_any_element())
                    })
                    .keywords(all_keywords.iter().copied()),
                ),
            )
    }

    /// Model settings page content: left column (provider list) plus right column
    /// (detail form), each in an official GroupBox (Fill variant, same card color
    /// as the official Settings groups); the add-provider button sits at the top
    /// of the left column
    fn render_models_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        h_flex()
            .w_full()
            .items_start()
            .gap_4()
            .child(
                GroupBox::new()
                    .id("providers")
                    .fill()
                    .w(px(240.))
                    .title(
                        div()
                            .text_sm()
                            .font_medium()
                            .text_color(cx.theme().muted_foreground)
                            .child(rust_i18n::t!("settings.models.providers").to_string()),
                    )
                    .child(
                        Button::new("add-provider")
                            .outline()
                            .small()
                            .w_full()
                            .icon(IconName::Plus)
                            .label(rust_i18n::t!("settings.models.add_provider"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.preset_picker_open = true;
                                cx.notify();
                            })),
                    )
                    .children(
                        (0..self.config.providers.len()).map(|ix| self.render_provider_row(ix, cx)),
                    ),
            )
            .child(
                GroupBox::new()
                    .id("provider-detail")
                    .fill()
                    .flex_1()
                    .min_w_0()
                    .child(self.render_detail(cx)),
            )
            .into_any_element()
    }

    /// MCP page content: header (search/new/refresh) + server list
    fn render_mcp_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .gap_4()
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(rust_i18n::t!("settings.mcp.header_hint").to_string()),
                    )
                    // Header controls all use the small size, so the primary
                    // button doesn't visually overwhelm neighbors at the default
                    // size
                    .child(
                        div()
                            .w(px(180.))
                            .child(Input::new(&self.mcp_search).small()),
                    )
                    .child(
                        Button::new("new-mcp")
                            .primary()
                            .small()
                            .icon(IconName::Plus)
                            .label(rust_i18n::t!("settings.mcp.new_server"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_mcp_dialog(None, window, cx);
                            })),
                    )
                    .child(
                        Button::new("refresh-mcp")
                            .outline()
                            .small()
                            .icon(IconName::RotateCw)
                            .label(rust_i18n::t!("settings.common.refresh"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.refresh_mcp(cx);
                            })),
                    )
                    // Help toggle: the manual edit format and config file paths
                    // expand on demand (not persistent on the main page)
                    .child(
                        Button::new("mcp-help")
                            .outline()
                            .small()
                            .icon(IconName::Info)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.mcp_help_open = !this.mcp_help_open;
                                cx.notify();
                            })),
                    ),
            )
            .child(self.render_mcp(cx))
            .into_any_element()
    }

    /// Skills page content: header (search/new) + skill list
    fn render_skills_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .gap_4()
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(rust_i18n::t!("settings.skills.header_hint").to_string()),
                    )
                    .child(
                        div()
                            .w(px(180.))
                            .child(Input::new(&self.skills_search).small()),
                    )
                    .child(
                        Button::new("new-skill")
                            .primary()
                            .small()
                            .icon(IconName::Plus)
                            .label(rust_i18n::t!("settings.skills.new_skill"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_skills_dialog(None, window, cx);
                            })),
                    )
                    // Help toggle: the SKILL.md format and skill directory paths
                    // pop up on demand (not persistent on the main page)
                    .child(
                        Button::new("skills-help")
                            .outline()
                            .small()
                            .icon(IconName::Info)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.skills_help_open = !this.skills_help_open;
                                cx.notify();
                            })),
                    ),
            )
            .child(self.render_skills(cx))
            .into_any_element()
    }
}
