use gpui_kit::assets::IconName as AssetIconName;
use gpui_kit::component::attachment::{
    Attachment, AttachmentContent, AttachmentDescription, AttachmentGroup, AttachmentMedia,
    AttachmentTitle,
};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::command::{Command, CommandGroup, CommandItem, CommandState};
use gpui_kit::component::input::{
    InlineToken, InputEvent, InputState, InputToken, Textarea, TextareaState,
};
use gpui_kit::component::progress::ProgressCircle;
use gpui_kit::component::questionnaire::{
    Questionnaire, QuestionnaireActions, QuestionnaireChoice, QuestionnaireChoiceDefinition,
    QuestionnaireChoices, QuestionnaireDescription, QuestionnaireError, QuestionnaireEvent,
    QuestionnaireInput, QuestionnaireInputDefinition, QuestionnaireItem,
    QuestionnaireItemDefinition, QuestionnaireNext, QuestionnairePrevious, QuestionnaireProgress,
    QuestionnaireShortcutMode, QuestionnaireState, QuestionnaireSubmission, QuestionnaireSubmit,
    QuestionnaireTitle,
};
use gpui_kit::component::separator::Separator;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, Size, StyledExt as _, h_flex,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use pig_protocol::{
    ApprovalDecision, ExecMode, QuestionItem, QuestionOption, TaskStatus, TaskSummary, TodoItem,
    TodoStatus,
};

use crate::{ComposerNavDown, ComposerNavNext, ComposerNavPrev, ComposerNavUp, ComposerPopupClose};

/// Slash commands: (command name, i18n key of the description). Command names
/// are fixed English and never localized; descriptions are fetched by t! in
/// the current language.
const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/clear", "composer.slash_clear"),
    ("/compact", "composer.slash_compact"),
];

/// (name, description, mode) — the constant table stores only modes; names and
/// descriptions are fetched by t! in the current language
const EXEC_MODES: &[ExecMode] = &[
    ExecMode::ConfirmBeforeEdit,
    ExecMode::AutoEdit,
    ExecMode::FullAccess,
    ExecMode::Yolo,
];

fn exec_mode_label(mode: ExecMode) -> std::borrow::Cow<'static, str> {
    match mode {
        ExecMode::ConfirmBeforeEdit => rust_i18n::t!("composer.exec_confirm"),
        ExecMode::AutoEdit => rust_i18n::t!("composer.exec_auto_edit"),
        ExecMode::FullAccess => rust_i18n::t!("composer.exec_full_access"),
        ExecMode::Yolo => rust_i18n::t!("composer.exec_yolo"),
    }
}

fn exec_mode_description(mode: ExecMode) -> std::borrow::Cow<'static, str> {
    match mode {
        ExecMode::ConfirmBeforeEdit => rust_i18n::t!("composer.exec_confirm_desc"),
        ExecMode::AutoEdit => rust_i18n::t!("composer.exec_auto_edit_desc"),
        ExecMode::FullAccess => rust_i18n::t!("composer.exec_full_access_desc"),
        ExecMode::Yolo => rust_i18n::t!("composer.exec_yolo_desc"),
    }
}

fn exec_mode_icon(mode: ExecMode) -> AssetIconName {
    match mode {
        ExecMode::ConfirmBeforeEdit => AssetIconName::Hand,
        ExecMode::AutoEdit => AssetIconName::ShieldCheck,
        ExecMode::FullAccess => AssetIconName::ShieldAlert,
        // Unregulated keeps the alert icon (nothing more suitable among the
        // existing icons)
        ExecMode::Yolo => AssetIconName::ShieldAlert,
    }
}

/// Mode colors (popup row icon+label, composer chip): by risk level, neutral →
/// yellow → orange → red. Description text stays muted gray, uncolored.
fn exec_mode_color(mode: ExecMode, cx: &App) -> Hsla {
    let theme = cx.theme();
    match mode {
        // Neutral: default foreground, no extra coloring
        ExecMode::ConfirmBeforeEdit => theme.foreground,
        // Yellow/amber: middle tier
        ExecMode::AutoEdit => theme.warning,
        // Orange: aggressive but with guardrails (the theme has no orange
        // token, use Tailwind palette orange-500)
        ExecMode::FullAccess => gpui_kit::component::theme::orange_500(),
        // Red: no guardrails
        ExecMode::Yolo => theme.danger,
    }
}

/// The small "read/write outside" toggle in the mode popup footer: label
/// (text_xs muted) + Checkbox (no handler attached; clicks bubble to the outer
/// layer), with the text and checkbox forming one click area. Clicking toggles
/// and emits SetFsAccess without closing the popup.
fn fs_toggle(
    id: &'static str,
    label: std::borrow::Cow<'static, str>,
    checked: bool,
    is_read: bool,
    composer: Entity<Composer>,
    cx: &mut App,
) -> AnyElement {
    let (accent, radius, muted) = {
        let theme = cx.theme();
        (theme.accent, theme.radius, theme.muted_foreground)
    };
    div()
        .id(SharedString::from(format!("{id}-toggle")))
        .cursor_pointer()
        .rounded(radius)
        .hover(move |style| style.bg(accent))
        .child(
            h_flex()
                .gap_1()
                .items_center()
                .child(div().text_xs().text_color(muted).child(label))
                .child(Checkbox::new(id).checked(checked).tab_stop(false)),
        )
        .on_click(move |_, _window, cx| {
            composer.update(cx, |this, cx| {
                if is_read {
                    this.fs_read_outside = !this.fs_read_outside;
                } else {
                    this.fs_write_outside = !this.fs_write_outside;
                }
                cx.emit(ComposerEvent::SetFsAccess {
                    read_outside: this.fs_read_outside,
                    write_outside: this.fs_write_outside,
                });
                cx.notify();
            });
        })
        .into_any_element()
}

/// Task duration: started→ended (or until now), "N seconds / N minutes".
fn format_task_duration(started_at: u64, end: u64) -> String {
    let secs = end.saturating_sub(started_at);
    if secs < 60 {
        rust_i18n::t!("composer.task_duration_seconds", n = secs).to_string()
    } else {
        rust_i18n::t!("composer.task_duration_minutes", n = secs / 60).to_string()
    }
}

/// Questionnaire choice values use the option labels directly (submissions
/// send back labels, consistent with the protocol): dedupe by label preserving
/// order — guards against the schema DuplicateChoice error and rendering id
/// conflicts when the server emits duplicate labels.
fn dedup_question_options(question: &QuestionItem) -> Vec<&QuestionOption> {
    let mut seen = std::collections::HashSet::new();
    question
        .options
        .iter()
        .filter(|option| seen.insert(option.label.as_str()))
        .collect()
}
/// (provider name, provider_id, model_id, reasoning level list [(id, display
/// name)])
pub type ModelOption = (String, String, String, Vec<(String, String)>);

/// Operation awaiting approval: while the approval is pending the composer is
/// hidden and the approval bar is shown.
#[derive(Clone)]
pub struct PendingApproval {
    /// This approval's wait id on the core side (for directing the decision
    /// reply)
    pub request_id: String,
    /// Session the approval came from (for building the ExitPlanMode plan file
    /// path)
    pub session_id: String,
    pub tool: String,
    /// Bash is the raw command; Write/Edit is a diff preview; ExitPlanMode is
    /// the full plan text
    pub detail: String,
    /// Reason key for a high-risk command (core bash.rs DangerReason.key; the
    /// approval card shows a localized warning line above the detail, see
    /// danger_reason_text)
    pub danger_key: Option<String>,
    pub cwd: String,
}

impl PendingApproval {
    /// Absolute path of the ExitPlanMode plan file (core persists it before
    /// raising the approval)
    pub fn plan_path(&self) -> String {
        format!("{}/.pigcode/plans/plan-{}.md", self.cwd, self.session_id)
    }
}

/// danger_key → localized risk reason text: a static match over the six known
/// keys (no dynamic key concatenation; unknown keys — added by future core —
/// return None and show no warning line; a guard test keeps all six keys
/// covered).
pub(crate) fn danger_reason_text(key: &str) -> Option<String> {
    let text = match key {
        "fork_bomb" => rust_i18n::t!("approval.danger.fork_bomb"),
        "rm_rf_root" => rust_i18n::t!("approval.danger.rm_rf_root"),
        "disk_format" => rust_i18n::t!("approval.danger.disk_format"),
        "dd_block" => rust_i18n::t!("approval.danger.dd_block"),
        "shutdown" => rust_i18n::t!("approval.danger.shutdown"),
        "chmod_root" => rust_i18n::t!("approval.danger.chmod_root"),
        _ => return None,
    };
    Some(text.to_string())
}

/// Structured question awaiting an answer: while the question bar is shown the
/// input area is hidden (mutually exclusive with the approval bar; the
/// question takes priority).
#[derive(Clone)]
pub struct PendingQuestion {
    pub request_id: String,
    pub questions: Vec<QuestionItem>,
}

/// Task panel filter tabs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TaskFilter {
    Running,
    Finished,
    All,
}

impl TaskFilter {
    const TABS: &[TaskFilter] = &[TaskFilter::Running, TaskFilter::Finished, TaskFilter::All];

    fn label(self) -> std::borrow::Cow<'static, str> {
        match self {
            TaskFilter::Running => rust_i18n::t!("composer.filter_running"),
            TaskFilter::Finished => rust_i18n::t!("composer.filter_finished"),
            TaskFilter::All => rust_i18n::t!("composer.filter_all"),
        }
    }

    fn matches(self, status: TaskStatus) -> bool {
        match self {
            TaskFilter::Running => matches!(status, TaskStatus::Running),
            TaskFilter::Finished => !matches!(status, TaskStatus::Running),
            TaskFilter::All => true,
        }
    }
}

/// Background task chip kinds (same split as kimi-code): Bash background
/// tasks / subagent (Agent) tasks, dispatched by TaskSummary.agent_id — chips
/// and popups show and hide independently
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskChipKind {
    Bash,
    Agent,
}

impl TaskChipKind {
    fn matches(self, task: &TaskSummary) -> bool {
        match self {
            Self::Bash => task.agent_id.is_none(),
            Self::Agent => task.agent_id.is_some(),
        }
    }

    fn label(self, running: usize) -> String {
        let name = match self {
            Self::Bash => rust_i18n::t!("composer.chip_bash"),
            Self::Agent => rust_i18n::t!("composer.chip_agent"),
        };
        if running > 0 {
            rust_i18n::t!("composer.chip_running", name = name.as_ref(), n = running).to_string()
        } else {
            name.to_string()
        }
    }
}

#[derive(Clone)]
pub enum ComposerEvent {
    Send {
        text: String,
        files: Vec<String>,
        /// Image attachments pasted from the clipboard (raw bytes, compressed
        /// on the core side)
        images: Vec<pig_protocol::PendingImage>,
        mode: ExecMode,
    },
    Stop,
    Clear,
    Compact {
        /// Focus notes the user appended after the chip when staging the
        /// command (absent = a plain compact)
        instruction: Option<String>,
    },
    SetModel {
        provider_id: String,
        model_id: String,
    },
    /// Path link in the plan approval panel: opens the plan file in the right
    /// "File" tab
    OpenFile {
        path: String,
    },
    SetReasoning(Option<String>),
    OpenSettings,
    SetExecMode(ExecMode),
    /// Plan mode toggle (orthogonal to exec mode; popup checkbox / chip close)
    SetPlanMode(bool),
    /// "Unregulated mode" selected: show the confirmation dialog first (hosted
    /// by AppView); only after confirmation proceed to SetExecMode
    RequestYoloConfirm,
    /// "Read/write outside workspace" toggles in the mode menu
    SetFsAccess {
        read_outside: bool,
        write_outside: bool,
    },
    SearchFiles(String),
    /// hero: open the system directory picker
    PickDirectory,
    /// hero: pick a recent directory
    SelectCwd(String),
    /// hero: cancel the workspace selection (work outside a workspace)
    ClearCwd,
    /// hero: switch git branch
    CheckoutBranch(String),
    /// Approval bar: approve / approve for this session / reject (directed at
    /// the request_id attached to the bar — with concurrent approvals queued,
    /// each request is answered individually, not indiscriminately as "the
    /// last one")
    DecideApproval {
        request_id: String,
        decision: ApprovalDecision,
        /// Feedback (kimi Revise: carried when the plan "Revise" is submitted;
        /// None for other approvals)
        feedback: Option<String>,
    },
    /// Question bar: submit (Some = selected labels per question) / skip (None)
    QuestionReply {
        request_id: String,
        answers: Option<Vec<Vec<String>>>,
    },
    /// Changes chip: directly opens the right panel's Changes tab (no popup)
    OpenChanges,
    /// Task row click in the "background Agent" popup: opens the right
    /// subagent conversation tab (title uses the task's raw command; AppView
    /// handles it via open_subagent_tab)
    OpenSubagent {
        agent_id: String,
        title: String,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Popup {
    Mention,
    Slash,
    ExecMode,
    Model,
    Reasoning,
    Cwd,
    Branch,
    Context,
    Todos,
    Tasks,
    /// Popup of the "background Agent" chip (split from Tasks (background
    /// Bash), with independent show/hide and close)
    AgentTasks,
}

impl EventEmitter<ComposerEvent> for Composer {}

/// Horizontal anchor of a popup relative to its trigger chip.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PopupAnchor {
    Left,
    Right,
    /// The popup's horizontal centerline aligns with the chip's centerline
    /// (used by the context capacity panel).
    Center,
}

pub struct Composer {
    input: Entity<TextareaState>,
    exec_mode: usize,
    /// Plan mode toggle (orthogonal to exec_mode; when on, the bar shows a
    /// separate "Plan" chip)
    plan_enabled: bool,
    /// Session-level "read/write outside workspace" toggles (the two
    /// checkboxes in the mode menu)
    fs_read_outside: bool,
    fs_write_outside: bool,
    model: String,
    models: Vec<ModelOption>,
    reasoning_level: Option<String>,
    popup: Option<(Popup, usize)>,
    /// The most recent popup closed by on_mouse_down_out plus the press
    /// position: clicking a chip while its popup is open triggers outside-close
    /// first, and the click from the same press arrives right after — swallow
    /// it by press position to avoid "collapse then instantly reopen".
    outside_closed: Option<(Popup, Point<Pixels>)>,
    streaming: bool,
    /// Pending approval: Some hides the input area and shows the approval bar
    approval: Option<PendingApproval>,
    /// Plan markdown view for ExitPlanMode approvals (the kimi plan approval
    /// panel body; created and released along with set_approval/
    /// decide_approval)
    plan_state: Option<Entity<gpui_kit::component::text::TextViewState>>,
    /// Plan "Revise" input state (kimi Revise): when true, the panel bottom
    /// shows a feedback input; submitting rejects with the feedback carried to
    /// the model for revision
    plan_revise: bool,
    /// Feedback input (lazily created, reused with the panel)
    plan_revise_input: Option<Entity<InputState>>,
    /// Approval bar focus (receives the ⏎ / Ctrl+⏎ / Esc shortcuts)
    approval_focus: FocusHandle,
    /// Whether focus has already been claimed for the current approval bar
    /// (claimed once per appearance)
    approval_focused: bool,
    /// Question awaiting an answer: Some hides the input area and shows the
    /// question bar (mutually exclusive with approval; the question takes
    /// priority)
    question: Option<PendingQuestion>,
    /// Questionnaire entity and event subscription for the question bar:
    /// InputState needs a window to build, so it is built lazily in render;
    /// released on request_id change / submit / abandon / clear
    questionnaire: Option<(Entity<QuestionnaireState>, Subscription)>,
    /// Current questionnaire page index (0-based): a mirror of
    /// CurrentItemChanged events (debug_question has no cx and cannot read the
    /// entity)
    question_current: usize,
    /// Question bar focus (receives Esc to abandon)
    question_focus: FocusHandle,
    question_focused: bool,
    mention_results: Vec<String>,
    /// Keyboard-selected item of the / and @ popups (Tab/↑↓ to move; reset
    /// when a popup opens / the query changes / results refresh)
    popup_sel: usize,
    /// Scroll handle of the / and @ popup lists (the selected item scrolls
    /// into view while navigating)
    popup_scroll: ScrollHandle,
    context_usage: Option<(u64, u64, u64, u64)>,
    /// Chips above the input area: TodoList progress / background Bash task
    /// snapshots (core-pushed); click to open a read-only panel
    todos: Vec<TodoItem>,
    tasks: Vec<TaskSummary>,
    task_filter: TaskFilter,
    /// This session's change stats and file list (pushed from ReviewPanel
    /// snapshots)
    changes: (u32, u32),
    change_files: Vec<(String, u32, u32)>,
    /// Task row id whose output tail is expanded
    expanded_task: Option<String>,
    /// Image attachments pasted from the clipboard (shown in the chip strip;
    /// sent as PendingImage on send, cleared afterwards)
    pasted_images: Vec<PastedImage>,
    /// Paste note (e.g. over the 8-image limit); cleared on the next
    /// successful paste
    paste_note: Option<String>,
    hero_mode: bool,
    hero_cwds: Vec<String>,
    hero_cwd: Option<String>,
    hero_cwd_label: String,
    cwd_command: Entity<CommandState>,
    hero_branch: Option<String>,
    hero_branches: Vec<String>,
    hero_is_git: bool,
    branch_command: Entity<CommandState>,
    exec_command: Entity<CommandState>,
    model_command: Entity<CommandState>,
    reasoning_command: Entity<CommandState>,
    placeholder_applied: String,
    _subscriptions: Vec<Subscription>,
}

mod aux_panel;
mod bar_popups;
mod images;
mod popups;
mod question;
mod tasks;
#[cfg(test)]
mod tests;

use images::*;

impl Composer {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(rust_i18n::t!("composer.placeholder_idle"))
                .auto_grow(2, 8)
                .submit_on_enter(true)
        });

        let _subscriptions = vec![cx.subscribe_in(
            &input,
            window,
            |this: &mut Self, input, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { shift, .. } if !shift => {
                    // A / or @ popup is open with candidates: Enter confirms
                    // the selection (no send)
                    if this.popup_selection_active(cx) {
                        this.confirm_selection(window, cx);
                    } else {
                        this.send(window, cx);
                    }
                }
                InputEvent::Change => this.update_suggestion(input, cx),
                _ => {}
            },
        )];

        Self {
            input,
            exec_mode: 1,
            plan_enabled: false,
            fs_read_outside: false,
            fs_write_outside: false,
            model: String::new(),
            models: vec![],
            reasoning_level: None,
            popup: None,
            outside_closed: None,
            streaming: false,
            approval: None,
            plan_state: None,
            plan_revise: false,
            plan_revise_input: None,
            approval_focus: cx.focus_handle(),
            approval_focused: false,
            question: None,
            questionnaire: None,
            question_current: 0,
            question_focus: cx.focus_handle(),
            question_focused: false,
            mention_results: Vec::new(),
            popup_sel: 0,
            popup_scroll: ScrollHandle::new(),
            context_usage: None,
            todos: Vec::new(),
            tasks: Vec::new(),
            task_filter: TaskFilter::Running,
            changes: (0, 0),
            change_files: Vec::new(),
            expanded_task: None,
            pasted_images: Vec::new(),
            paste_note: None,
            hero_mode: false,
            hero_cwds: Vec::new(),
            hero_cwd: None,
            hero_cwd_label: String::new(),
            cwd_command: cx.new(|cx| CommandState::new(window, cx)),
            hero_branch: None,
            hero_branches: Vec::new(),
            hero_is_git: false,
            branch_command: cx.new(|cx| CommandState::new(window, cx)),
            exec_command: cx.new(|cx| CommandState::new(window, cx)),
            model_command: cx.new(|cx| CommandState::new(window, cx)),
            reasoning_command: cx.new(|cx| CommandState::new(window, cx)),
            placeholder_applied: rust_i18n::t!("composer.placeholder_idle").to_string(),
            _subscriptions,
        }
    }

    pub fn set_streaming(&mut self, streaming: bool, cx: &mut Context<Self>) {
        self.streaming = streaming;
        cx.notify();
    }

    pub fn set_hero_mode(&mut self, hero: bool, cx: &mut Context<Self>) {
        self.hero_mode = hero;
        cx.notify();
    }

    #[allow(clippy::too_many_arguments)]
    pub fn set_hero_info(
        &mut self,
        cwd: Option<String>,
        cwd_label: String,
        cwds: Vec<String>,
        branch: Option<String>,
        branches: Vec<String>,
        is_git: bool,
        cx: &mut Context<Self>,
    ) {
        self.hero_cwd = cwd;
        self.hero_cwd_label = cwd_label;
        self.hero_cwds = cwds;
        self.hero_branch = branch;
        self.hero_branches = branches;
        self.hero_is_git = is_git;
        cx.notify();
    }

    /// Suggestion chip: fill in guiding text (without sending).
    pub fn fill_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| {
            input.set_value(text, window, cx);
            input.focus(window, cx);
        });
    }

    pub fn set_model_name(&mut self, model: String, cx: &mut Context<Self>) {
        // Empty string = the "no model configured" sentinel (rendered as the
        // localized placeholder at draw time, so a language switch updates it)
        self.model = model;
        cx.notify();
    }

    /// Bar-chip label for the current model: the empty sentinel maps to the
    /// localized "no model configured" placeholder
    fn model_display(&self) -> String {
        if self.model.is_empty() {
            rust_i18n::t!("composer.no_model").to_string()
        } else {
            self.model.clone()
        }
    }

    pub fn set_models(&mut self, models: Vec<ModelOption>, cx: &mut Context<Self>) {
        self.models = models;
        cx.notify();
    }

    /// For selftest.
    pub fn debug_model_count(&self) -> usize {
        self.models.len()
    }

    /// For selftest.
    #[allow(dead_code)]
    pub fn debug_reasoning_level(&self) -> Option<String> {
        self.reasoning_level.clone()
    }

    /// Pending approval operation: Some hides the input area and shows the
    /// approval bar; None restores the input.
    pub fn set_approval(&mut self, approval: Option<PendingApproval>, cx: &mut Context<Self>) {
        // Plan panel body (ExitPlanMode only): rebuild only when request_id
        // changes; repeated syncs do not lose the scroll position
        let recreate = match (&approval, &self.approval) {
            (Some(new), Some(old)) => new.request_id != old.request_id,
            (Some(_), None) => true,
            _ => false,
        };
        if recreate {
            self.plan_revise = false;
            self.plan_state =
                match &approval {
                    Some(a) if a.tool == "ExitPlanMode" => Some(cx.new(|cx| {
                        gpui_kit::component::text::TextViewState::markdown(&a.detail, cx)
                    })),
                    _ => None,
                };
        } else if approval.is_none() {
            self.plan_revise = false;
            self.plan_state = None;
        }
        self.approval = approval;
        cx.notify();
    }

    /// Approval bar decision: clear the approval state, emit the event
    /// (directed via this bar's request_id), and return focus to the input.
    /// feedback is non-None only on the plan "Revise" submit path (kimi Revise
    /// carries it to the model for revision)
    fn decide_approval(
        &mut self,
        decision: ApprovalDecision,
        feedback: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(approval) = self.approval.take() {
            self.plan_state = None;
            self.plan_revise = false;
            cx.emit(ComposerEvent::DecideApproval {
                request_id: approval.request_id,
                decision,
                feedback,
            });
            self.input.update(cx, |input, cx| input.focus(window, cx));
            cx.notify();
        }
    }

    /// Plan "Revise" submit: reject carrying the feedback text (kimi Revise;
    /// empty text = a bare reject)
    fn submit_plan_revise(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let feedback = self
            .plan_revise_input
            .as_ref()
            .map(|input| input.read(cx).value().to_string())
            .filter(|text| !text.trim().is_empty());
        self.decide_approval(ApprovalDecision::Reject, feedback, window, cx);
    }

    /// Plan "Revise" cancel: back to the three-button state, clear the input,
    /// return focus to the approval bar
    fn cancel_plan_revise(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.plan_revise = false;
        if let Some(input) = &self.plan_revise_input {
            input.update(cx, |input, cx| input.set_value("", window, cx));
        }
        self.approval_focus.focus(window, cx);
        cx.notify();
    }

    /// Question awaiting an answer: Some shows the question bar; None clears
    /// it (after submit/abandon/turn end). When request_id changes (or is
    /// cleared) the questionnaire entity is released too (rebuilt lazily in
    /// render); repeated syncs of the same question keep the questionnaire
    /// (paging and selected state are not lost).
    pub fn set_question(&mut self, question: Option<PendingQuestion>, cx: &mut Context<Self>) {
        let changed = match (&self.question, &question) {
            (Some(old), Some(new)) => old.request_id != new.request_id,
            (None, None) => false,
            _ => true,
        };
        if changed {
            self.questionnaire = None;
            self.question_current = 0;
        }
        self.question = question;
        cx.notify();
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Command staging: input starts with a command token → dispatch as
        // "command + appended text", sending no chat message
        if let Some((command, args)) = self.command_mode(cx) {
            self.input.update(cx, |state, cx| {
                state.set_value("", window, cx);
            });
            self.popup = None;
            match command.as_str() {
                "/compact" => cx.emit(ComposerEvent::Compact { instruction: args }),
                "/clear" => cx.emit(ComposerEvent::Clear),
                _ => {}
            }
            cx.notify();
            return;
        }
        let text = self.input.read(cx).value().trim().to_string();
        // Allow sending empty text when there are image attachments
        if text.is_empty() && self.pasted_images.is_empty() {
            return;
        }
        // @mentions are already stored as atomic InlineTokens: collect files
        // from the token list (no dedup, matching the old whitespace-splitting
        // behavior); @ in plain text no longer counts
        let files: Vec<String> = self
            .input
            .read(cx)
            .tokens()
            .iter()
            .filter_map(|span| span.token().text().strip_prefix('@'))
            .map(str::to_string)
            .collect();
        // Attachments are sent with the message and cleared (the chip strip
        // disappears)
        let images: Vec<pig_protocol::PendingImage> = std::mem::take(&mut self.pasted_images)
            .into_iter()
            .map(|image| pig_protocol::PendingImage {
                bytes: (*image.bytes).clone(),
                mime: image.mime.clone(),
            })
            .collect();
        self.paste_note = None;
        self.input.update(cx, |state, cx| {
            state.set_value("", window, cx);
        });
        self.popup = None;
        cx.emit(ComposerEvent::Send {
            text,
            files,
            images,
            mode: EXEC_MODES[self.exec_mode],
        });
        cx.notify();
    }

    /// Detect an @ or / trigger in the text before the caret, returning the
    /// trigger position and query string.
    fn detect_trigger(head: &str) -> Option<(Popup, usize)> {
        for (ix, ch) in head.char_indices().rev() {
            let boundary = ix == 0 || head[..ix].ends_with(char::is_whitespace);
            match ch {
                '@' if boundary => return Some((Popup::Mention, ix)),
                '/' if boundary => return Some((Popup::Slash, ix)),
                c if c.is_whitespace() => return None,
                _ => {}
            }
        }
        None
    }

    fn update_suggestion(&mut self, input: &Entity<TextareaState>, cx: &mut Context<Self>) {
        let value = input.read(cx).value();
        let caret = input.read(cx).selected_range().start.min(value.len());
        self.popup = Self::detect_trigger(&value[..caret]);
        // Trigger position inside a token range → not a real trigger: after
        // deleting the trailing space of a chip/@token the caret sits right at
        // the token tail, and a plain-text scan would treat the //@ inside the
        // token as a trigger again (the shared "popup resurrection" pitfall of
        // command chips and @mentions)
        if let Some((_kind, start)) = self.popup {
            let inside_token = input
                .read(cx)
                .tokens()
                .iter()
                .any(|span| span.range().start <= start && start < span.range().end);
            if inside_token {
                self.popup = None;
            }
        }
        // A changed query returns to the first item (matching the "filtered
        // list changed" intuition)
        self.popup_sel = 0;
        if let Some((Popup::Mention, start)) = self.popup {
            let query = value[start + 1..caret].to_string();
            cx.emit(ComposerEvent::SearchFiles(query));
        }
        cx.notify();
    }

    pub fn set_mention_results(&mut self, results: Vec<String>, cx: &mut Context<Self>) {
        self.mention_results = results;
        self.popup_sel = 0;
        cx.notify();
    }

    /// Current candidate count of the / and @ popups (navigation/confirmation
    /// and rendering share the same filtered view)
    fn popup_nav_count(&self, cx: &App) -> usize {
        match self.popup_query(cx).map(|(kind, _, query)| (kind, query)) {
            Some((Popup::Slash, query)) => Self::slash_filtered(&query).len(),
            Some((Popup::Mention, _)) => self.mention_results.len(),
            _ => 0,
        }
    }

    /// Popup open with candidates: Enter should confirm the selection, not
    /// send
    fn popup_selection_active(&self, cx: &App) -> bool {
        matches!(self.popup, Some((Popup::Mention | Popup::Slash, _)))
            && self.popup_nav_count(cx) > 0
    }

    fn slash_filtered(query: &str) -> Vec<(&'static str, std::borrow::Cow<'static, str>)> {
        SLASH_COMMANDS
            .iter()
            .filter(|(name, _)| name[1..].contains(query))
            .map(|&(name, key)| (name, rust_i18n::t!(key)))
            .collect()
    }

    /// Tab/↑/↓ navigation: when a popup is open with candidates, cycle
    /// through them and scroll into view; otherwise cx.propagate() defers to
    /// the input's native behavior (caret movement/indent)
    fn nav_popup(&mut self, delta: i32, cx: &mut Context<Self>) {
        let count = self.popup_nav_count(cx);
        if count == 0 {
            cx.propagate();
            return;
        }
        self.popup_sel = (self.popup_sel as i32 + delta).rem_euclid(count as i32) as usize;
        self.popup_scroll.scroll_to_item(self.popup_sel);
        cx.notify();
    }

    /// Esc (app-layer binding on the Input context): close the / and @ popups
    /// when open; for other popups (Command panels have their own Cancel
    /// chain) and no popup, propagate through
    fn close_popup_key(&mut self, cx: &mut Context<Self>) {
        if matches!(self.popup, Some((Popup::Mention | Popup::Slash, _))) {
            self.popup = None;
            cx.notify();
        } else {
            cx.propagate();
        }
    }

    /// Enter/click confirms the current selection: Slash stages a command
    /// token (not executed), Mention inserts a file token
    fn confirm_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((kind, _start, query)) = self.popup_query(cx) else {
            return;
        };
        match kind {
            Popup::Slash => {
                let items = Self::slash_filtered(&query);
                let Some((name, _)) = items
                    .get(self.popup_sel.min(items.len().saturating_sub(1)))
                    .cloned()
                else {
                    return;
                };
                self.stage_command(name, window, cx);
            }
            Popup::Mention => {
                let Some(path) = self
                    .mention_results
                    .get(
                        self.popup_sel
                            .min(self.mention_results.len().saturating_sub(1)),
                    )
                    .cloned()
                else {
                    return;
                };
                self.insert_file(path, window, cx);
            }
            _ => {}
        }
    }

    /// Command staging: the chosen slash command is replaced with a leading
    /// InlineToken (chip) and the caret stays after the token for more input —
    /// dispatch happens on send as "command + appended text", not on selection
    /// (matching kimi-code's "Compact focus: xxxx" shape after selecting
    /// /compact)
    fn stage_command(
        &mut self,
        command: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((Popup::Slash, start)) = self.popup else {
            return;
        };
        let caret = self.input.read(cx).selected_range().start;
        let label = if command == "/compact" {
            "Compact"
        } else {
            "Clear"
        };
        let token = InlineToken::new(command.to_string(), command.to_string()).with_label(label);
        self.input.update(cx, |input, cx| {
            if input
                .replace_range_with_token(start..caret, token, window, cx)
                .is_ok()
            {
                // Same as @mentions: the token API adds no separator, so
                // append a trailing space
                input.replace(" ", window, cx);
            } else {
                input.set_selected_range(start..caret, cx);
                input.replace(format!("{command} "), window, cx);
            }
            input.focus(window, cx);
        });
        self.popup = None;
        cx.notify();
    }

    /// Command mode: input starts with a /compact or /clear command token →
    /// returns (command, appended text)
    fn command_mode(&self, cx: &App) -> Option<(String, Option<String>)> {
        let value = self.input.read(cx).value();
        for span in self.input.read(cx).tokens() {
            let text = span.token().text();
            if (text == "/compact" || text == "/clear")
                && span.range().start == 0
                && value.starts_with(text.as_str())
            {
                let args = value[text.len()..].trim();
                return Some((
                    text.to_string(),
                    (!args.is_empty()).then(|| args.to_string()),
                ));
            }
        }
        None
    }

    /// For selftest.
    pub fn debug_mention_results(&self) -> &[String] {
        &self.mention_results
    }

    pub fn set_context_usage(
        &mut self,
        used: u64,
        total: u64,
        cache_read_total: u64,
        input_total: u64,
        cx: &mut Context<Self>,
    ) {
        self.context_usage = Some((used, total, cache_read_total, input_total));
        cx.notify();
    }

    /// Clear the previous session's watermark on session switch / returning
    /// to hero (nothing shows until the new session's ContextUsage arrives)
    pub fn clear_context_usage(&mut self, cx: &mut Context<Self>) {
        if self.context_usage.take().is_some() {
            cx.notify();
        }
    }

    pub fn set_exec_mode(&mut self, mode: ExecMode, cx: &mut Context<Self>) {
        if let Some(ix) = EXEC_MODES.iter().position(|m| *m == mode) {
            self.exec_mode = ix;
        }
        cx.notify();
    }

    /// Plan mode toggle (synced by SessionConfigured/PlanModeChanged, or the
    /// popup checkbox)
    pub fn set_plan_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.plan_enabled = enabled;
        cx.notify();
    }

    /// Current plan toggle (carried when hero creates a session)
    pub fn plan_enabled(&self) -> bool {
        self.plan_enabled
    }

    /// Refocus the input (called by AppView after dialogs/popups close)
    pub fn focus_input(&self, window: &mut Window, cx: &mut App) {
        self.input.update(cx, |input, cx| input.focus(window, cx));
    }

    /// Restore the session-persisted outside read/write toggles (synced from
    /// meta on session switch/creation/replay)
    pub fn set_fs_access(
        &mut self,
        read_outside: bool,
        write_outside: bool,
        cx: &mut Context<Self>,
    ) {
        self.fs_read_outside = read_outside;
        self.fs_write_outside = write_outside;
        cx.notify();
    }

    /// Restore the session-persisted reasoning level (synced by
    /// SessionConfigured on session switch)
    pub fn set_reasoning_level(&mut self, level: Option<String>, cx: &mut Context<Self>) {
        self.reasoning_level = level;
        cx.notify();
    }

    /// For selftest.
    pub fn debug_exec_mode(&self) -> ExecMode {
        EXEC_MODES[self.exec_mode]
    }

    /// Automatic token-count units: <1k as-is; k/M levels show an integer
    /// when evenly divisible, otherwise one decimal
    fn format_tokens_compact(n: u64) -> String {
        if n < 1_000 {
            n.to_string()
        } else if n < 1_000_000 {
            let k = n as f64 / 1_000.0;
            if k.fract().abs() < 0.05 {
                format!("{}k", k.round() as u64)
            } else {
                format!("{k:.1}k")
            }
        } else {
            let m = n as f64 / 1_000_000.0;
            if m.fract().abs() < 0.05 {
                format!("{}M", m.round() as u64)
            } else {
                format!("{m:.1}M")
            }
        }
    }
}

impl Render for Composer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let desired_placeholder = if self.streaming {
            rust_i18n::t!("composer.placeholder_streaming")
        } else {
            rust_i18n::t!("composer.placeholder_idle")
        };
        if self.placeholder_applied != desired_placeholder {
            self.placeholder_applied = desired_placeholder.to_string();
            self.input.update(cx, |input, cx| {
                input.set_placeholder(desired_placeholder, window, cx);
            });
        }
        let approval = self.approval.clone();
        // One-time focus handover when the approval bar appears/disappears:
        // on appearance it claims focus to receive the ⏎/Esc shortcuts; after
        // disappearance (decision or turn end) focus returns to the input
        match (&approval, self.approval_focused) {
            (Some(_), false) => {
                self.approval_focused = true;
                self.approval_focus.focus(window, cx);
            }
            (None, true) => {
                self.approval_focused = false;
                self.input.update(cx, |input, cx| input.focus(window, cx));
            }
            _ => {}
        }
        let question = self.question.clone();
        self.ensure_questionnaire(window, cx);
        // Question bar focus handover (same pattern as the approval bar;
        // mutually exclusive with approval, the question takes priority): on
        // appearance focus goes to the questionnaire's current item
        // (receiving number keys/⏎); after disappearance focus returns to
        // the input
        match (&question, self.question_focused) {
            (Some(_), false) => {
                self.question_focused = true;
                if let Some(state) = self.questionnaire.as_ref().map(|(state, _)| state.clone()) {
                    state.update(cx, |state, cx| {
                        state.focus_current_item(window, cx);
                    });
                }
            }
            (None, true) => {
                self.question_focused = false;
                self.input.update(cx, |input, cx| input.focus(window, cx));
            }
            _ => {}
        }
        let cwd_open = matches!(self.popup, Some((Popup::Cwd, _)));
        let cwd_popup = cwd_open.then(|| self.render_cwd_popup(cx));
        let branch_open = matches!(self.popup, Some((Popup::Branch, _)));
        let branch_popup = branch_open.then(|| self.render_branch_popup(cx));
        let exec_open = matches!(self.popup, Some((Popup::ExecMode, _)));
        let exec_popup = exec_open.then(|| self.render_exec_mode_popup(cx));
        let model_open = matches!(self.popup, Some((Popup::Model, _)));
        let model_popup = model_open.then(|| self.render_model_popup(cx));
        let reasoning_levels: Vec<(String, String)> = self
            .models
            .iter()
            .find(|(_, _, model_id, _)| self.model.ends_with(&format!("/{model_id}")))
            .map(|(_, _, _, levels)| levels.clone())
            .unwrap_or_default();
        // Display name of the currently selected level (falls back to the id
        // itself by default)
        let reasoning_label = self.reasoning_level.as_ref().map(|id| {
            reasoning_levels
                .iter()
                .find(|(level_id, _)| level_id == id)
                .map(|(_, label)| label.clone())
                .unwrap_or_else(|| id.clone())
        });
        let reasoning_open = matches!(self.popup, Some((Popup::Reasoning, _)));
        let can_send = !self.input.read(cx).value().trim().is_empty();
        let reasoning_popup =
            reasoning_open.then(|| self.render_reasoning_popup(&reasoning_levels, cx));
        let context_open = matches!(self.popup, Some((Popup::Context, _)));
        let context_popup =
            (context_open && self.context_usage.is_some()).then(|| self.render_context_popup(cx));
        let aux_open = matches!(
            self.popup,
            Some((Popup::Todos | Popup::Tasks | Popup::AgentTasks, _))
        );
        let palette_open = cwd_open
            || branch_open
            || exec_open
            || model_open
            || reasoning_open
            || context_open
            || aux_open;
        let popup = if palette_open {
            None
        } else {
            self.render_popup(cx)
        };

        // Composer container surface color: in dark mode, brightened to
        // around neutral-850 to float above the window background
        // (referencing the official site's message-scroller input); popup
        // panels use the theme's dark popover plus a border boundary.
        let composer_surface = if cx.theme().is_dark() {
            hsla(0., 0., 0.11, 1.)
        } else {
            cx.theme().popover
        };
        let dark = cx.theme().is_dark();

        // Layered frame and content: GPUI paints an element's border after
        // all descendants (style.paint draws the background first and the
        // border only after the children); if the border stays on the content
        // container, popups above get crossed by the container's top border
        // line. Splitting the border/background into a separate underlying
        // sibling painted first lets popups cover it normally.
        div()
            .w_full()
            .p_3()
            // Keyboard navigation for the / and @ popups (actions bubble from
            // the input; when a popup is closed the handler cx.propagate()s
            // through, falling back to the input's native behavior)
            .on_action(cx.listener(|this, _: &ComposerNavUp, _, cx| this.nav_popup(-1, cx)))
            .on_action(cx.listener(|this, _: &ComposerNavDown, _, cx| this.nav_popup(1, cx)))
            .on_action(cx.listener(|this, _: &ComposerNavNext, _, cx| this.nav_popup(1, cx)))
            .on_action(cx.listener(|this, _: &ComposerNavPrev, _, cx| this.nav_popup(-1, cx)))
            .on_action(cx.listener(|this, _: &ComposerPopupClose, _, cx| {
                this.close_popup_key(cx)
            }))
            .child(
            div()
                .relative()
                .w_full()
                .max_w(px(860.))
                .mx_auto()
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .rounded_2xl()
                        // In dark mode the surface color separates (official
                        // style has no stroke); in light mode the background
                        // is white like the window, so a stroke is still
                        // needed for separation
                        .when(!dark, |this| {
                            this.border_1().border_color(cx.theme().border)
                        })
                        .bg(composer_surface),
                )
                .child(
                    v_flex()
                        .w_full()
                        .gap_2()
                        .px_3()
                        .py_2()
                        .when(self.hero_mode, |this| {
                            this.child(
                                h_flex()
                                    .w_full()
                                    .gap_2()
                                    .pb_1()
                                    .border_b_1()
                                    .border_color(cx.theme().border)
                                    .child(
                                        div()
                                            .relative()
                                            .child(
                                                h_flex()
                                                    .id("hero-cwd")
                                                    .gap_1()
                                                    .px_3()
                                                    .py_1()
                                                    .rounded_full()
                                                    .bg(cx.theme().accent.opacity(0.5))
                                                    .cursor_pointer()
                                                    .hover(|this| this.bg(cx.theme().accent))
                                                    .on_click(cx.listener(
                                                        move |this, event: &ClickEvent, window, cx| {
                                                            let command = this.cwd_command.clone();
                                                            this.toggle_popup(
                                                                Popup::Cwd,
                                                                event,
                                                                Some(command),
                                                                window,
                                                                cx,
                                                            );
                                                        },
                                                    ))
                                                    .child(
                                                        Icon::new(IconName::Folder)
                                                            .size_4()
                                                            .text_color(
                                                                cx.theme().muted_foreground,
                                                            ),
                                                    )
                                                    .child(
                                                        div()
                                                            .text_sm()
                                                            .when(self.hero_cwd.is_none(), |this| {
                                                                this.text_color(
                                                                    cx.theme().muted_foreground,
                                                                )
                                                            })
                                                            .child(self.hero_cwd_label.clone()),
                                                    )
                                                    .child(
                                                        Icon::new(IconName::ChevronDown)
                                                            .size_3()
                                                            .text_color(
                                                                cx.theme().muted_foreground,
                                                            ),
                                                    ),
                                            )
                                            .when_some(cwd_popup, |this, popup| this.child(popup)),
                                    )
                                    .when(self.hero_cwd.is_some(), |this| {
                                        this.child(
                                            div()
                                                .relative()
                                                .child(
                                                    h_flex()
                                                        .id("hero-branch")
                                                        .gap_1()
                                                        .px_3()
                                                        .py_1()
                                                        .rounded_full()
                                                        .when(self.hero_is_git, |this| {
                                                            this.bg(cx.theme().accent.opacity(0.5))
                                                                .cursor_pointer()
                                                                .hover(|this| {
                                                                    this.bg(cx.theme().accent)
                                                                })
                                                                .on_click(cx.listener(
                                                                    move |this, event: &ClickEvent, window, cx| {
                                                                        let command = this
                                                                            .branch_command
                                                                            .clone();
                                                                        this.toggle_popup(
                                                                            Popup::Branch,
                                                                            event,
                                                                            Some(command),
                                                                            window,
                                                                            cx,
                                                                        );
                                                                    },
                                                                ))
                                                        })
                                                        .child(
                                                            Icon::new(IconName::Github)
                                                                .size_4()
                                                                .text_color(
                                                                    cx.theme().muted_foreground,
                                                                ),
                                                        )
                                                        .child(
                                                            div()
                                                                .text_sm()
                                                                .when(!self.hero_is_git, |this| {
                                                                    this.text_color(
                                                                        cx.theme().muted_foreground,
                                                                    )
                                                                })
                                                                .child(if self.hero_is_git {
                                                                    self.hero_branch
                                                                        .clone()
                                                                        .unwrap_or_else(|| {
                                                                            "?".into()
                                                                        })
                                                                } else {
                                                                    rust_i18n::t!(
                                                                        "composer.not_git_repo"
                                                                    )
                                                                    .to_string()
                                                                }),
                                                        )
                                                        .when(self.hero_is_git, |this| {
                                                            this.child(
                                                                Icon::new(IconName::ChevronDown)
                                                                    .size_3()
                                                                    .text_color(
                                                                        cx.theme().muted_foreground,
                                                                    ),
                                                            )
                                                        }),
                                                )
                                                .when_some(branch_popup, |this, popup| {
                                                    this.child(popup)
                                                }),
                                        )
                                    }),
                            )
                        })
                        .when(
                            // hero (the new session page) belongs to no
                            // session: progress/task/changes chips are never
                            // shown
                            !self.hero_mode
                                && (!self.todos.is_empty()
                                    || !self.tasks.is_empty()
                                    || !self.change_files.is_empty()),
                            |this| this.child(self.render_aux(cx)),
                        )
                        .when_some(question.clone(), |this, question| {
                            this.child(self.render_question_bar(&question, cx))
                        })
                        .when(question.is_none() && approval.is_some(), |this| {
                            this.child(
                                self.render_approval_bar(approval.as_ref().expect("approval"), cx),
                            )
                        })
                        .when(!self.pasted_images.is_empty() || self.paste_note.is_some(), |this| {
                            this.child(self.render_pasted_images(composer_surface, cx))
                        })
                        .when(question.is_none() && approval.is_none(), |this| {
                            this.child(
                                div()
                                    .relative()
                                    .w_full()
                                    .child(
                                        Textarea::new(&self.input)
                                            .appearance(false)
                                            .bordered(false)
                                            // Token icons: command chips use
                                            // the terminal icon, @mentions the
                                            // file icon
                                            .token(|ctx, _, _| {
                                                if ctx.token().text().starts_with('/') {
                                                    InputToken::new(ctx).icon(IconName::SquareTerminal)
                                                } else {
                                                    InputToken::new(ctx).icon(IconName::FileText)
                                                }
                                            })
                                            .on_paste({
                                                let composer = cx.entity();
                                                move |item, window, cx| {
                                                    composer.update(cx, |this, cx| {
                                                        this.handle_paste(item, window, cx)
                                                    })
                                                }
                                            }),
                                    )
                                    .children(popup),
                            )
                        })
                        .when(question.is_none() && approval.is_none(), |this| {
                            this.child(
                            h_flex()
                                .w_full()
                                .gap_2()
                                .child(
                                    div()
                                        .relative()
                                        .child(self.render_bar_chip(
                                            "exec-mode",
                                            Some(exec_mode_icon(EXEC_MODES[self.exec_mode])),
                                            exec_mode_label(EXEC_MODES[self.exec_mode]).to_string(),
                                            exec_open,
                                            Some(exec_mode_color(EXEC_MODES[self.exec_mode], cx)),
                                            cx.listener(move |this, event: &ClickEvent, window, cx| {
                                                let command = this.exec_command.clone();
                                                this.toggle_popup(
                                                    Popup::ExecMode,
                                                    event,
                                                    Some(command),
                                                    window,
                                                    cx,
                                                );
                                                // Highlight follows the mouse
                                                // only: clear the default
                                                // keyboard selection block
                                                // (otherwise the first row
                                                // keeps a highlight-like
                                                // block, reading as "two
                                                // highlights" when hovering
                                                // the plan row); keyboard ↓
                                                // reselects, so behavior is
                                                // unchanged
                                                if matches!(this.popup, Some((Popup::ExecMode, _))) {
                                                    this.exec_command.update(cx, |state, cx| {
                                                        state.set_selected_index(None, window, cx);
                                                    });
                                                }
                                            }),
                                            cx,
                                        ))
                                        .when_some(exec_popup, |this, popup| this.child(popup)),
                                )
                                // Plan mode chip (orthogonal to the permission
                                // tier, same as ZCode composer's plan chip):
                                // lightbulb + "Plan" + X to close; rendered
                                // only when enabled
                                .when(self.plan_enabled, |this| {
                                    this.child(
                                        h_flex()
                                            .id("plan-chip")
                                            .test_support()
                                            .gap_1()
                                            .px_3()
                                            .py_1()
                                            .rounded_full()
                                            .items_center()
                                            .bg(cx.theme().accent.opacity(0.5))
                                            .child(
                                                Icon::new(AssetIconName::Lightbulb)
                                                    .size_4()
                                                    .text_color(cx.theme().info),
                                            )
                                            .child(
                                                div()
                                                    .text_sm()
                                                    .text_color(cx.theme().info)
                                                    .child(rust_i18n::t!("composer.plan")),
                                            )
                                            .child(
                                                div()
                                                    .id("plan-chip-close")
                                                    .test_support()
                                                    .cursor_pointer()
                                                    .rounded_full()
                                                    .hover(|this| this.bg(cx.theme().accent))
                                                    .child(
                                                        Icon::new(IconName::Close)
                                                            .size_3()
                                                            .text_color(
                                                                cx.theme().muted_foreground,
                                                            ),
                                                    )
                                                    .on_click(cx.listener(
                                                        |this, _, _, cx| {
                                                            this.plan_enabled = false;
                                                            cx.emit(ComposerEvent::SetPlanMode(
                                                                false,
                                                            ));
                                                            cx.notify();
                                                        },
                                                    )),
                                            ),
                                    )
                                })
                                .child(div().flex_1())
                                .when_some(self.context_usage, |this, (used, total, _, _)| {
                                    // Context usage ring indicator (same as
                                    // ZCode): hover to show the capacity
                                    // panel
                                    let ratio = (used as f32 / total as f32).clamp(0.0, 1.0);
                                    let ring_color = if ratio > 0.8 {
                                        cx.theme().warning
                                    } else {
                                        cx.theme().progress_bar
                                    };
                                    this.child(
                                        div()
                                            .relative()
                                            .child(
                                                h_flex()
                                                    .id("context-usage")
                                                    .px_2()
                                                    .py_1()
                                                    .rounded_full()
                                                    .when(context_open, |this| {
                                                        this.bg(cx.theme().accent)
                                                    })
                                                    .hover(|this| this.bg(cx.theme().accent))
                                                    .on_hover(cx.listener(
                                                        |this, hovered: &bool, _, cx| {
                                                            if *hovered {
                                                                this.popup =
                                                                    Some((Popup::Context, 0));
                                                            } else if matches!(
                                                                this.popup,
                                                                Some((Popup::Context, _))
                                                            ) {
                                                                this.popup = None;
                                                            }
                                                            cx.notify();
                                                        },
                                                    ))
                                                    .child(
                                                        ProgressCircle::new("context-usage-ring")
                                                            .value(ratio * 100.)
                                                            .color(ring_color)
                                                            .small(),
                                                    ),
                                            )
                                            .when_some(context_popup, |this, popup| {
                                                this.child(popup)
                                            }),
                                    )
                                })
                                .child(
                                    div()
                                        .relative()
                                        .child(self.render_bar_chip(
                                            "model-picker",
                                            None,
                                            self.model_display(),
                                            model_open,
                                            None,
                                            cx.listener(move |this, event: &ClickEvent, window, cx| {
                                                let command = this.model_command.clone();
                                                this.toggle_popup(
                                                    Popup::Model,
                                                    event,
                                                    Some(command),
                                                    window,
                                                    cx,
                                                );
                                            }),
                                            cx,
                                        ))
                                        .when_some(model_popup, |this, popup| this.child(popup)),
                                )
                                .when(!reasoning_levels.is_empty(), |this| {
                                    this.child(
                                        div()
                                            .relative()
                                            .child(
                                                self.render_bar_chip(
                                                    "reasoning-picker",
                                                    Some(AssetIconName::Brain),
                                                    reasoning_label
                                                        .clone()
                                                        .unwrap_or_else(|| {
                                                            rust_i18n::t!(
                                                                "composer.reasoning_off_chip"
                                                            )
                                                            .into()
                                                        }),
                                                    reasoning_open,
                                                    None,
                                                    cx.listener(move |this, event: &ClickEvent, window, cx| {
                                                        let command =
                                                            this.reasoning_command.clone();
                                                        this.toggle_popup(
                                                            Popup::Reasoning,
                                                            event,
                                                            Some(command),
                                                            window,
                                                            cx,
                                                        );
                                                    }),
                                                    cx,
                                                ),
                                            )
                                            .when_some(reasoning_popup, |this, popup| {
                                                this.child(popup)
                                            }),
                                    )
                                })
                                .when(self.streaming, |this| {
                                    this.child(
                                        Button::new("stop")
                                            .danger()
                                            .icon(AssetIconName::Square)
                                            .rounded(px(999.))
                                            .tooltip(rust_i18n::t!("composer.stop"))
                                            .on_click(cx.listener(|_, _: &ClickEvent, _, cx| {
                                                cx.emit(ComposerEvent::Stop);
                                            })),
                                    )
                                })
                                .when(!self.streaming, |this| {
                                    this.child(
                                        Button::new("send")
                                            .primary()
                                            .icon(AssetIconName::ArrowUp)
                                            .rounded(px(999.))
                                            .tooltip(rust_i18n::t!("composer.send"))
                                            .when(!can_send, |this| this.disabled(true))
                                            .on_click(cx.listener(
                                                |this, _: &ClickEvent, window, cx| {
                                                    this.send(window, cx);
                                                },
                                            )),
                                    )
                                }),
                            )
                        }),
                ),
        )
    }
}
