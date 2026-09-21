use gpui_kit::assets::IconName as AssetIconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::command::{Command, CommandGroup, CommandItem, CommandState};
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::progress::ProgressCircle;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _, h_flex,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use pig_protocol::{ApprovalDecision, ExecMode, TaskStatus, TaskSummary, TodoItem, TodoStatus};

const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/clear", "清空当前会话消息"),
    ("/compact", "压缩上下文（演示）"),
];

pub const PLACEHOLDER_IDLE: &str = "向 pig-code 提问，使用 @ 添加上下文，使用 / 选择命令";
pub const PLACEHOLDER_STREAMING: &str = "继续输入以排队后续修改";

/// (名称, 描述, 模式)
const EXEC_MODES: &[(&str, &str, ExecMode)] = &[
    (
        "变更前确认",
        "改文件前先问我。",
        ExecMode::ConfirmBeforeEdit,
    ),
    ("自动编辑", "自动编辑文件。", ExecMode::AutoEdit),
    ("计划模式", "编辑前先出计划。", ExecMode::Plan),
    ("完全访问", "减少确认次数。", ExecMode::FullAccess),
];

fn exec_mode_icon(mode: ExecMode) -> AssetIconName {
    match mode {
        ExecMode::ConfirmBeforeEdit => AssetIconName::Hand,
        ExecMode::AutoEdit => AssetIconName::ShieldCheck,
        ExecMode::Plan => AssetIconName::Lightbulb,
        ExecMode::FullAccess => AssetIconName::ShieldAlert,
    }
}

/// 任务耗时：started→ended（或至今），"N 秒 / N 分"。
fn format_task_duration(started_at: u64, end: u64) -> String {
    let secs = end.saturating_sub(started_at);
    if secs < 60 {
        format!("{secs} 秒")
    } else {
        format!("{} 分", secs / 60)
    }
}
/// (供应商名, provider_id, model_id, 推理等级列表)
pub type ModelOption = (String, String, String, Vec<String>);

/// 待审批的操作：审批期间输入框隐藏，显示审批条。
#[derive(Clone)]
pub struct PendingApproval {
    pub tool: String,
    /// Bash 是命令原文；Write/Edit 是 diff 预览
    pub detail: String,
    pub cwd: String,
}

/// 输入区上方的辅助面板（当前进度 / 后台 Bash 任务）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum AuxPanel {
    Todos,
    Tasks,
}

/// 任务面板过滤 tab。
#[derive(Clone, Copy, PartialEq, Eq)]
enum TaskFilter {
    Running,
    Finished,
    All,
}

impl TaskFilter {
    const TABS: &[(TaskFilter, &str)] = &[
        (TaskFilter::Running, "进行中"),
        (TaskFilter::Finished, "已完成"),
        (TaskFilter::All, "全部"),
    ];

    fn matches(self, status: TaskStatus) -> bool {
        match self {
            TaskFilter::Running => matches!(status, TaskStatus::Running),
            TaskFilter::Finished => !matches!(status, TaskStatus::Running),
            TaskFilter::All => true,
        }
    }
}

#[derive(Clone)]
pub enum ComposerEvent {
    Send {
        text: String,
        files: Vec<String>,
        mode: ExecMode,
    },
    Stop,
    Clear,
    Compact,
    SetModel {
        provider_id: String,
        model_id: String,
    },
    SetReasoning(Option<String>),
    OpenSettings,
    SetExecMode(ExecMode),
    SearchFiles(String),
    /// hero：打开系统目录选择器
    PickDirectory,
    /// hero：选择最近目录
    SelectCwd(String),
    /// hero：取消工作区选择（不在工作区中工作）
    ClearCwd,
    /// hero：切换 git 分支
    CheckoutBranch(String),
    /// 审批条：批准 / 本会话内批准 / 拒绝
    DecideApproval(ApprovalDecision),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Popup {
    Mention,
    Slash,
    ExecMode,
    Model,
    Reasoning,
    Cwd,
    Branch,
    Context,
}

impl EventEmitter<ComposerEvent> for Composer {}

/// 弹层相对触发芯片的水平锚点。
#[derive(Clone, Copy, PartialEq, Eq)]
enum PopupAnchor {
    Left,
    Right,
    /// 弹层水平中线对齐芯片中线（上下文容量面板用）。
    Center,
}

pub struct Composer {
    input: Entity<TextareaState>,
    attachments: Vec<&'static str>,
    exec_mode: usize,
    model: String,
    models: Vec<ModelOption>,
    reasoning_level: Option<String>,
    popup: Option<(Popup, usize)>,
    /// 最近一次被 on_mouse_down_out 关掉的弹层及按下位置：弹层打开时点击芯片，
    /// outside-close 先把它关掉，同一次按压的 click 紧跟着到达——按按下位置吞掉它，
    /// 避免「收起又马上弹开」。
    outside_closed: Option<(Popup, Point<Pixels>)>,
    streaming: bool,
    /// 待审批：Some 时输入区隐藏，显示审批条
    approval: Option<PendingApproval>,
    /// 审批条的焦点（承接 ⏎ / Ctrl+⏎ / Esc 快捷键）
    approval_focus: FocusHandle,
    /// 是否已为当前审批条抢过焦点（每次出现只抢一次）
    approval_focused: bool,
    mention_results: Vec<String>,
    context_usage: Option<(u64, u64)>,
    /// 输入区上方面板：TodoList 进度 / 后台 Bash 任务快照（core 推送）
    todos: Vec<TodoItem>,
    tasks: Vec<TaskSummary>,
    active_panel: Option<AuxPanel>,
    task_filter: TaskFilter,
    /// 展开输出尾部的任务行 id
    expanded_task: Option<String>,
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
    placeholder_applied: &'static str,
    _subscriptions: Vec<Subscription>,
}

impl Composer {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(PLACEHOLDER_IDLE)
                .auto_grow(2, 8)
                .submit_on_enter(true)
        });

        let _subscriptions = vec![cx.subscribe_in(
            &input,
            window,
            |this: &mut Self, input, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { shift, .. } if !shift => this.send(window, cx),
                InputEvent::Change => this.update_suggestion(input, cx),
                _ => {}
            },
        )];

        Self {
            input,
            attachments: Vec::new(),
            exec_mode: 1,
            model: "未配置模型".to_string(),
            models: vec![],
            reasoning_level: None,
            popup: None,
            outside_closed: None,
            streaming: false,
            approval: None,
            approval_focus: cx.focus_handle(),
            approval_focused: false,
            mention_results: Vec::new(),
            context_usage: None,
            todos: Vec::new(),
            tasks: Vec::new(),
            active_panel: None,
            task_filter: TaskFilter::Running,
            expanded_task: None,
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
            placeholder_applied: PLACEHOLDER_IDLE,
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

    /// 建议芯片：填入引导文本（不发送）。
    pub fn fill_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| {
            input.set_value(text, window, cx);
            input.focus(window, cx);
        });
    }

    pub fn set_model_name(&mut self, model: String, cx: &mut Context<Self>) {
        self.model = model;
        cx.notify();
    }

    pub fn set_models(&mut self, models: Vec<ModelOption>, cx: &mut Context<Self>) {
        self.models = models;
        cx.notify();
    }

    /// 自测用。
    pub fn debug_model_count(&self) -> usize {
        self.models.len()
    }

    /// 自测用。
    #[allow(dead_code)]
    pub fn debug_reasoning_level(&self) -> Option<String> {
        self.reasoning_level.clone()
    }

    /// 待审批操作：Some 时输入区隐藏，显示审批条；None 恢复输入。
    pub fn set_approval(&mut self, approval: Option<PendingApproval>, cx: &mut Context<Self>) {
        self.approval = approval;
        cx.notify();
    }

    /// 审批条决议：清空审批态、发事件、焦点还回输入框。
    fn decide_approval(
        &mut self,
        decision: ApprovalDecision,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.approval.take().is_some() {
            cx.emit(ComposerEvent::DecideApproval(decision));
            self.input.update(cx, |input, cx| input.focus(window, cx));
            cx.notify();
        }
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        let files: Vec<String> = text
            .split_whitespace()
            .filter_map(|word| word.strip_prefix('@'))
            .filter(|path| !path.is_empty())
            .map(str::to_string)
            .collect();
        self.input.update(cx, |state, cx| {
            state.set_value("", window, cx);
        });
        self.popup = None;
        cx.emit(ComposerEvent::Send {
            text,
            files,
            mode: EXEC_MODES[self.exec_mode].2,
        });
        cx.notify();
    }

    /// 从光标前的文本检测 @ 或 / 触发符，返回触发位置和查询串。
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
        if let Some((Popup::Mention, start)) = self.popup {
            let query = value[start + 1..caret].to_string();
            cx.emit(ComposerEvent::SearchFiles(query));
        }
        cx.notify();
    }

    pub fn set_mention_results(&mut self, results: Vec<String>, cx: &mut Context<Self>) {
        self.mention_results = results;
        cx.notify();
    }

    /// 自测用。
    pub fn debug_mention_results(&self) -> &[String] {
        &self.mention_results
    }

    pub fn set_context_usage(&mut self, used: u64, total: u64, cx: &mut Context<Self>) {
        self.context_usage = Some((used, total));
        cx.notify();
    }

    pub fn set_todos(&mut self, todos: Vec<TodoItem>, cx: &mut Context<Self>) {
        self.todos = todos;
        cx.notify();
    }

    pub fn set_tasks(&mut self, tasks: Vec<TaskSummary>, cx: &mut Context<Self>) {
        self.tasks = tasks;
        cx.notify();
    }

    /// 自测用。
    pub fn debug_context_usage(&self) -> Option<(u64, u64)> {
        self.context_usage
    }

    pub fn set_exec_mode(&mut self, mode: ExecMode, cx: &mut Context<Self>) {
        if let Some(ix) = EXEC_MODES.iter().position(|(_, _, m)| *m == mode) {
            self.exec_mode = ix;
        }
        cx.notify();
    }

    /// 自测用。
    pub fn debug_exec_mode(&self) -> ExecMode {
        EXEC_MODES[self.exec_mode].2
    }

    /// 紧凑 token 数：1 万以下原样，以上用「万」（10.5万 / 100万）
    fn format_tokens_compact(n: u64) -> String {
        if n >= 10_000 {
            let wan = n as f64 / 10_000.0;
            if wan.fract().abs() < 0.05 {
                format!("{}万", wan.round() as u64)
            } else {
                format!("{wan:.1}万")
            }
        } else {
            n.to_string()
        }
    }

    /// 上下文容量面板：标题 + 用量/占比 + 进度条，居中锚定在指示器芯片正上方（悬停展示）。
    fn render_context_popup(&self, cx: &mut Context<Self>) -> AnyElement {
        let (used, total) = self.context_usage.unwrap_or((0, 1));
        let ratio = (used as f32 / total as f32).clamp(0.0, 1.0);
        let bar_color = if ratio > 0.8 {
            cx.theme().warning
        } else {
            cx.theme().progress_bar
        };

        let content = v_flex()
            .w_full()
            .gap_2()
            .rounded(cx.theme().radius_lg)
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().popover)
            .p_3()
            .child(
                h_flex()
                    .w_full()
                    .child(div().text_sm().font_medium().child("上下文容量"))
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!(
                                "{}/{} ({:.1}%)",
                                Self::format_tokens_compact(used),
                                Self::format_tokens_compact(total),
                                ratio * 100.0
                            )),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .h(px(6.))
                    .rounded_full()
                    .bg(cx.theme().muted_foreground.opacity(0.15))
                    .when(used > 0, |this| {
                        this.child(
                            div()
                                .h_full()
                                .w(relative(ratio))
                                // 极小占比也保留可见的一截（0.1% 仅 0.3px，会被圆整掉）
                                .min_w(px(3.))
                                .rounded_full()
                                .bg(bar_color),
                        )
                    }),
            )
            .into_any_element();
        self.popup_shell(
            "composer-context-popup",
            content,
            PopupAnchor::Center,
            None,
            cx,
        )
    }

    fn popup_query(&self, cx: &App) -> Option<(Popup, usize, String)> {
        let (kind, start) = self.popup?;
        match kind {
            Popup::Mention | Popup::Slash => {
                let value = self.input.read(cx).value();
                let caret = self.input.read(cx).selected_range().start.min(value.len());
                if caret < start + 1 {
                    return None;
                }
                Some((kind, start, value[start + 1..caret].to_string()))
            }
            _ => Some((kind, start, String::new())),
        }
    }

    fn insert_file(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some((Popup::Mention, start)) = self.popup else {
            return;
        };
        let caret = self.input.read(cx).selected_range().start;
        self.input.update(cx, |input, cx| {
            input.set_selected_range(start..caret, cx);
            input.replace(format!("@{path} "), window, cx);
            input.focus(window, cx);
        });
        self.popup = None;
        cx.notify();
    }

    fn run_command(&mut self, command: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((Popup::Slash, start)) = self.popup {
            let caret = self.input.read(cx).selected_range().start;
            self.input.update(cx, |input, cx| {
                input.set_selected_range(start..caret, cx);
                input.replace("", window, cx);
                input.focus(window, cx);
            });
        }
        self.popup = None;
        match command {
            "/clear" => cx.emit(ComposerEvent::Clear),
            "/compact" => cx.emit(ComposerEvent::Compact),
            _ => {}
        }
        cx.notify();
    }

    fn render_list_item(
        &self,
        id: impl Into<ElementId>,
        icon: IconName,
        label: String,
        detail: Option<String>,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        h_flex()
            .id(id)
            .w_full()
            .gap_2()
            .px_3()
            .py_1()
            .cursor_pointer()
            .hover(|this| this.bg(cx.theme().accent))
            .on_click(on_click)
            .child(
                Icon::new(icon)
                    .size_4()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(div().text_sm().child(label))
            .when_some(detail, |this, detail| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(detail),
                )
            })
            .into_any_element()
    }

    fn render_popup(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (kind, _start, query) = self.popup_query(cx)?;
        let items: Vec<AnyElement> = match kind {
            Popup::Mention => {
                let _ = query;
                self.mention_results
                    .iter()
                    .enumerate()
                    .map(|(ix, path)| {
                        let path = path.clone();
                        self.render_list_item(
                            ("mention", ix),
                            IconName::FileText,
                            path.clone(),
                            None,
                            cx.listener(move |this, _, window, cx| {
                                this.insert_file(path.clone(), window, cx);
                            }),
                            cx,
                        )
                    })
                    .collect()
            }
            Popup::Slash => SLASH_COMMANDS
                .iter()
                .filter(|(name, _)| name[1..].contains(query.as_str()))
                .enumerate()
                .map(|(ix, (name, desc))| {
                    self.render_list_item(
                        ("slash", ix),
                        IconName::SquareTerminal,
                        name.to_string(),
                        Some(desc.to_string()),
                        cx.listener(move |this, _, window, cx| {
                            this.run_command(name, window, cx);
                        }),
                        cx,
                    )
                })
                .collect(),
            Popup::ExecMode
            | Popup::Cwd
            | Popup::Branch
            | Popup::Model
            | Popup::Reasoning
            | Popup::Context => {
                unreachable!("Cwd/Branch/ExecMode/Model/Reasoning/Context 由各自的专用面板渲染")
            }
        };
        if items.is_empty() {
            return None;
        }

        let tag: &'static str = match kind {
            Popup::Mention => "mention",
            Popup::Slash => "slash",
            Popup::ExecMode => "exec",
            Popup::Model => "model",
            Popup::Reasoning => "reasoning",
            Popup::Cwd => "cwd",
            Popup::Branch => "branch",
            Popup::Context => "context",
        };
        Some(
            div()
                .id("composer-popup")
                .absolute()
                .bottom_full()
                .left_0()
                .mb_1()
                .w(px(320.))
                .max_h(px(240.))
                .overflow_y_scroll()
                .rounded(cx.theme().radius)
                .border_1()
                .border_color(cx.theme().border)
                .bg(cx.theme().popover)
                .py_1()
                .child(
                    div()
                        .relative()
                        .with_animation(
                            format!("popup-enter-{tag}"),
                            Animation::new(std::time::Duration::from_millis(150))
                                .with_easing(ease_out_quint()),
                            |el, delta| el.top(px(6.0 * (1.0 - delta))).opacity(delta),
                        )
                        .children(items),
                )
                .into_any_element(),
        )
    }

    /// 弹层外壳：锚定在触发芯片正上方，点击外部关闭，带进入动画。
    /// `anchor` 为 Center 时弹层水平中线对齐芯片中线（定宽 360）；
    /// 其余弹层宽度按内容伸缩（160 ~ 360）。
    /// `hover_clear`：Command 面板是「悬停即选中」，鼠标移出面板后选中行的高亮
    /// 会残留，传对应 CommandState 时在移出面板时清掉选中。
    fn popup_shell(
        &self,
        id: &'static str,
        content: AnyElement,
        anchor: PopupAnchor,
        hover_clear: Option<Entity<CommandState>>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id(id)
            .absolute()
            .bottom_full()
            .mb_2()
            .map(|this| match anchor {
                PopupAnchor::Left => this.left_0(),
                PopupAnchor::Right => this.right_0(),
                // 外层拉伸到芯片宽度，再由 flex 把固定宽的面板居中到芯片中线
                PopupAnchor::Center => this.left_0().right_0().flex().flex_row().justify_center(),
            })
            .on_mouse_down_out(cx.listener(|this, event: &MouseDownEvent, _, cx| {
                if let Some((kind, _)) = this.popup {
                    this.outside_closed = Some((kind, event.position));
                }
                this.popup = None;
                cx.notify();
            }))
            .when_some(hover_clear, |this, command| {
                this.on_hover(cx.listener(move |_, hovered: &bool, window, cx| {
                    if !*hovered {
                        command.update(cx, |state, cx| {
                            state.set_selected_index(None, window, cx);
                        });
                    }
                }))
            })
            .child(
                div()
                    .relative()
                    .map(|this| match anchor {
                        // 上下文容量面板内容固定（标题行 + 进度条），定宽居中
                        PopupAnchor::Center => this.w(px(360.)),
                        // 其余面板按内容伸缩：思考等级这类短列表不用撑满 360
                        _ => this.min_w(px(160.)).max_w(px(360.)),
                    })
                    .with_animation(
                        format!("{id}-enter"),
                        Animation::new(std::time::Duration::from_millis(150))
                            .with_easing(ease_out_quint()),
                        |el, delta| el.top(px(6.0 * (1.0 - delta))).opacity(delta),
                    )
                    .child(content),
            )
            .into_any_element()
    }

    /// Command 弹层外壳：锚定在触发芯片正上方，点击外部关闭，带进入动画。
    fn command_popup_shell(
        &self,
        id: &'static str,
        state: &Entity<CommandState>,
        command: Command,
        anchor: PopupAnchor,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.popup_shell(
            id,
            command.into_any_element(),
            anchor,
            Some(state.clone()),
            cx,
        )
    }

    /// 面板确认/取消的通用收尾：关闭弹层并回焦输入框。
    fn close_command_popup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.popup = None;
        self.input.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    /// 芯片点击开合弹层：弹层打开时点击芯片会先触发弹层的 on_mouse_down_out 把它
    /// 关掉（中间隔着一次重渲染，渲染时捕获的开合状态不可靠），这里按「同一次按压
    /// 的按下位置」吞掉紧随其后的 click，避免收起又马上弹开。
    fn toggle_popup(
        &mut self,
        kind: Popup,
        click: &ClickEvent,
        command: Option<Entity<CommandState>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let down_pos = match click {
            ClickEvent::Mouse(event) => Some(event.down.position),
            _ => None,
        };
        if let Some((closed, pos)) = self.outside_closed.take()
            && closed == kind
            && Some(pos) == down_pos
        {
            return;
        }
        if matches!(self.popup, Some((k, _)) if k == kind) {
            self.close_command_popup(window, cx);
        } else {
            self.popup = Some((kind, 0));
            if let Some(command) = command {
                command.update(cx, |state, cx| {
                    state.set_query("", window, cx);
                    state.focus(window, cx);
                });
            }
            cx.notify();
        }
    }

    /// 底部工具栏芯片：图标 + 文本 + 下拉箭头，样式与 hero 区工作区/分支芯片一致。
    fn render_bar_chip(
        &self,
        id: &'static str,
        icon: Option<AssetIconName>,
        label: String,
        filled: bool,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        h_flex()
            .id(id)
            .gap_1()
            .px_3()
            .py_1()
            .rounded_full()
            .cursor_pointer()
            .when(filled, |this| this.bg(cx.theme().accent.opacity(0.5)))
            .hover(|this| this.bg(cx.theme().accent))
            .on_click(on_click)
            .when_some(icon, |this, icon| {
                this.child(
                    Icon::new(icon)
                        .size_4()
                        .text_color(cx.theme().muted_foreground),
                )
            })
            .child(div().text_sm().child(label))
            .child(
                Icon::new(IconName::ChevronDown)
                    .size_3()
                    .text_color(cx.theme().muted_foreground),
            )
    }

    /// 工作区选择面板：Command 面板（搜索框 + 工作区列表 + 操作行），锚定在工作区芯片正上方。
    fn render_cwd_popup(&self, cx: &mut Context<Self>) -> AnyElement {
        let on_confirm_composer = cx.entity();
        let on_cancel_composer = cx.entity();

        let command = Command::new(&self.cwd_command)
            .placeholder("搜索工作区")
            .items(self.hero_cwds.iter().map(|cwd| {
                let name = std::path::Path::new(cwd)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| cwd.clone());
                CommandItem::new()
                    .label(name)
                    .keywords([cwd.clone()])
                    .icon(IconName::Folder)
                    .checked(self.hero_cwd.as_deref() == Some(cwd.as_str()))
            }))
            .separator()
            .item(
                CommandItem::new()
                    .label("打开文件夹")
                    .icon(IconName::FolderOpen),
            )
            .when(self.hero_cwd.is_some(), |this| {
                this.item(
                    CommandItem::new()
                        .label("不在工作区中工作")
                        .icon(IconName::CircleX),
                )
            })
            .empty(|_, _, cx| {
                div()
                    .px_2()
                    .py_1p5()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("没有匹配的工作区")
            })
            .on_confirm(move |ix, window, cx| {
                on_confirm_composer.update(cx, |this, cx| {
                    let len = this.hero_cwds.len();
                    if let Some(cwd) = this.hero_cwds.get(ix.row) {
                        cx.emit(ComposerEvent::SelectCwd(cwd.clone()));
                    } else if ix.row == len {
                        cx.emit(ComposerEvent::PickDirectory);
                    } else {
                        cx.emit(ComposerEvent::ClearCwd);
                    }
                    this.close_command_popup(window, cx);
                });
            })
            .on_cancel(move |window, cx| {
                on_cancel_composer.update(cx, |this, cx| {
                    this.close_command_popup(window, cx);
                });
            });
        self.command_popup_shell(
            "composer-cwd-popup",
            &self.cwd_command,
            command,
            PopupAnchor::Left,
            cx,
        )
    }

    /// 分支选择面板：Command 面板（搜索框 + 分支列表），锚定在分支芯片正上方。
    fn render_branch_popup(&self, cx: &mut Context<Self>) -> AnyElement {
        let on_confirm_composer = cx.entity();
        let on_cancel_composer = cx.entity();

        let command = Command::new(&self.branch_command)
            .placeholder("搜索分支")
            .items(self.hero_branches.iter().map(|branch| {
                CommandItem::new()
                    .label(branch.clone())
                    .keywords([branch.clone()])
                    .icon(IconName::Github)
                    .checked(self.hero_branch.as_deref() == Some(branch.as_str()))
            }))
            .empty(|_, _, cx| {
                div()
                    .px_2()
                    .py_1p5()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("没有匹配的分支")
            })
            .on_confirm(move |ix, window, cx| {
                on_confirm_composer.update(cx, |this, cx| {
                    if let Some(branch) = this.hero_branches.get(ix.row) {
                        cx.emit(ComposerEvent::CheckoutBranch(branch.clone()));
                    }
                    this.close_command_popup(window, cx);
                });
            })
            .on_cancel(move |window, cx| {
                on_cancel_composer.update(cx, |this, cx| {
                    this.close_command_popup(window, cx);
                });
            });
        self.command_popup_shell(
            "composer-branch-popup",
            &self.branch_command,
            command,
            PopupAnchor::Left,
            cx,
        )
    }

    /// 执行模式面板：无搜索框，每项带图标 + 描述，当前模式勾选。
    fn render_exec_mode_popup(&self, cx: &mut Context<Self>) -> AnyElement {
        let on_confirm_composer = cx.entity();
        let on_cancel_composer = cx.entity();

        let command = Command::new(&self.exec_command)
            .searchable(false)
            .items(
                EXEC_MODES
                    .iter()
                    .enumerate()
                    .map(|(ix, (label, desc, mode))| {
                        let icon = exec_mode_icon(*mode);
                        CommandItem::new()
                            .label(*label)
                            .checked(ix == self.exec_mode)
                            .child(move |_, cx| {
                                h_flex()
                                    .flex_1()
                                    .gap_2()
                                    .items_center()
                                    .child(
                                        Icon::new(icon)
                                            .size_4()
                                            .text_color(cx.theme().muted_foreground),
                                    )
                                    .child(
                                        v_flex().child(div().child(*label)).child(
                                            div()
                                                .text_xs()
                                                .text_color(cx.theme().muted_foreground)
                                                .child(*desc),
                                        ),
                                    )
                            })
                    }),
            )
            .on_confirm(move |ix, window, cx| {
                on_confirm_composer.update(cx, |this, cx| {
                    if let Some((_, _, mode)) = EXEC_MODES.get(ix.row) {
                        this.exec_mode = ix.row;
                        cx.emit(ComposerEvent::SetExecMode(*mode));
                    }
                    this.close_command_popup(window, cx);
                });
            })
            .on_cancel(move |window, cx| {
                on_cancel_composer.update(cx, |this, cx| {
                    this.close_command_popup(window, cx);
                });
            });
        self.command_popup_shell(
            "composer-exec-popup",
            &self.exec_command,
            command,
            PopupAnchor::Left,
            cx,
        )
    }

    /// 模型面板：搜索框 + 按供应商分组的模型列表 + 「管理模型」操作行。
    fn render_model_popup(&self, cx: &mut Context<Self>) -> AnyElement {
        let on_confirm_composer = cx.entity();
        let on_cancel_composer = cx.entity();

        // 按供应商分组，保持配置中的出现顺序
        let mut groups: Vec<(String, Vec<ModelOption>)> = Vec::new();
        for model in &self.models {
            match groups.iter_mut().find(|(name, _)| *name == model.0) {
                Some((_, items)) => items.push(model.clone()),
                None => groups.push((model.0.clone(), vec![model.clone()])),
            }
        }

        let mut command = Command::new(&self.model_command).placeholder("搜索模型");
        if groups.is_empty() {
            command = command.item(
                CommandItem::new()
                    .label("还没有配置模型，去设置页添加")
                    .icon(IconName::Info),
            );
        } else {
            for (provider_name, items) in &groups {
                command = command.group(CommandGroup::new().label(provider_name.clone()).items(
                    items.iter().map(|(provider_name, _, model_id, _)| {
                        CommandItem::new()
                            .label(model_id.clone())
                            .keywords([provider_name.clone()])
                            .checked(self.model == format!("{provider_name}/{model_id}"))
                    }),
                ));
            }
        }
        // 未分组项固定为 section 0，分组从 section 1 起（与 entries 书写顺序无关）
        command = command.separator().item(
            CommandItem::new()
                .label("管理模型")
                .icon(AssetIconName::Settings),
        );
        let command = command
            .empty(|_, _, cx| {
                div()
                    .px_2()
                    .py_1p5()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("没有匹配的模型")
            })
            .on_confirm(move |ix, window, cx| {
                on_confirm_composer.update(cx, |this, cx| {
                    if ix.section == 0 {
                        cx.emit(ComposerEvent::OpenSettings);
                    } else if let Some((_, items)) = groups.get(ix.section - 1)
                        && let Some((provider_name, provider_id, model_id, _)) = items.get(ix.row)
                    {
                        this.model = format!("{provider_name}/{model_id}");
                        cx.emit(ComposerEvent::SetModel {
                            provider_id: provider_id.clone(),
                            model_id: model_id.clone(),
                        });
                    }
                    this.close_command_popup(window, cx);
                });
            })
            .on_cancel(move |window, cx| {
                on_cancel_composer.update(cx, |this, cx| {
                    this.close_command_popup(window, cx);
                });
            });
        self.command_popup_shell(
            "composer-model-popup",
            &self.model_command,
            command,
            PopupAnchor::Right,
            cx,
        )
    }

    /// 思考等级面板：无搜索框，「关闭」+ 等级列表，当前等级勾选。
    fn render_reasoning_popup(&self, levels: &[String], cx: &mut Context<Self>) -> AnyElement {
        let on_confirm_composer = cx.entity();
        let on_cancel_composer = cx.entity();
        let levels = levels.to_vec();

        let command = Command::new(&self.reasoning_command)
            .searchable(false)
            .item(
                CommandItem::new()
                    .label("关闭")
                    .checked(self.reasoning_level.is_none()),
            )
            .items(levels.iter().map(|level| {
                CommandItem::new()
                    .label(level.clone())
                    .checked(self.reasoning_level.as_deref() == Some(level.as_str()))
            }))
            .on_confirm(move |ix, window, cx| {
                on_confirm_composer.update(cx, |this, cx| {
                    let level = if ix.row == 0 {
                        None
                    } else {
                        levels.get(ix.row - 1).cloned()
                    };
                    this.reasoning_level = level.clone();
                    cx.emit(ComposerEvent::SetReasoning(level));
                    this.close_command_popup(window, cx);
                });
            })
            .on_cancel(move |window, cx| {
                on_cancel_composer.update(cx, |this, cx| {
                    this.close_command_popup(window, cx);
                });
            });
        self.command_popup_shell(
            "composer-reasoning-popup",
            &self.reasoning_command,
            command,
            PopupAnchor::Right,
            cx,
        )
    }

    fn render_attachments(&self, cx: &mut Context<Self>) -> AnyElement {
        h_flex()
            .gap_2()
            .children(self.attachments.iter().enumerate().map(|(ix, name)| {
                h_flex()
                    .gap_1()
                    .pl_2()
                    .pr_1()
                    .py_0p5()
                    .rounded(cx.theme().radius)
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().accent)
                    .child(
                        Icon::new(IconName::FileText)
                            .size_3()
                            .text_color(cx.theme().muted_foreground),
                    )
                    .child(div().text_xs().child(*name))
                    .child(
                        div()
                            .id(("remove-attachment", ix))
                            .cursor_pointer()
                            .rounded_sm()
                            .hover(|this| this.bg(cx.theme().danger.opacity(0.3)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.attachments.remove(ix);
                                cx.notify();
                            }))
                            .child(Icon::new(IconName::Close).size_3()),
                    )
            }))
            .into_any_element()
    }

    /// 当前进度（TodoList）+ 后台 Bash 任务：chip 行 + 可切换的只读面板（v1 无停止按钮）。
    fn render_aux(&self, cx: &mut Context<Self>) -> AnyElement {
        let running = self
            .tasks
            .iter()
            .filter(|t| matches!(t.status, TaskStatus::Running))
            .count();
        let done = self
            .todos
            .iter()
            .filter(|t| t.status == TodoStatus::Done)
            .count();

        let mut chips = h_flex().w_full().gap_2();
        if !self.tasks.is_empty() {
            let label = if running > 0 {
                format!("后台 Bash {running} 运行中")
            } else {
                "后台 Bash".to_string()
            };
            chips = chips.child(self.render_aux_chip(
                "aux-tasks",
                AssetIconName::Terminal,
                label,
                self.active_panel == Some(AuxPanel::Tasks),
                AuxPanel::Tasks,
                cx,
            ));
        }
        if !self.todos.is_empty() {
            chips = chips.child(self.render_aux_chip(
                "aux-todos",
                AssetIconName::ListTodo,
                format!("当前进度 {done}/{}", self.todos.len()),
                self.active_panel == Some(AuxPanel::Todos),
                AuxPanel::Todos,
                cx,
            ));
        }

        let mut root = v_flex().w_full().gap_2().child(chips);
        match self.active_panel {
            Some(AuxPanel::Todos) if !self.todos.is_empty() => {
                root = root.child(self.render_todos_panel(cx))
            }
            Some(AuxPanel::Tasks) if !self.tasks.is_empty() => {
                root = root.child(self.render_tasks_panel(cx))
            }
            _ => {}
        }
        root.into_any_element()
    }

    fn render_aux_chip(
        &self,
        id: &'static str,
        icon: AssetIconName,
        label: String,
        active: bool,
        panel: AuxPanel,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        h_flex()
            .id(id)
            .gap_1()
            .px_3()
            .py_1()
            .rounded_full()
            .cursor_pointer()
            .when(active, |this| this.bg(cx.theme().accent.opacity(0.5)))
            .hover(|this| this.bg(cx.theme().accent))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                // 再点同一个 chip 收起面板
                this.active_panel = if this.active_panel == Some(panel) {
                    None
                } else {
                    Some(panel)
                };
                cx.notify();
            }))
            .child(
                Icon::new(icon)
                    .size_4()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(div().text_sm().child(label))
    }

    /// 面板外壳：与现有弹层观感一致（rounded_xl + popover 背景 + 边框）。
    fn aux_panel_shell(&self, content: Div, cx: &mut Context<Self>) -> Div {
        content
            .w_full()
            .gap_2()
            .p_3()
            .rounded_xl()
            .bg(cx.theme().popover)
            .border_1()
            .border_color(cx.theme().border)
    }

    fn render_todos_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let done = self
            .todos
            .iter()
            .filter(|t| t.status == TodoStatus::Done)
            .count();
        let mut list = v_flex().w_full().gap_1();
        for item in &self.todos {
            let (icon, color) = match item.status {
                TodoStatus::Done => (AssetIconName::CircleCheck, cx.theme().success),
                TodoStatus::InProgress => (AssetIconName::LoaderCircle, cx.theme().progress_bar),
                TodoStatus::Pending => (AssetIconName::Circle, cx.theme().muted_foreground),
            };
            list = list.child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(Icon::new(icon).size_4().text_color(color))
                    .child(
                        div()
                            .text_sm()
                            .when(item.status == TodoStatus::Done, |this| {
                                this.text_color(cx.theme().muted_foreground)
                            })
                            .child(item.content.clone()),
                    ),
            );
        }
        self.aux_panel_shell(
            v_flex()
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!("当前进度 {done}/{}", self.todos.len())),
                )
                .child(list),
            cx,
        )
        .into_any_element()
    }

    fn render_tasks_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let running = self
            .tasks
            .iter()
            .filter(|t| matches!(t.status, TaskStatus::Running))
            .count();
        let title = if running > 0 {
            format!("后台 Bash {running} 运行中")
        } else {
            "后台 Bash".to_string()
        };

        // 过滤 tab：进行中 / 已完成 / 全部
        let mut tabs = h_flex().gap_1();
        for (ix, (filter, label)) in TaskFilter::TABS.iter().enumerate() {
            let active = self.task_filter == *filter;
            let filter = *filter;
            tabs = tabs.child(
                div()
                    .id(("aux-task-filter", ix))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .text_xs()
                    .when(active, |this| this.bg(cx.theme().accent))
                    .when(!active, |this| this.text_color(cx.theme().muted_foreground))
                    .hover(|this| this.bg(cx.theme().accent))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.task_filter = filter;
                        cx.notify();
                    }))
                    .child(*label),
            );
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let visible: Vec<&TaskSummary> = self
            .tasks
            .iter()
            .filter(|t| self.task_filter.matches(t.status))
            .collect();
        let mut list = v_flex().w_full().gap_1();
        if visible.is_empty() {
            list = list.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("无任务"),
            );
        }
        for (ix, task) in visible.iter().enumerate() {
            let (icon, color) = match task.status {
                TaskStatus::Running => (AssetIconName::LoaderCircle, cx.theme().progress_bar),
                TaskStatus::Exited(0) => (AssetIconName::CircleCheck, cx.theme().success),
                TaskStatus::Exited(_) => (AssetIconName::TriangleAlert, cx.theme().warning),
                TaskStatus::Killed => (AssetIconName::TriangleAlert, cx.theme().muted_foreground),
            };
            let duration = format_task_duration(task.started_at, task.ended_at.unwrap_or(now));
            let expanded = self.expanded_task.as_deref() == Some(task.id.as_str());
            let task_id = task.id.clone();
            let mut row = v_flex().w_full().child(
                h_flex()
                    .id(("aux-task-row", ix))
                    .w_full()
                    .gap_2()
                    .px_1()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|this| this.bg(cx.theme().accent.opacity(0.5)))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.expanded_task = if this.expanded_task.as_deref() == Some(task_id.as_str()) {
                            None
                        } else {
                            Some(task_id.clone())
                        };
                        cx.notify();
                    }))
                    .child(Icon::new(icon).size_4().text_color(color))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_x_hidden()
                            .whitespace_nowrap()
                            .text_sm()
                            .font_family("monospace")
                            .child(task.command.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(duration),
                    )
                    .child(
                        Icon::new(AssetIconName::ChevronRight)
                            .size_3()
                            .text_color(cx.theme().muted_foreground),
                    ),
            );
            if expanded {
                row = row.child(
                    div()
                        .id(("aux-task-output", ix))
                        .w_full()
                        .mt_1()
                        .p_2()
                        .rounded_md()
                        .bg(cx.theme().accent.opacity(0.3))
                        .max_h(px(240.))
                        .overflow_y_scroll()
                        .text_xs()
                        .font_family("monospace")
                        .text_color(cx.theme().muted_foreground)
                        .child(if task.output_tail.is_empty() {
                            "（暂无输出）".to_string()
                        } else {
                            task.output_tail.clone()
                        }),
                );
            }
            list = list.child(row);
        }

        self.aux_panel_shell(
            v_flex()
                .child(
                    h_flex()
                        .w_full()
                        .child(div().text_sm().child(title))
                        .child(div().flex_1())
                        .child(tabs),
                )
                .child(list),
            cx,
        )
        .into_any_element()
    }

    /// 审批条（kimi 同款）：审批期间替换输入区。橙色圆点 + 标题，深色内嵌块
    /// 展示命令/diff，底部 本会话内批准(Ctrl+⏎) / 拒绝(Esc) / 批准(⏎)。
    fn render_approval_bar(
        &self,
        approval: &PendingApproval,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let title = match approval.tool.as_str() {
            "Bash" => "运行命令？".to_string(),
            "Write" => "写入文件？".to_string(),
            "Edit" => "修改文件？".to_string(),
            tool => format!("执行 {tool}？"),
        };
        let detail = if approval.tool == "Bash" {
            format!("$ {}", approval.detail)
        } else {
            approval.detail.clone()
        };
        let cwd = approval.cwd.clone();

        v_flex()
            .id("approval-bar")
            .w_full()
            .gap_3()
            .p_2()
            .track_focus(&self.approval_focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let decision = match event.keystroke.key.as_str() {
                    "enter" if event.keystroke.modifiers.control => {
                        Some(ApprovalDecision::AlwaysAllow)
                    }
                    "enter" => Some(ApprovalDecision::Allow),
                    "escape" => Some(ApprovalDecision::Reject),
                    _ => None,
                };
                if let Some(decision) = decision {
                    this.decide_approval(decision, window, cx);
                }
            }))
            .child(
                h_flex()
                    .gap_2()
                    .child(div().size(px(8.)).rounded_full().bg(cx.theme().warning))
                    .child(div().text_sm().font_medium().child(title)),
            )
            .child(
                div()
                    .id("approval-detail")
                    .w_full()
                    .rounded(px(10.))
                    .bg(cx.theme().background)
                    .p_3()
                    .max_h(px(200.))
                    .overflow_y_scroll()
                    .text_sm()
                    .font_family("monospace")
                    .child(detail),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("工作目录： {cwd}")),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(
                        Button::new("approval-always")
                            .secondary()
                            .label("本会话内批准  Ctrl+⏎")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.decide_approval(ApprovalDecision::AlwaysAllow, window, cx);
                            })),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("approval-reject")
                            .secondary()
                            .label("拒绝  Esc")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.decide_approval(ApprovalDecision::Reject, window, cx);
                            })),
                    )
                    .child(
                        Button::new("approval-allow")
                            .primary()
                            .label("批准  ⏎")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.decide_approval(ApprovalDecision::Allow, window, cx);
                            })),
                    ),
            )
            .into_any_element()
    }
}

impl Render for Composer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let desired_placeholder = if self.streaming {
            PLACEHOLDER_STREAMING
        } else {
            PLACEHOLDER_IDLE
        };
        if self.placeholder_applied != desired_placeholder {
            self.placeholder_applied = desired_placeholder;
            self.input.update(cx, |input, cx| {
                input.set_placeholder(desired_placeholder, window, cx);
            });
        }
        let approval = self.approval.clone();
        // 审批条出现/消失时做一次焦点交接：出现时抢焦点承接 ⏎/Esc 快捷键，
        // 消失（决议或回合结束）后焦点还回输入框
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
        let cwd_open = matches!(self.popup, Some((Popup::Cwd, _)));
        let cwd_popup = cwd_open.then(|| self.render_cwd_popup(cx));
        let branch_open = matches!(self.popup, Some((Popup::Branch, _)));
        let branch_popup = branch_open.then(|| self.render_branch_popup(cx));
        let exec_open = matches!(self.popup, Some((Popup::ExecMode, _)));
        let exec_popup = exec_open.then(|| self.render_exec_mode_popup(cx));
        let model_open = matches!(self.popup, Some((Popup::Model, _)));
        let model_popup = model_open.then(|| self.render_model_popup(cx));
        let reasoning_levels: Vec<String> = self
            .models
            .iter()
            .find(|(_, _, model_id, _)| self.model.ends_with(&format!("/{model_id}")))
            .map(|(_, _, _, levels)| levels.clone())
            .unwrap_or_default();
        let reasoning_open = matches!(self.popup, Some((Popup::Reasoning, _)));
        let can_send = !self.input.read(cx).value().trim().is_empty();
        let reasoning_popup =
            reasoning_open.then(|| self.render_reasoning_popup(&reasoning_levels, cx));
        let context_open = matches!(self.popup, Some((Popup::Context, _)));
        let context_popup =
            (context_open && self.context_usage.is_some()).then(|| self.render_context_popup(cx));
        let palette_open =
            cwd_open || branch_open || exec_open || model_open || reasoning_open || context_open;
        let popup = if palette_open {
            None
        } else {
            self.render_popup(cx)
        };

        // 输入框容器表面色：暗色下提亮到 neutral-850 左右从窗口背景浮起（参考官网
        // message-scroller 的输入框）；弹层面板用主题 popover 深色 + 边框分界。
        let composer_surface = if cx.theme().is_dark() {
            hsla(0., 0., 0.11, 1.)
        } else {
            cx.theme().popover
        };
        let dark = cx.theme().is_dark();

        // 外框与内容分层：GPUI 会把元素的边框画在所有子孙之后（style.paint 先画背景，
        // 画完子元素才画边框），边框留在内容容器上的话，上方弹层会被容器顶边穿线；
        // 边框/背景拆成独立的底层兄弟元素先画，弹层就能正常盖住它。
        div().w_full().p_3().child(
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
                        // 暗色下靠表面色分界（官网样式无描边）；亮色下背景与窗口同为白色，
                        // 仍需描边分界
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
                                                                    "非 git 仓库".to_string()
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
                        .when(!self.todos.is_empty() || !self.tasks.is_empty(), |this| {
                            this.child(self.render_aux(cx))
                        })
                        .when(!self.attachments.is_empty(), |this| {
                            this.child(self.render_attachments(cx))
                        })
                        .when_some(approval.clone(), |this, approval| {
                            this.child(self.render_approval_bar(&approval, cx))
                        })
                        .when(approval.is_none(), |this| {
                            this.child(
                                div()
                                    .relative()
                                    .w_full()
                                    .child(
                                        Textarea::new(&self.input).appearance(false).bordered(false),
                                    )
                                    .children(popup),
                            )
                        })
                        .when(approval.is_none(), |this| {
                            this.child(
                            h_flex()
                                .w_full()
                                .gap_2()
                                .child(
                                    div()
                                        .relative()
                                        .child(self.render_bar_chip(
                                            "exec-mode",
                                            Some(exec_mode_icon(EXEC_MODES[self.exec_mode].2)),
                                            EXEC_MODES[self.exec_mode].0.to_string(),
                                            exec_open,
                                            cx.listener(move |this, event: &ClickEvent, window, cx| {
                                                let command = this.exec_command.clone();
                                                this.toggle_popup(
                                                    Popup::ExecMode,
                                                    event,
                                                    Some(command),
                                                    window,
                                                    cx,
                                                );
                                            }),
                                            cx,
                                        ))
                                        .when_some(exec_popup, |this, popup| this.child(popup)),
                                )
                                .child(div().flex_1())
                                .when_some(self.context_usage, |this, (used, total)| {
                                    // 上下文水位环形指示器（ZCode 同款）：悬停展示容量面板
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
                                            self.model.clone(),
                                            model_open,
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
                                                    self.reasoning_level
                                                        .clone()
                                                        .unwrap_or_else(|| "关".into()),
                                                    reasoning_open,
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
                                            .tooltip("停止")
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
                                            .tooltip("发送")
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
