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
        "改文件、跑命令前先问我",
        ExecMode::ConfirmBeforeEdit,
    ),
    ("自动编辑", "自动编辑文件，跑命令前问我", ExecMode::AutoEdit),
    ("计划模式", "编辑前先出计划", ExecMode::Plan),
    (
        "完全访问",
        "全自动执行；高风险命令仍会弹窗确认",
        ExecMode::FullAccess,
    ),
    (
        "无管制模式",
        "全自动执行，无确认无拦截；仅限容器/沙箱使用",
        ExecMode::Yolo,
    ),
];

fn exec_mode_icon(mode: ExecMode) -> AssetIconName {
    match mode {
        ExecMode::ConfirmBeforeEdit => AssetIconName::Hand,
        ExecMode::AutoEdit => AssetIconName::ShieldCheck,
        ExecMode::Plan => AssetIconName::Lightbulb,
        ExecMode::FullAccess => AssetIconName::ShieldAlert,
        // 无管制沿用警示图标（现有图标里没有更合适的）
        ExecMode::Yolo => AssetIconName::ShieldAlert,
    }
}

/// 模式色（弹层行的图标+label、输入框 chip）：按危险程度 中性 → 蓝 → 黄 → 橙 → 红。
/// 描述文字保持 muted 灰不上色。
fn exec_mode_color(mode: ExecMode, cx: &App) -> Hsla {
    let theme = cx.theme();
    match mode {
        // 中性：默认前景，不额外着色
        ExecMode::ConfirmBeforeEdit => theme.foreground,
        // 蓝：只读/信息语义
        ExecMode::Plan => theme.info,
        // 黄/琥珀：中间档
        ExecMode::AutoEdit => theme.warning,
        // 橙：激进但有护栏（主题无 orange token，用 Tailwind 色板的 orange-500）
        ExecMode::FullAccess => gpui_kit::component::theme::orange_500(),
        // 红：无护栏
        ExecMode::Yolo => theme.danger,
    }
}

/// 模式弹层 footer 里的「区外读/写」小开关：label（text_xs muted）+ Checkbox
///（不挂 handler，点击冒泡到外层），文字与复选框整体一个点击区。
/// 点击切换并 emit SetFsAccess，不关弹层。
fn fs_toggle(
    id: &'static str,
    label: &'static str,
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

/// 剪贴板图片附件（chip 条展示；发送时转 PendingImage 下发）
struct PastedImage {
    bytes: std::sync::Arc<Vec<u8>>,
    mime: String,
    width: u32,
    height: u32,
}

/// 粘贴图片上限（ZCode 同款）
const MAX_PASTED_IMAGES: usize = 8;

/// 任务耗时：started→ended（或至今），"N 秒 / N 分"。
fn format_task_duration(started_at: u64, end: u64) -> String {
    let secs = end.saturating_sub(started_at);
    if secs < 60 {
        format!("{secs} 秒")
    } else {
        format!("{} 分", secs / 60)
    }
}

/// Questionnaire 的 choice value 直接用选项 label（提交回 label，与协议一致）：
/// 按 label 去重保序——服务端发出重复 label 时防 schema DuplicateChoice 错误与渲染 id 冲突。
fn dedup_question_options(question: &QuestionItem) -> Vec<&QuestionOption> {
    let mut seen = std::collections::HashSet::new();
    question
        .options
        .iter()
        .filter(|option| seen.insert(option.label.as_str()))
        .collect()
}
/// (供应商名, provider_id, model_id, 推理等级列表[(id, 显示名)])
pub type ModelOption = (String, String, String, Vec<(String, String)>);

/// 待审批的操作：审批期间输入框隐藏，显示审批条。
#[derive(Clone)]
pub struct PendingApproval {
    pub tool: String,
    /// Bash 是命令原文；Write/Edit 是 diff 预览
    pub detail: String,
    pub cwd: String,
}

/// 待回答的结构化提问：显示问题条时输入区隐藏（与审批条互斥，问题优先）。
#[derive(Clone)]
pub struct PendingQuestion {
    pub request_id: String,
    pub questions: Vec<QuestionItem>,
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

/// 后台任务 chip 类别（kimi-code 同款拆分）：Bash 后台任务 / 子代理（Agent）任务，
/// 按 TaskSummary.agent_id 分派——chip 与弹层各自独立显隐
#[derive(Clone, Copy, PartialEq, Eq)]
enum TaskChipKind {
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
            Self::Bash => "后台 Bash",
            Self::Agent => "后台 Agent",
        };
        if running > 0 {
            format!("{name} {running} 运行中")
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
        /// 剪贴板粘贴的图片附件（原始字节，core 侧压缩）
        images: Vec<pig_protocol::PendingImage>,
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
    /// 选中「无管制模式」：先弹确认框（AppView 宿主），确认后才走 SetExecMode
    RequestYoloConfirm,
    /// 模式菜单里的「工作区外读/写」开关
    SetFsAccess {
        read_outside: bool,
        write_outside: bool,
    },
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
    /// 问题条：提交（Some=各题选中标签）/ 跳过（None）
    QuestionReply {
        request_id: String,
        answers: Option<Vec<Vec<String>>>,
    },
    /// 改动 chip：直接打开右侧面板的改动 tab（不走弹层）
    OpenChanges,
    /// 「后台 Agent」弹层的任务行点击：打开右侧子代理对话 tab
    ///（title 用任务 command 原文；AppView 经 open_subagent_tab 处理）
    OpenSubagent {
        agent_id: String,
        title: String,
    },
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
    Todos,
    Tasks,
    /// 「后台 Agent」chip 的弹层（与 Tasks（后台 Bash）分家，独立显隐/关闭）
    AgentTasks,
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
    exec_mode: usize,
    /// 会话级「工作区外读/写」开关（模式菜单里的两个勾选项）
    fs_read_outside: bool,
    fs_write_outside: bool,
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
    /// 待回答提问：Some 时输入区隐藏，显示问题条（与审批互斥，问题优先）
    question: Option<PendingQuestion>,
    /// 问题条的问卷实体与事件订阅：InputState 需要 window 才能建，render 里惰性构建；
    /// request_id 变化 / 提交 / 放弃 / 清空时释放
    questionnaire: Option<(Entity<QuestionnaireState>, Subscription)>,
    /// 问卷当前页题号（0 起）：CurrentItemChanged 事件的镜像（debug_question 无 cx，读不了实体）
    question_current: usize,
    /// 问题条焦点（承接 Esc 放弃）
    question_focus: FocusHandle,
    question_focused: bool,
    mention_results: Vec<String>,
    context_usage: Option<(u64, u64, u64, u64)>,
    /// 输入区上方芯片：TodoList 进度 / 后台 Bash 任务快照（core 推送），点击弹出只读面板
    todos: Vec<TodoItem>,
    tasks: Vec<TaskSummary>,
    task_filter: TaskFilter,
    /// 本会话改动统计与文件列表（ReviewPanel 快照推送）
    changes: (u32, u32),
    change_files: Vec<(String, u32, u32)>,
    /// 展开输出尾部的任务行 id
    expanded_task: Option<String>,
    /// 剪贴板粘贴的图片附件（chip 条展示；发送时转 PendingImage 下发，发送后清空）
    pasted_images: Vec<PastedImage>,
    /// 粘贴提示（如超过 8 张上限）；下一次成功粘贴清除
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
            exec_mode: 1,
            fs_read_outside: false,
            fs_write_outside: false,
            model: "未配置模型".to_string(),
            models: vec![],
            reasoning_level: None,
            popup: None,
            outside_closed: None,
            streaming: false,
            approval: None,
            approval_focus: cx.focus_handle(),
            approval_focused: false,
            question: None,
            questionnaire: None,
            question_current: 0,
            question_focus: cx.focus_handle(),
            question_focused: false,
            mention_results: Vec::new(),
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

    /// 待回答提问：Some 时显示问题条；None 清除（提交/放弃/回合结束后）。
    /// request_id 变化（或清空）时问卷实体一并释放（render 惰性重建）；同一提问的
    /// 重复同步保留问卷（翻页与已选状态不丢）。
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

    /// 惰性构建问卷实体（InputState 需要 window，set_question 拿不到，故在 render 调用）。
    /// 每题：题号为 item 名、题干为标题、可选 header 为题注、选项 label 为 choice value
    /// （提交直接回 label，与协议一致）、每题一个「其他」自由文本输入；全部必答
    /// （对齐原「每题作答才可提交」门控），数字键快捷选中。
    fn ensure_questionnaire(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.questionnaire.is_some() {
            return;
        }
        let Some(question) = &self.question else {
            return;
        };
        let mut items = Vec::with_capacity(question.questions.len());
        for (ix, q) in question.questions.iter().enumerate() {
            let input = cx.new(|cx| InputState::new(window, cx).placeholder("说说你的想法…"));
            let choices: Vec<QuestionnaireChoiceDefinition> = dedup_question_options(q)
                .into_iter()
                .map(|option| {
                    let choice = QuestionnaireChoiceDefinition::new(
                        option.label.clone(),
                        option.label.clone(),
                    );
                    match &option.description {
                        Some(description) => choice.with_description(description.clone()),
                        None => choice,
                    }
                })
                .collect();
            let mut item = QuestionnaireItemDefinition::new(ix.to_string(), q.question.clone())
                .with_required(true)
                .with_multiple(q.multi_select)
                .with_choices(choices)
                .with_input(QuestionnaireInputDefinition::new(input, "其他"));
            if let Some(header) = &q.header {
                item = item.with_description(header.clone());
            }
            items.push(item);
        }
        let state = cx.new(|cx| {
            QuestionnaireState::new(items, cx)
                .map(|state| state.with_shortcuts(QuestionnaireShortcutMode::Numbers))
                .expect("item 名为题号、choice 已按 label 去重，schema 必然合法")
        });
        let sub = cx.subscribe_in(
            &state,
            window,
            |this, _state, event: &QuestionnaireEvent, window, cx| match event {
                // submit() 校验通过先 emit Completed 再 emit Submit：
                // finish 内部 take(question)，只处理先到的一个
                QuestionnaireEvent::Completed(submission)
                | QuestionnaireEvent::Submit(submission) => {
                    this.finish_question_submission(submission, window, cx);
                }
                // 翻页：镜像当前页题号
                QuestionnaireEvent::CurrentItemChanged { current, .. } => {
                    if let Some(ix) = current
                        .as_ref()
                        .and_then(|name| name.parse::<usize>().ok())
                    {
                        this.question_current = ix;
                    }
                    cx.notify();
                }
                // 选择/「其他」输入变化：重绘问题条刷新选中态与按钮显隐
                QuestionnaireEvent::AnswerChanged(_) => cx.notify(),
                _ => cx.notify(),
            },
        );
        self.question_current = 0;
        self.questionnaire = Some((state, sub));
    }

    /// 问卷提交：按题序收集答案（选中 label 按选项定义序 + 非空「其他」文本 trim 后
    /// 追加为一个 label），发 QuestionReply、清问题态与问卷实体、焦点还回输入框。
    fn finish_question_submission(
        &mut self,
        submission: &QuestionnaireSubmission,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(question) = self.question.take() else {
            return;
        };
        self.questionnaire = None;
        self.question_current = 0;
        let mut answers: Vec<Vec<String>> = Vec::with_capacity(question.questions.len());
        for ix in 0..question.questions.len() {
            let mut labels: Vec<String> = Vec::new();
            if let Some(answer) = submission.answer(&ix.to_string()) {
                labels.extend(answer.choices().iter().map(ToString::to_string));
                let other = answer
                    .freeform()
                    .map(|text| text.trim().to_string())
                    .unwrap_or_default();
                if !other.is_empty() {
                    labels.push(other);
                }
            }
            answers.push(labels);
        }
        cx.emit(ComposerEvent::QuestionReply {
            request_id: question.request_id,
            answers: Some(answers),
        });
        self.input.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    /// 放弃：回复 None（core 按「用户选择不回答」继续，不算错误）。
    fn skip_question(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(question) = self.question.take() {
            self.questionnaire = None;
            self.question_current = 0;
            cx.emit(ComposerEvent::QuestionReply {
                request_id: question.request_id,
                answers: None,
            });
            self.input.update(cx, |input, cx| input.focus(window, cx));
            cx.notify();
        }
    }

    /// 自测用：问题条是否在显示（返回当前页题干）。
    pub fn debug_question(&self) -> Option<String> {
        let question = self.question.as_ref()?;
        self.questionnaire.as_ref()?;
        let qix = self
            .question_current
            .min(question.questions.len().saturating_sub(1));
        question.questions.get(qix).map(|q| q.question.clone())
    }

    /// 自测用：等价点「下一题」（受当前题已作答门控；go_next 需要 Window，取首窗口）。
    pub fn debug_next_question_page(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.questionnaire.as_ref().map(|(state, _)| state.clone()) else {
            return;
        };
        let Some(window) = cx.windows().first().copied() else {
            return;
        };
        let _ = window.update(cx, |_, window, cx| {
            state.update(cx, |state, cx| {
                state.go_next(window, cx);
            });
        });
        cx.notify();
    }

    /// 自测用：选中某题某选项（等价点击选项按钮；不管焦点与「其他」输入）。
    pub fn debug_select_question_option(&mut self, qix: usize, oix: usize, cx: &mut Context<Self>) {
        let Some(label) = self
            .question
            .as_ref()
            .and_then(|q| q.questions.get(qix))
            .and_then(|q| q.options.get(oix))
            .map(|option| option.label.clone())
        else {
            return;
        };
        let Some(state) = self.questionnaire.as_ref().map(|(state, _)| state.clone()) else {
            return;
        };
        let name = qix.to_string();
        state.update(cx, |state, cx| {
            let _ = state.activate_choice(&name, &label, cx);
        });
        cx.notify();
    }

    /// 自测用：等价点「提交」（问卷校验全过才经事件订阅发 QuestionReply，门控与原实现一致）。
    pub fn debug_submit_question(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.questionnaire.as_ref().map(|(state, _)| state.clone()) else {
            return;
        };
        let Some(window) = cx.windows().first().copied() else {
            return;
        };
        let _ = window.update(cx, |_, window, cx| {
            state.update(cx, |state, cx| {
                state.submit(window, cx);
            });
        });
        cx.notify();
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().trim().to_string();
        // 有图片附件时允许空文本发送
        if text.is_empty() && self.pasted_images.is_empty() {
            return;
        }
        // @提及已存为 InlineToken 原子 token：从 token 列表收集文件
        //（不去重，与原先按空白切词的行为一致）；纯文本里的 @ 不再计入
        let files: Vec<String> = self
            .input
            .read(cx)
            .tokens()
            .iter()
            .filter_map(|span| span.token().text().strip_prefix('@'))
            .map(str::to_string)
            .collect();
        // 附件随消息下发并清空（chip 条消失）
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

    /// 切换会话/回 hero 时清掉上一个会话的水位（新会话的 ContextUsage 到达前不显示）
    pub fn clear_context_usage(&mut self, cx: &mut Context<Self>) {
        if self.context_usage.take().is_some() {
            cx.notify();
        }
    }

    pub fn set_todos(&mut self, todos: Vec<TodoItem>, cx: &mut Context<Self>) {
        // 清单清空时收起对应弹层，避免 popup 状态悬置
        if todos.is_empty() && matches!(self.popup, Some((Popup::Todos, _))) {
            self.popup = None;
        }
        self.todos = todos;
        cx.notify();
    }

    /// 自测用：(「后台 Bash」chip 可见, 「后台 Agent」chip 可见)——
    /// chip 显隐由任务快照按 agent_id 类型分派
    pub fn debug_task_chips(&self) -> (bool, bool) {
        (
            self.tasks.iter().any(|t| t.agent_id.is_none()),
            self.tasks.iter().any(|t| t.agent_id.is_some()),
        )
    }

    /// 自测用：Agent 任务行的 agent_id 列表
    pub fn debug_agent_task_ids(&self) -> Vec<String> {
        self.tasks
            .iter()
            .filter_map(|t| t.agent_id.clone())
            .collect()
    }

    /// 自测用：模拟点开「后台 Agent」chip 弹层（复现 palette_open 漏 AgentTasks
    /// 导致 render_popup 踩 unreachable 的崩溃路径）；返回弹层是否打开
    pub fn debug_open_agent_tasks_popup(&mut self, cx: &mut Context<Self>) -> bool {
        self.popup = Some((Popup::AgentTasks, 0));
        cx.notify();
        matches!(self.popup, Some((Popup::AgentTasks, _)))
    }

    /// 自测用：收起任意弹层
    pub fn debug_close_popup(&mut self, cx: &mut Context<Self>) {
        self.popup = None;
        cx.notify();
    }

    pub fn set_tasks(&mut self, tasks: Vec<TaskSummary>, cx: &mut Context<Self>) {
        // 对应类别的任务清空时收起对应弹层（chip 按 agent_id 拆分后各自判定）
        if !tasks.iter().any(|t| t.agent_id.is_none())
            && matches!(self.popup, Some((Popup::Tasks, _)))
        {
            self.popup = None;
        }
        if !tasks.iter().any(|t| t.agent_id.is_some())
            && matches!(self.popup, Some((Popup::AgentTasks, _)))
        {
            self.popup = None;
        }
        self.tasks = tasks;
        cx.notify();
    }

    /// 改动统计 + 文件列表（ReviewPanel 快照）
    pub fn set_changes(
        &mut self,
        added: u32,
        removed: u32,
        files: Vec<(String, u32, u32)>,
        cx: &mut Context<Self>,
    ) {
        self.changes = (added, removed);
        self.change_files = files;
        cx.notify();
    }

    /// 自测用：返回 (used, total)。
    pub fn debug_context_usage(&self) -> Option<(u64, u64)> {
        self.context_usage.map(|(used, total, _, _)| (used, total))
    }

    pub fn set_exec_mode(&mut self, mode: ExecMode, cx: &mut Context<Self>) {
        if let Some(ix) = EXEC_MODES.iter().position(|(_, _, m)| *m == mode) {
            self.exec_mode = ix;
        }
        cx.notify();
    }

    /// 回焦输入框（对话框/弹层关闭后由 AppView 调用）
    pub fn focus_input(&self, window: &mut Window, cx: &mut App) {
        self.input.update(cx, |input, cx| input.focus(window, cx));
    }

    /// 恢复会话持久化的区外读写开关（会话切换/新建/回放时由 meta 同步）
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

    /// 恢复会话持久化的思考等级（会话切换时由 SessionConfigured 同步）
    pub fn set_reasoning_level(&mut self, level: Option<String>, cx: &mut Context<Self>) {
        self.reasoning_level = level;
        cx.notify();
    }

    /// 自测用。
    pub fn debug_exec_mode(&self) -> ExecMode {
        EXEC_MODES[self.exec_mode].2
    }

    /// token 数自动单位：<1k 原样；k/M 级整除显示整数、否则一位小数
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

    /// 上下文容量面板：标题 + 用量/占比 + 进度条 + 平均缓存命中率，
    /// 居中锚定在指示器芯片正上方（悬停展示）。
    fn render_context_popup(&self, cx: &mut Context<Self>) -> AnyElement {
        let (used, total, cache_read_total, input_total) =
            self.context_usage.unwrap_or((0, 1, 0, 0));
        let ratio = (used as f32 / total as f32).clamp(0.0, 1.0);
        let bar_color = if ratio > 0.8 {
            cx.theme().warning
        } else {
            cx.theme().progress_bar
        };
        let cache_total = cache_read_total + input_total;

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
                                "{} / {} ({:.1}%)",
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
            .when(cache_total > 0, |this| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!(
                            "平均缓存命中率 {:.1}%（命中 {} / 输入 {}）",
                            cache_read_total as f64 / cache_total as f64 * 100.0,
                            Self::format_tokens_compact(cache_read_total),
                            Self::format_tokens_compact(cache_total),
                        )),
                )
            })
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
        // 文档文本是完整 @path（发送时据此收集文件），展示文本只显示文件名
        let file_name = path.rsplit('/').next().unwrap_or(path.as_str());
        let token =
            InlineToken::new(path.clone(), format!("@{path}")).with_label(format!("@{file_name}"));
        self.input.update(cx, |input, cx| {
            if input
                .replace_range_with_token(start..caret, token, window, cx)
                .is_ok()
            {
                // token API 不自动加分隔符：插入后选区已塌缩在 token 尾，补一个尾随空格
                input.replace(" ", window, cx);
            } else {
                // token 校验失败等场景回落为纯文本插入
                input.set_selected_range(start..caret, cx);
                input.replace(format!("@{path} "), window, cx);
            }
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
            | Popup::Context
            | Popup::Todos
            | Popup::Tasks
            | Popup::AgentTasks => {
                unreachable!(
                    "Cwd/Branch/ExecMode/Model/Reasoning/Context/Todos/Tasks/AgentTasks 由各自的专用面板渲染"
                )
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
            Popup::Todos => "todos",
            Popup::Tasks => "tasks",
            Popup::AgentTasks => "agent-tasks",
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
            // 滚轮事件不穿透到弹层背后的会话消息流；内容自身的滚动（Command 虚拟列表
            // 等更深的滚动区）先消费事件，不受影响
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
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
    /// `color` 非 None 时图标与文本着色（模式芯片按危险程度着色用），箭头保持 muted。
    fn render_bar_chip(
        &self,
        id: &'static str,
        icon: Option<AssetIconName>,
        label: String,
        filled: bool,
        color: Option<Hsla>,
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
                        .text_color(color.unwrap_or(cx.theme().muted_foreground)),
                )
            })
            .child(
                div()
                    .text_sm()
                    .when_some(color, |this, color| this.text_color(color))
                    .child(label),
            )
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

    /// 执行模式面板：Command 单选模式列表（键盘导航保持可用）。
    /// 「工作区外访问」开关区放在 Command 的 footer 槽：模式（单选）与开关（多选）
    /// 分区展示，开关行不参与 Command 的键盘选择（鼠标交互，可接受的取舍）。
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
                                // 图标与 label 按模式危险程度上色（显式 text_color，
                                // hover/选中的继承色盖不住模式色）；描述保持 muted
                                let mode_color = exec_mode_color(*mode, cx);
                                h_flex()
                                    .flex_1()
                                    .gap_2()
                                    .items_center()
                                    .child(Icon::new(icon).size_4().text_color(mode_color))
                                    .child(
                                        v_flex()
                                            .child(div().text_color(mode_color).child(*label))
                                            .child(
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
                        if *mode == ExecMode::Yolo && this.exec_mode != ix.row {
                            // 无管制模式：先关弹层再弹确认框（确认后才 emit SetExecMode）
                            this.close_command_popup(window, cx);
                            cx.emit(ComposerEvent::RequestYoloConfirm);
                            return;
                        }
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
            })
            // footer 槽：分隔线 + 单行开关区（「工作区外访问  读 ☐  写 ☑」）；
            // 复选框即开关态视觉，不再用 CommandItem.checked 的 ✓
            .footer({
                let composer = cx.entity();
                let read_on = self.fs_read_outside;
                let write_on = self.fs_write_outside;
                move |_, _window, cx| {
                    let muted = cx.theme().muted_foreground;
                    v_flex()
                        .w_full()
                        .pt_1()
                        .child(Separator::horizontal())
                        .child(
                            h_flex()
                                .w_full()
                                .px_2()
                                .py_1p5()
                                .items_center()
                                .justify_between()
                                .child(div().text_xs().text_color(muted).child("工作区外访问"))
                                .child(
                                    h_flex()
                                        .gap_3()
                                        .items_center()
                                        .child(fs_toggle(
                                            "fs-access-read",
                                            "读",
                                            read_on,
                                            true,
                                            composer.clone(),
                                            cx,
                                        ))
                                        .child(fs_toggle(
                                            "fs-access-write",
                                            "写",
                                            write_on,
                                            false,
                                            composer.clone(),
                                            cx,
                                        )),
                                ),
                        )
                        .into_any_element()
                }
            });
        // 定宽 300px：开关区描述文字按宽度换行，不再被弹层裁切
        let content = v_flex().w(px(300.)).child(command).into_any_element();
        self.popup_shell(
            "composer-exec-popup",
            content,
            PopupAnchor::Left,
            Some(self.exec_command.clone()),
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
                        eprintln!("[model-popup] 点击「管理模型」(section=0)");
                        cx.emit(ComposerEvent::OpenSettings);
                    } else if let Some((_, items)) = groups.get(ix.section - 1)
                        && let Some((provider_name, provider_id, model_id, _)) = items.get(ix.row)
                    {
                        eprintln!(
                            "[model-popup] 选中 section={} row={} → {provider_id}/{model_id}",
                            ix.section, ix.row
                        );
                        this.model = format!("{provider_name}/{model_id}");
                        cx.emit(ComposerEvent::SetModel {
                            provider_id: provider_id.clone(),
                            model_id: model_id.clone(),
                        });
                    } else {
                        eprintln!(
                            "[model-popup] 点击未能解析为模型: section={} row={}",
                            ix.section, ix.row
                        );
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
    /// levels 为 (id, 显示名)；界面展示显示名，确认回传 id。
    fn render_reasoning_popup(
        &self,
        levels: &[(String, String)],
        cx: &mut Context<Self>,
    ) -> AnyElement {
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
            .items(levels.iter().map(|(id, label)| {
                // 显示名与 id 不同则括号附上 id，避免歧义
                let text = if label == id {
                    id.clone()
                } else {
                    format!("{label}（{id}）")
                };
                CommandItem::new()
                    .label(text)
                    .checked(self.reasoning_level.as_deref() == Some(id.as_str()))
            }))
            .on_confirm(move |ix, window, cx| {
                on_confirm_composer.update(cx, |this, cx| {
                    let level = if ix.row == 0 {
                        None
                    } else {
                        levels.get(ix.row - 1).map(|(id, _)| id.clone())
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

    /// 粘贴入口（Textarea::on_paste）：仲裁结果决定是否拦截默认文本插入。
    /// 返回 true = 已作为附件处理，输入框不插文本；false = 交给引擎插文本。
    fn handle_paste(
        &mut self,
        item: &ClipboardItem,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        use crate::clipboard::{PasteArb, arbitrate_clipboard};
        let entry_count = item.entries.len();
        match arbitrate_clipboard(item) {
            PasteArb::FilePath(path) => {
                // >20MB 跳过到文本粘贴（粘贴路径文本）；读不出/非图片同样落回文本
                let Ok(meta) = std::fs::metadata(&path) else {
                    eprintln!("[clipboard] paste FilePath failed: metadata unavailable");
                    return false;
                };
                if meta.len() > 20 * 1024 * 1024 {
                    eprintln!(
                        "[clipboard] paste FilePath skipped: size={} exceeds 20MB",
                        meta.len()
                    );
                    return false;
                }
                let Ok(bytes) = std::fs::read(&path) else {
                    eprintln!("[clipboard] paste FilePath failed: read error");
                    return false;
                };
                let Some(mime) = pig_core::tool::sniff_image(&bytes) else {
                    eprintln!("[clipboard] paste FilePath skipped: unsupported file bytes");
                    return false;
                };
                eprintln!(
                    "[clipboard] paste FilePath accepted: mime={}, bytes={}, entries={}",
                    mime,
                    bytes.len(),
                    entry_count
                );
                self.attach_image(bytes, mime, cx);
                true
            }
            PasteArb::ImageBytes { bytes, mime } => {
                eprintln!(
                    "[clipboard] paste ImageBytes accepted: mime={}, bytes={}, entries={}",
                    mime,
                    bytes.len(),
                    entry_count
                );
                self.attach_image(bytes, mime, cx);
                true
            }
            PasteArb::Text => {
                eprintln!("[clipboard] paste Text: fallback to input text");
                false
            }
            PasteArb::Nothing => {
                eprintln!("[clipboard] paste Nothing: fallback to default paste");
                false
            }
        }
    }

    /// 图片进附件列表（chip 条）：超上限只提示不附加；TIFF 在这里规范化为 PNG。
    fn attach_image(&mut self, mut bytes: Vec<u8>, mut mime: &str, cx: &mut Context<Self>) {
        if self.pasted_images.len() >= MAX_PASTED_IMAGES {
            eprintln!(
                "[clipboard] attach_image skipped: already at max={} images",
                MAX_PASTED_IMAGES
            );
            self.paste_note = Some(format!("最多粘贴 {MAX_PASTED_IMAGES} 张图片"));
            cx.notify();
            return;
        }
        if mime == "image/tiff" {
            let source_bytes = bytes.len();
            match pig_core::tool::convert_tiff_to_png(&bytes) {
                Ok((png, width, height)) => {
                    eprintln!(
                        "[clipboard] TIFF converted to PNG: {}x{}, bytes={} -> {}",
                        width,
                        height,
                        source_bytes,
                        png.len()
                    );
                    bytes = png;
                    mime = "image/png";
                }
                Err(error) => {
                    eprintln!("[clipboard] TIFF conversion failed: {error}");
                    self.paste_note = Some(format!("TIFF 图片无法转换：{error}"));
                    cx.notify();
                    return;
                }
            }
        }
        self.paste_note = None;
        let byte_len = bytes.len();
        let (width, height) = pig_core::tool::image_dimensions(&bytes).unwrap_or((0, 0));
        self.pasted_images.push(PastedImage {
            bytes: std::sync::Arc::new(bytes),
            mime: mime.to_string(),
            width,
            height,
        });
        eprintln!(
            "[clipboard] attach_image stored: mime={}, bytes={}, dimensions={}x{}, count={}",
            mime,
            byte_len,
            width,
            height,
            self.pasted_images.len()
        );
        cx.notify();
    }

    /// 图片附件条：官方 AttachmentGroup（每图一个 Attachment：缩略图 + 尺寸/体积 +
    /// 悬停删除钮）；paste_note 警告行跟在 Group 之后（样式不变）。
    /// `surface` 是行背后的表面色（输入框容器色），用于 Group 的边缘渐隐。
    fn render_pasted_images(&self, surface: Hsla, cx: &mut Context<Self>) -> AnyElement {
        let attachments: Vec<AnyElement> = self
            .pasted_images
            .iter()
            .enumerate()
            .map(|(ix, image)| {
                let format = match image.mime.as_str() {
                    "image/jpeg" => ImageFormat::Jpeg,
                    "image/webp" => ImageFormat::Webp,
                    "image/gif" => ImageFormat::Gif,
                    _ => ImageFormat::Png,
                };
                let thumb = std::sync::Arc::new(gpui_kit::Image {
                    format,
                    bytes: (*image.bytes).clone(),
                    id: gpui_kit::hash(&(image.bytes.as_slice(), ix)),
                });
                let title = format!("图片 {}", ix + 1);
                let info = format!(
                    "{}×{} · {}KB",
                    image.width,
                    image.height,
                    image.bytes.len() / 1024
                );
                Attachment::new()
                    .id(("pasted-image", ix))
                    .media(AttachmentMedia::new().src(thumb))
                    .content(
                        AttachmentContent::new()
                            .title(AttachmentTitle::new(title.clone()))
                            .description(AttachmentDescription::new(info.clone())),
                    )
                    .tooltip(format!("{title}（{info}）"))
                    .on_remove(cx.listener(move |this, _, _, cx| {
                        this.pasted_images.remove(ix);
                        cx.notify();
                    }))
                    .axis(Axis::Horizontal)
                    .small()
                    .into_any_element()
            })
            .collect();
        v_flex()
            .w_full()
            .when(!attachments.is_empty(), |this| {
                this.child(
                    AttachmentGroup::new("pasted-images")
                        .with_edge_fade(surface)
                        .children(attachments),
                )
            })
            .when_some(self.paste_note.clone(), |this, note| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().warning)
                        .child(note),
                )
            })
            .into_any_element()
    }

    /// 当前进度（TodoList）+ 后台 Bash 任务 + 会话改动：chip 行。
    /// 进度/任务 chip 点击在芯片上方弹出只读面板（v1 无停止按钮）；
    /// 改动 chip 发事件让 AppView 打开右侧面板的改动 tab。
    fn render_aux(&self, cx: &mut Context<Self>) -> AnyElement {
        let bash_running = self
            .tasks
            .iter()
            .filter(|t| TaskChipKind::Bash.matches(t) && matches!(t.status, TaskStatus::Running))
            .count();
        let agent_running = self
            .tasks
            .iter()
            .filter(|t| TaskChipKind::Agent.matches(t) && matches!(t.status, TaskStatus::Running))
            .count();
        let done = self
            .todos
            .iter()
            .filter(|t| t.status == TodoStatus::Done)
            .count();

        let mut chips = h_flex().w_full().gap_2();
        // chip 按任务类型拆分（kimi-code 同款）：「后台 Bash」「后台 Agent」各自独立
        // 显隐与弹层；对应类别的任务列表为空时该 chip 不出现
        if self.tasks.iter().any(|t| TaskChipKind::Bash.matches(t)) {
            let open = matches!(self.popup, Some((Popup::Tasks, _)));
            chips = chips.child(
                div()
                    .relative()
                    .child(self.render_aux_chip(
                        "aux-tasks",
                        AssetIconName::Terminal,
                        TaskChipKind::Bash.label(bash_running),
                        open,
                        Popup::Tasks,
                        cx,
                    ))
                    .when(open, |this| {
                        this.child(self.render_tasks_panel(TaskChipKind::Bash, cx))
                    }),
            );
        }
        if self.tasks.iter().any(|t| TaskChipKind::Agent.matches(t)) {
            let open = matches!(self.popup, Some((Popup::AgentTasks, _)));
            chips = chips.child(
                div()
                    .relative()
                    .child(self.render_aux_chip(
                        "aux-agent-tasks",
                        IconName::Bot,
                        TaskChipKind::Agent.label(agent_running),
                        open,
                        Popup::AgentTasks,
                        cx,
                    ))
                    .when(open, |this| {
                        this.child(self.render_tasks_panel(TaskChipKind::Agent, cx))
                    }),
            );
        }
        if !self.change_files.is_empty() {
            let (added, removed) = self.changes;
            chips = chips.child(
                h_flex()
                    .id("aux-changes")
                    .gap_1()
                    .px_3()
                    .py_1()
                    .rounded_full()
                    .cursor_pointer()
                    .hover(|this| this.bg(cx.theme().accent))
                    // 不再弹层：点击直接打开右侧面板的改动 tab
                    .on_click(cx.listener(|_, _, _, cx| {
                        cx.emit(ComposerEvent::OpenChanges);
                    }))
                    .child(
                        Icon::new(AssetIconName::Diff)
                            .size_4()
                            .text_color(cx.theme().muted_foreground),
                    )
                    .child(div().text_sm().child("改动"))
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().success)
                            .child(format!("+{added}")),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().danger)
                            .child(format!("-{removed}")),
                    ),
            );
        }
        if !self.todos.is_empty() {
            let open = matches!(self.popup, Some((Popup::Todos, _)));
            chips = chips.child(
                div()
                    .relative()
                    .child(self.render_aux_chip(
                        "aux-todos",
                        AssetIconName::ListTodo,
                        format!("当前进度 {done}/{}", self.todos.len()),
                        open,
                        Popup::Todos,
                        cx,
                    ))
                    .when(open, |this| this.child(self.render_todos_panel(cx))),
            );
        }
        chips.into_any_element()
    }

    fn render_aux_chip(
        &self,
        id: &'static str,
        icon: impl Into<Icon>,
        label: String,
        active: bool,
        kind: Popup,
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
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.toggle_popup(kind, event, None, window, cx);
            }))
            .child(
                Icon::new(icon)
                    .size_4()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(div().text_sm().child(label))
    }

    /// 面板内容外壳：弹层内衬（rounded_xl + popover 背景 + 边框），观感与其他弹层一致。
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
        let mut list = v_flex()
            .id("aux-todos-list")
            .w_full()
            .gap_1()
            .max_h(px(280.))
            .overflow_y_scroll();
        for item in &self.todos {
            // 进行中用 Spinner（旋转动画），其余状态静态图标
            let icon = match item.status {
                TodoStatus::Done => Icon::new(AssetIconName::CircleCheck)
                    .size_4()
                    .text_color(cx.theme().success)
                    .into_any_element(),
                TodoStatus::InProgress => Spinner::new()
                    .icon(AssetIconName::LoaderCircle)
                    .color(cx.theme().progress_bar)
                    .into_any_element(),
                TodoStatus::Pending => Icon::new(AssetIconName::Circle)
                    .size_4()
                    .text_color(cx.theme().muted_foreground)
                    .into_any_element(),
            };
            list = list.child(
                h_flex().w_full().gap_2().child(icon).child(
                    div()
                        .text_sm()
                        .when(item.status == TodoStatus::Done, |this| {
                            this.text_color(cx.theme().muted_foreground)
                        })
                        .child(item.content.clone()),
                ),
            );
        }
        let content = self.aux_panel_shell(
            v_flex()
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!("当前进度 {done}/{}", self.todos.len())),
                )
                .child(list),
            cx,
        );
        self.popup_shell(
            "composer-todos-popup",
            content.into_any_element(),
            PopupAnchor::Left,
            None,
            cx,
        )
    }

    /// 后台任务弹层（按 kind 拆成「后台 Bash / 后台 Agent」两个独立面板）：
    /// Bash 行点击展开输出尾部（现状）；Agent 行点击开右侧子代理对话 tab
    ///（收起弹层 + ComposerEvent::OpenSubagent 上冒给 AppView）
    fn render_tasks_panel(&self, kind: TaskChipKind, cx: &mut Context<Self>) -> AnyElement {
        let running = self
            .tasks
            .iter()
            .filter(|t| kind.matches(t) && matches!(t.status, TaskStatus::Running))
            .count();
        let title = kind.label(running);

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
            .filter(|t| kind.matches(t) && self.task_filter.matches(t.status))
            .collect();
        let list_id = match kind {
            TaskChipKind::Bash => "aux-tasks-list",
            TaskChipKind::Agent => "aux-agent-tasks-list",
        };
        let mut list = v_flex()
            .id(list_id)
            .w_full()
            .gap_1()
            .max_h(px(280.))
            .overflow_y_scroll();
        if visible.is_empty() {
            let empty = match kind {
                TaskChipKind::Bash => "无后台 Bash 任务",
                TaskChipKind::Agent => "无后台 Agent 任务",
            };
            list = list.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(empty),
            );
        }
        for (ix, task) in visible.iter().enumerate() {
            let icon = match task.status {
                TaskStatus::Running => Spinner::new()
                    .icon(AssetIconName::LoaderCircle)
                    .color(cx.theme().progress_bar)
                    .into_any_element(),
                TaskStatus::Exited(0) => Icon::new(AssetIconName::CircleCheck)
                    .size_4()
                    .text_color(cx.theme().success)
                    .into_any_element(),
                TaskStatus::Exited(_) => Icon::new(AssetIconName::TriangleAlert)
                    .size_4()
                    .text_color(cx.theme().warning)
                    .into_any_element(),
                TaskStatus::Killed => Icon::new(AssetIconName::TriangleAlert)
                    .size_4()
                    .text_color(cx.theme().muted_foreground)
                    .into_any_element(),
            };
            let duration = format_task_duration(task.started_at, task.ended_at.unwrap_or(now));
            let expanded = self.expanded_task.as_deref() == Some(task.id.as_str());
            let task_id = task.id.clone();
            let agent_open = task
                .agent_id
                .clone()
                .map(|agent_id| (agent_id, task.command.clone()));
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
                        // Agent 行：点击开右侧子代理对话 tab（收弹层 + 上冒事件）；
                        // Bash 行：展开/收起输出尾部
                        if let Some((agent_id, title)) = &agent_open {
                            this.popup = None;
                            cx.emit(ComposerEvent::OpenSubagent {
                                agent_id: agent_id.clone(),
                                title: title.clone(),
                            });
                        } else {
                            this.expanded_task =
                                if this.expanded_task.as_deref() == Some(task_id.as_str()) {
                                    None
                                } else {
                                    Some(task_id.clone())
                                };
                        }
                        cx.notify();
                    }))
                    .child(icon)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_x_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_sm()
                            .font_family(cx.theme().mono_font_family.clone())
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
            // 输出尾部展开仅 Bash 行（Agent 的完整对话在右侧 tab 看）
            if kind == TaskChipKind::Bash && expanded {
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
                        .font_family(cx.theme().mono_font_family.clone())
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

        // 固定宽：有/无内容同宽。宽度不许超过 popup_shell 内层容器的 max_w(360)——
        // 超出部分没有 hitbox（可视正常但点击被判 outside 触发弹层关闭）；
        // 命令文本超长走省略（行内 text_ellipsis）
        let content = self
            .aux_panel_shell(
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
            .w(px(360.));
        let popup_id = match kind {
            TaskChipKind::Bash => "composer-tasks-popup",
            TaskChipKind::Agent => "composer-agent-tasks-popup",
        };
        self.popup_shell(
            popup_id,
            content.into_any_element(),
            PopupAnchor::Left,
            None,
            cx,
        )
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
                    .font_family(cx.theme().mono_font_family.clone())
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
                    // 与问卷动作按钮同尺寸（Small），各确认条按钮字号一致
                    .child(
                        Button::new("approval-always")
                            .secondary()
                            .small()
                            .label("本会话内批准  Ctrl+⏎")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.decide_approval(ApprovalDecision::AlwaysAllow, window, cx);
                            })),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("approval-reject")
                            .secondary()
                            .small()
                            .label("拒绝  Esc")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.decide_approval(ApprovalDecision::Reject, window, cx);
                            })),
                    )
                    .child(
                        Button::new("approval-allow")
                            .primary()
                            .small()
                            .label("批准  ⏎")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.decide_approval(ApprovalDecision::Allow, window, cx);
                            })),
                    ),
            )
            .into_any_element()
    }

    /// 问题条（gpui-kit Questionnaire 官方组件，向导分页一次一题）：Progress（题号/总数）→
    /// 当前题 Item（Title 题干 / Description 放可选 header / Choices 选项卡 / Input「其他」/
    /// Error 校验提示）→ Actions（[上一题] [放弃 Esc] [下一题]/[提交]，按问卷导航态自动显隐）。
    /// 数字键选选项、⏎ 确认进下一题/提交由 Questionnaire 根的键盘路由承接；「放弃」协议是
    /// 整卷 answers: None（官方 Skip 是逐题跳过，表达不了），自绘按钮 + 外层 Esc 走 skip_question。
    fn render_question_bar(
        &self,
        question: &PendingQuestion,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some((state, _)) = &self.questionnaire else {
            // ensure_questionnaire 先于本帧渲染执行，正常到不了这里
            return div().into_any_element();
        };
        let progress = state.read(cx).progress();
        let mut item_parts = Vec::with_capacity(question.questions.len());
        for (ix, q) in question.questions.iter().enumerate() {
            let name = ix.to_string();
            // 与 ensure_questionnaire 同一套 label 去重（渲染 id 按 value 生成）
            let choice_parts: Vec<QuestionnaireChoice> = dedup_question_options(q)
                .into_iter()
                .map(|option| {
                    // 选项卡两行（label+description）：官方默认把指示器/角标对齐
                    // 首行文本（items_start），整行垂直居中更顺眼
                    QuestionnaireChoice::new(state, name.clone(), option.label.clone())
                        .items_center()
                })
                .collect();
            // 非当前题的 part 自行渲染为空，全部挂上即可
            item_parts.push(
                QuestionnaireItem::new(state, name.clone())
                    .child(QuestionnaireTitle::new(state, name.clone()))
                    .child(QuestionnaireDescription::new(state, name.clone()))
                    .child(QuestionnaireChoices::new(state, name.clone()).children(choice_parts))
                    .child(QuestionnaireInput::new(state, name.clone()))
                    .child(QuestionnaireError::new(state, name.clone())),
            );
        }
        v_flex()
            .id("question-bar")
            .w_full()
            .gap_3()
            .p_2()
            .track_focus(&self.question_focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                // Esc 放弃走协议层（answers: None），Questionnaire 无对应概念
                if event.keystroke.key.as_str() == "escape" {
                    this.skip_question(window, cx);
                }
            }))
            .child(
                Questionnaire::new(state)
                    // 输入区是紧凑条形：整体小一号贴现状（行距/题干字重沿用 part 默认）
                    .with_size(Size::Small)
                    .child(
                        QuestionnaireProgress::new(state)
                            .child(format!("{}/{}", progress.current(), progress.total())),
                    )
                    .children(item_parts)
                    .child(
                        QuestionnaireActions::new(state)
                            .child(QuestionnairePrevious::new(state).child("上一题"))
                            .child(
                                // 与官方问卷动作按钮同尺寸（Small）
                                Button::new("question-skip")
                                    .secondary()
                                    .small()
                                    .label("放弃  Esc")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.skip_question(window, cx);
                                    })),
                            )
                            .child(QuestionnaireNext::new(state).child("下一题  ⏎"))
                            .child(QuestionnaireSubmit::new(state).child("提交  ⏎")),
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
        let question = self.question.clone();
        self.ensure_questionnaire(window, cx);
        // 问题条焦点交接（与审批条同模式；与审批互斥、问题优先）：出现时焦点交给问卷
        // 当前题（承接数字键/⏎），消失后焦点还回输入框
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
        // 当前选中等级的显示名（缺省回退 id 本身）
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
                        .when(
                            // hero（新会话页）不属于任何会话：进度/任务/改动 chip 一律不显示
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
                                            // @提及 token 的展示：默认样式加文件图标
                                            .token(|ctx, _, _| {
                                                InputToken::new(ctx).icon(IconName::FileText)
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
                                            Some(exec_mode_icon(EXEC_MODES[self.exec_mode].2)),
                                            EXEC_MODES[self.exec_mode].0.to_string(),
                                            exec_open,
                                            Some(exec_mode_color(EXEC_MODES[self.exec_mode].2, cx)),
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
                                .when_some(self.context_usage, |this, (used, total, _, _)| {
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
                                                        .unwrap_or_else(|| "关".into()),
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
