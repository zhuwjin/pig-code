use gpui_kit::assets::IconName as AssetIconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::command::{Command, CommandGroup, CommandItem, CommandState};
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::progress::ProgressCircle;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use pig_protocol::ExecMode;

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
/// (供应商名, provider_id, model_id, 推理等级列表)
pub type ModelOption = (String, String, String, Vec<String>);

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
    /// hero：取消项目选择（不在项目中工作）
    ClearCwd,
    /// hero：切换 git 分支
    CheckoutBranch(String),
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

pub struct Composer {
    input: Entity<TextareaState>,
    attachments: Vec<&'static str>,
    exec_mode: usize,
    model: String,
    models: Vec<ModelOption>,
    reasoning_level: Option<String>,
    popup: Option<(Popup, usize)>,
    streaming: bool,
    approval_pending: bool,
    mention_results: Vec<String>,
    context_usage: Option<(u64, u64)>,
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
            streaming: false,
            approval_pending: false,
            mention_results: Vec::new(),
            context_usage: None,
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

    pub fn set_approval_pending(&mut self, pending: bool, cx: &mut Context<Self>) {
        self.approval_pending = pending;
        cx.notify();
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

    /// 上下文容量面板：标题 + 用量/占比 + 进度条，锚定在指示器芯片正上方。
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
            .bg(Self::command_surface(cx))
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
                    .bg(cx.theme().border)
                    .child(
                        div()
                            .h_full()
                            .w(relative(ratio))
                            .rounded_full()
                            .bg(bar_color),
                    ),
            )
            .into_any_element();
        self.popup_shell("composer-context-popup", content, true, cx)
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

    /// Command 面板的表面色：暗色主题下 popover 与窗口背景同为 #0a0a0a，
    /// 面板会糊在背景上像透明一样，覆盖为不透明 neutral-800（比输入框容器亮一档）；
    /// 亮色主题保持 popover。
    fn command_surface(cx: &App) -> Hsla {
        if cx.theme().is_dark() {
            hsla(0., 0., 0.15, 1.)
        } else {
            cx.theme().popover
        }
    }

    /// 弹层外壳：锚定在触发芯片正上方，点击外部关闭，带进入动画。
    /// `anchor_right` 时右对齐触发芯片（底部右侧芯片的弹层避免超出输入框右缘）。
    fn popup_shell(
        &self,
        id: &'static str,
        content: AnyElement,
        anchor_right: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id(id)
            .absolute()
            .bottom_full()
            .when(!anchor_right, |this| this.left_0())
            .when(anchor_right, |this| this.right_0())
            .mb_2()
            .w(px(360.))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.popup = None;
                cx.notify();
            }))
            .child(
                div()
                    .relative()
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
        command: Command,
        anchor_right: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.popup_shell(id, command.into_any_element(), anchor_right, cx)
    }

    /// 面板确认/取消的通用收尾：关闭弹层并回焦输入框。
    fn close_command_popup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.popup = None;
        self.input.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    /// 芯片点击开合弹层：outside-click 在 capture 阶段已先关掉面板时，
    /// 同一击不再重开（与渲染时状态一致才翻转）；打开时重置并聚焦对应 Command 面板。
    fn toggle_popup(
        &mut self,
        kind: Popup,
        render_open: bool,
        command: Option<Entity<CommandState>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let open_now = matches!(self.popup, Some((k, _)) if k == kind);
        if open_now != render_open {
            cx.notify();
            return;
        }
        if open_now {
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

        let mut command = Command::new(&self.cwd_command)
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
                        .label("不在项目中工作")
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
        command.style().background = Some(Self::command_surface(cx).into());

        self.command_popup_shell("composer-cwd-popup", command, false, cx)
    }

    /// 分支选择面板：Command 面板（搜索框 + 分支列表），锚定在分支芯片正上方。
    fn render_branch_popup(&self, cx: &mut Context<Self>) -> AnyElement {
        let on_confirm_composer = cx.entity();
        let on_cancel_composer = cx.entity();

        let mut command = Command::new(&self.branch_command)
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
        command.style().background = Some(Self::command_surface(cx).into());

        self.command_popup_shell("composer-branch-popup", command, false, cx)
    }

    /// 执行模式面板：无搜索框，每项带图标 + 描述，当前模式勾选。
    fn render_exec_mode_popup(&self, cx: &mut Context<Self>) -> AnyElement {
        let on_confirm_composer = cx.entity();
        let on_cancel_composer = cx.entity();

        let mut command = Command::new(&self.exec_command)
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
        command.style().background = Some(Self::command_surface(cx).into());

        self.command_popup_shell("composer-exec-popup", command, false, cx)
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
        let mut command = command
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
        command.style().background = Some(Self::command_surface(cx).into());

        self.command_popup_shell("composer-model-popup", command, true, cx)
    }

    /// 思考等级面板：无搜索框，「关闭」+ 等级列表，当前等级勾选。
    fn render_reasoning_popup(&self, levels: &[String], cx: &mut Context<Self>) -> AnyElement {
        let on_confirm_composer = cx.entity();
        let on_cancel_composer = cx.entity();
        let levels = levels.to_vec();

        let mut command = Command::new(&self.reasoning_command)
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
        command.style().background = Some(Self::command_surface(cx).into());

        self.command_popup_shell("composer-reasoning-popup", command, true, cx)
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
        // message-scroller 的输入框），比 Command 面板暗一档保持 窗口 < 输入框 < 面板 的层次。
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
                                                        move |this, _, window, cx| {
                                                            let command = this.cwd_command.clone();
                                                            this.toggle_popup(
                                                                Popup::Cwd,
                                                                cwd_open,
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
                                                                    move |this, _, window, cx| {
                                                                        let command = this
                                                                            .branch_command
                                                                            .clone();
                                                                        this.toggle_popup(
                                                                            Popup::Branch,
                                                                            branch_open,
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
                        .when(!self.attachments.is_empty(), |this| {
                            this.child(self.render_attachments(cx))
                        })
                        .when(self.approval_pending, |this| {
                            this.child(
                                h_flex()
                                    .gap_2()
                                    .text_xs()
                                    .text_color(cx.theme().warning)
                                    .child(
                                        "⏳ 等待审批：请在上方审批卡中选择 允许 / 始终允许 / 拒绝",
                                    ),
                            )
                        })
                        .child(
                            div()
                                .relative()
                                .w_full()
                                .child(Textarea::new(&self.input).appearance(false).bordered(false))
                                .children(popup),
                        )
                        .child(
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
                                            true,
                                            cx.listener(move |this, _, window, cx| {
                                                let command = this.exec_command.clone();
                                                this.toggle_popup(
                                                    Popup::ExecMode,
                                                    exec_open,
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
                                    // 上下文水位环形指示器（ZCode 同款）：点开是容量面板
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
                                                    .cursor_pointer()
                                                    .when(context_open, |this| {
                                                        this.bg(cx.theme().accent)
                                                    })
                                                    .hover(|this| this.bg(cx.theme().accent))
                                                    .on_click(cx.listener(
                                                        move |this, _, window, cx| {
                                                            this.toggle_popup(
                                                                Popup::Context,
                                                                context_open,
                                                                None,
                                                                window,
                                                                cx,
                                                            );
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
                                            cx.listener(move |this, _, window, cx| {
                                                let command = this.model_command.clone();
                                                this.toggle_popup(
                                                    Popup::Model,
                                                    model_open,
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
                                                    cx.listener(move |this, _, window, cx| {
                                                        let command =
                                                            this.reasoning_command.clone();
                                                        this.toggle_popup(
                                                            Popup::Reasoning,
                                                            reasoning_open,
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
                        ),
                ),
        )
    }
}
