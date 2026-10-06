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

const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/clear", "清空当前会话消息"),
    ("/compact", "压缩上下文（模型摘要）"),
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
        ExecMode::FullAccess => AssetIconName::ShieldAlert,
        // 无管制沿用警示图标（现有图标里没有更合适的）
        ExecMode::Yolo => AssetIconName::ShieldAlert,
    }
}

/// 模式色（弹层行的图标+label、输入框 chip）：按危险程度 中性 → 黄 → 橙 → 红。
/// 描述文字保持 muted 灰不上色。
fn exec_mode_color(mode: ExecMode, cx: &App) -> Hsla {
    let theme = cx.theme();
    match mode {
        // 中性：默认前景，不额外着色
        ExecMode::ConfirmBeforeEdit => theme.foreground,
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
    /// 这笔审批在 core 侧的等待 id（决议定向回复用）
    pub request_id: String,
    /// 审批来源会话（ExitPlanMode 的计划文件路径拼接用）
    pub session_id: String,
    pub tool: String,
    /// Bash 是命令原文；Write/Edit 是 diff 预览；ExitPlanMode 是计划全文
    pub detail: String,
    pub cwd: String,
}

impl PendingApproval {
    /// ExitPlanMode 的计划文件绝对路径（core 弹审批前已落盘）
    pub fn plan_path(&self) -> String {
        format!("{}/.pigcode/plans/plan-{}.md", self.cwd, self.session_id)
    }
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
    Compact {
        /// 命令分阶时用户在 chip 后续写的重点说明（无 = 普通压缩）
        instruction: Option<String>,
    },
    SetModel {
        provider_id: String,
        model_id: String,
    },
    /// 计划审批面板的路径链接：右侧「文件」tab 打开计划文件
    OpenFile {
        path: String,
    },
    SetReasoning(Option<String>),
    OpenSettings,
    SetExecMode(ExecMode),
    /// 计划模式开关（与执行模式正交；弹层勾选 / chip 关闭）
    SetPlanMode(bool),
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
    /// 审批条：批准 / 本会话内批准 / 拒绝（定向到条上挂的 request_id——
    /// 并发审批排队时各笔请求各答各的，不能笼统答「最后一个」）
    DecideApproval {
        request_id: String,
        decision: ApprovalDecision,
        /// 反馈意见（kimi Revise：计划「修改」提交时携带；其余审批为 None）
        feedback: Option<String>,
    },
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
    /// 「后台 Agent」chip 的弹层（与 Tasks（后台 Bash）分家，独立显隐/关闭）
    AgentTasks,
}

impl EventEmitter<ComposerEvent> for Composer {}

/// 弹层相对触发芯片的水平锚点。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PopupAnchor {
    Left,
    Right,
    /// 弹层水平中线对齐芯片中线（上下文容量面板用）。
    Center,
}

pub struct Composer {
    input: Entity<TextareaState>,
    exec_mode: usize,
    /// 计划模式开关（与 exec_mode 正交；开启时 bar 上显示独立「计划」chip）
    plan_enabled: bool,
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
    /// ExitPlanMode 审批的计划 markdown 视图（kimi 计划审批面板正文；
    /// 随 set_approval/decide_approval 建立与释放）
    plan_state: Option<Entity<gpui_kit::component::text::TextViewState>>,
    /// 计划「修改」输入态（kimi Revise）：true 时面板底部显示反馈输入框，
    /// 提交并拒绝携带反馈给模型修订
    plan_revise: bool,
    /// 反馈输入框（惰性创建，随面板复用）
    plan_revise_input: Option<Entity<InputState>>,
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
    /// / 和 @ 弹层的键盘选中项（Tab/↑↓ 切换；弹层开/查询变/结果刷新时归零）
    popup_sel: usize,
    /// / 和 @ 弹层列表的滚动句柄（选中项随导航滚进视野）
    popup_scroll: ScrollHandle,
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
                .placeholder(PLACEHOLDER_IDLE)
                .auto_grow(2, 8)
                .submit_on_enter(true)
        });

        let _subscriptions = vec![cx.subscribe_in(
            &input,
            window,
            |this: &mut Self, input, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { shift, .. } if !shift => {
                    // / 和 @ 弹层打开且有候选项：Enter 确认选中项（不发送）
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
            model: "未配置模型".to_string(),
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
        // 计划面板正文（ExitPlanMode 专用）：request_id 变了才重建，重复同步不丢滚动位置
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

    /// 审批条决议：清空审批态、发事件（带本条 request_id 定向）、焦点还回输入框。
    /// feedback 仅计划「修改」提交路径非 None（kimi Revise 携带给模型修订）
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

    /// 计划「修改」提交：拒绝并携带反馈文本（kimi Revise；空文本 = 裸拒绝）
    fn submit_plan_revise(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let feedback = self
            .plan_revise_input
            .as_ref()
            .map(|input| input.read(cx).value().to_string())
            .filter(|text| !text.trim().is_empty());
        self.decide_approval(ApprovalDecision::Reject, feedback, window, cx);
    }

    /// 计划「修改」取消：回三按钮态、清空输入、焦点还审批条
    fn cancel_plan_revise(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.plan_revise = false;
        if let Some(input) = &self.plan_revise_input {
            input.update(cx, |input, cx| input.set_value("", window, cx));
        }
        self.approval_focus.focus(window, cx);
        cx.notify();
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

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 命令分阶：命令 token 打头 → 按「命令 + 续写文本」分派，不发聊天消息
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
        // 触发位置落在某个 token 范围内 → 不是真触发符：删掉 chip/@token 尾随
        // 空格后光标紧贴 token 尾，纯文本扫描会把 token 里的 //@ 又当触发符
        //（命令 chip 与 @提及共有的「弹层复活」坑）
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
        // 查询变化即回到首项（与「过滤列表变化」的直觉一致）
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

    /// / 和 @ 弹层当前候选数（导航/确认与渲染共用同一过滤口径）
    fn popup_nav_count(&self, cx: &App) -> usize {
        match self.popup_query(cx).map(|(kind, _, query)| (kind, query)) {
            Some((Popup::Slash, query)) => Self::slash_filtered(&query).count(),
            Some((Popup::Mention, _)) => self.mention_results.len(),
            _ => 0,
        }
    }

    /// 弹层打开且有候选：Enter 应确认选中项而不是发送
    fn popup_selection_active(&self, cx: &App) -> bool {
        matches!(self.popup, Some((Popup::Mention | Popup::Slash, _)))
            && self.popup_nav_count(cx) > 0
    }

    fn slash_filtered(query: &str) -> impl Iterator<Item = &'static (&'static str, &'static str)> {
        SLASH_COMMANDS
            .iter()
            .filter(move |(name, _)| name[1..].contains(query))
    }

    /// Tab/↑/↓ 导航：弹层打开且有候选时循环切换并滚进视野；
    /// 否则 cx.propagate() 放行给输入框原生行为（光标移动/缩进）
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

    /// Esc（Input context 上的应用层绑定）：/ 和 @ 弹层开着就关掉；
    /// 其余弹层（Command 面板有自己的 Cancel 链）与无弹层时 propagate 放行
    fn close_popup_key(&mut self, cx: &mut Context<Self>) {
        if matches!(self.popup, Some((Popup::Mention | Popup::Slash, _))) {
            self.popup = None;
            cx.notify();
        } else {
            cx.propagate();
        }
    }

    /// Enter/点击确认当前选中项：Slash 分阶为命令 token（不执行），Mention 插文件 token
    fn confirm_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((kind, _start, query)) = self.popup_query(cx) else {
            return;
        };
        match kind {
            Popup::Slash => {
                let items: Vec<_> = Self::slash_filtered(&query).collect();
                let Some((name, _)) = items
                    .get(self.popup_sel.min(items.len().saturating_sub(1)))
                    .copied()
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

    /// 命令分阶：选中的斜杠命令替换为行首 InlineToken（chip），光标留在 token
    /// 之后继续输入——发送时才按「命令 + 续写文本」分派，而不是选中即执行
    ///（对齐 kimi-code 的 /compact 选中后「Compact 重点：xxxx」形态）
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
                // 同 @提及：token API 不自动加分隔符，补一个尾随空格
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

    /// 命令模式：输入以 /compact 或 /clear 命令 token 打头 → 返回 (命令, 续写文本)
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

    pub fn set_exec_mode(&mut self, mode: ExecMode, cx: &mut Context<Self>) {
        if let Some(ix) = EXEC_MODES.iter().position(|(_, _, m)| *m == mode) {
            self.exec_mode = ix;
        }
        cx.notify();
    }

    /// 计划模式开关（SessionConfigured/PlanModeChanged 同步，或弹层勾选）
    pub fn set_plan_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.plan_enabled = enabled;
        cx.notify();
    }

    /// 当前计划开关（hero 新建会话携带用）
    pub fn plan_enabled(&self) -> bool {
        self.plan_enabled
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
        div()
            .w_full()
            .p_3()
            // / 和 @ 弹层的键盘导航（动作冒泡自输入框；弹层关闭时处理器
            // cx.propagate() 放行，回落到输入框原生行为）
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
                                            // token 图标：命令 chip 用终端图标，@提及用文件图标
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
                                                // 高亮只跟鼠标走：清掉默认的键盘选中块
                                                //（否则首行常驻一个类高亮块，悬停
                                                // 计划行时读作「两个高亮」）；键盘
                                                // ↓ 会重新选中，行为不变
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
                                // 计划模式 chip（与权限档正交，ZCode composer 计划 chip 同款）：
                                // 灯泡 + 「计划」+ X 关闭；仅开启时渲染
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
                                                    .child("计划"),
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
