use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use pig_protocol::ExecMode;

const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/clear", "清空当前会话消息"),
    ("/compact", "压缩上下文（演示）"),
];

pub const PLACEHOLDER_IDLE: &str = "向 pig-code 提问，使用 @ 添加上下文，使用 / 选择命令";
pub const PLACEHOLDER_STREAMING: &str = "继续输入以排队后续修改";

const EXEC_MODES: &[(&str, ExecMode)] = &[
    ("变更前确认", ExecMode::ConfirmBeforeEdit),
    ("自动编辑", ExecMode::AutoEdit),
    ("计划模式", ExecMode::Plan),
    ("完全访问", ExecMode::FullAccess),
];
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
    SetModel { provider_id: String, model_id: String },
    SetReasoning(Option<String>),
    OpenSettings,
    SetExecMode(ExecMode),
    SearchFiles(String),
    /// hero：打开系统目录选择器
    PickDirectory,
    /// hero：选择最近目录
    SelectCwd(String),
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
    hero_cwd_label: String,
    hero_branch: Option<String>,
    hero_branches: Vec<String>,
    hero_is_git: bool,
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
            hero_cwd_label: String::new(),
            hero_branch: None,
            hero_branches: Vec::new(),
            hero_is_git: false,
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
        cwd_label: String,
        cwds: Vec<String>,
        branch: Option<String>,
        branches: Vec<String>,
        is_git: bool,
        cx: &mut Context<Self>,
    ) {
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
            mode: EXEC_MODES[self.exec_mode].1,
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
        if let Some(ix) = EXEC_MODES.iter().position(|(_, m)| *m == mode) {
            self.exec_mode = ix;
        }
        cx.notify();
    }

    /// 自测用。
    pub fn debug_exec_mode(&self) -> ExecMode {
        EXEC_MODES[self.exec_mode].1
    }

    fn render_water_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (used, total) = self.context_usage?;
        let ratio = (used as f32 / total as f32).clamp(0.0, 1.0);
        let warn = ratio > 0.8;
        let bar_color = if warn {
            cx.theme().warning
        } else {
            cx.theme().success
        };
        Some(
            h_flex()
                .w_full()
                .gap_2()
                .items_center()
                .child(
                    div()
                        .flex_1()
                        .h(px(4.))
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
                .child(
                    div()
                        .text_xs()
                        .text_color(if warn {
                            cx.theme().warning
                        } else {
                            cx.theme().muted_foreground
                        })
                        .child(format!(
                            "上下文 {:.1}k / {}k（{}%）",
                            used as f64 / 1000.0,
                            total / 1000,
                            (ratio * 100.0) as u32
                        )),
                )
                .into_any_element(),
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
            Popup::ExecMode => EXEC_MODES
                .iter()
                .enumerate()
                .map(|(ix, (label, _))| {
                    self.render_list_item(
                        ("exec-mode", ix),
                        if ix == self.exec_mode {
                            IconName::Check
                        } else {
                            IconName::Dash
                        },
                        label.to_string(),
                        None,
                        cx.listener(move |this, _, _, cx| {
                            this.exec_mode = ix;
                            this.popup = None;
                            cx.emit(ComposerEvent::SetExecMode(EXEC_MODES[ix].1));
                            cx.notify();
                        }),
                        cx,
                    )
                })
                .collect(),
            Popup::Cwd => {
                let mut items: Vec<AnyElement> = self
                    .hero_cwds
                    .iter()
                    .enumerate()
                    .map(|(ix, cwd)| {
                        let cwd = cwd.clone();
                        self.render_list_item(
                            ("cwd", ix),
                            IconName::FolderOpen,
                            cwd.clone(),
                            None,
                            cx.listener(move |this, _, _, cx| {
                                this.popup = None;
                                cx.emit(ComposerEvent::SelectCwd(cwd.clone()));
                            }),
                            cx,
                        )
                    })
                    .collect();
                items.push(self.render_list_item(
                    "cwd-browse",
                    IconName::Ellipsis,
                    "浏览文件夹…".to_string(),
                    None,
                    cx.listener(|this, _, _, cx| {
                        this.popup = None;
                        cx.emit(ComposerEvent::PickDirectory);
                    }),
                    cx,
                ));
                items
            }
            Popup::Branch => self
                .hero_branches
                .iter()
                .enumerate()
                .map(|(ix, branch)| {
                    let branch = branch.clone();
                    let current = Some(&branch) == self.hero_branch.as_ref();
                    self.render_list_item(
                        ("branch", ix),
                        if current { IconName::Check } else { IconName::Dash },
                        branch.clone(),
                        None,
                        cx.listener(move |this, _, _, cx| {
                            this.popup = None;
                            cx.emit(ComposerEvent::CheckoutBranch(branch.clone()));
                        }),
                        cx,
                    )
                })
                .collect(),
            Popup::Model if self.models.is_empty() => vec![self.render_list_item(
                "model-empty",
                IconName::Info,
                "还没有配置模型，去设置页添加 →".to_string(),
                None,
                cx.listener(|_, _, _, cx| {
                    cx.emit(ComposerEvent::OpenSettings);
                }),
                cx,
            )],
            Popup::Model => self
                .models
                .iter()
                .enumerate()
                .map(|(ix, (provider_name, provider_id, model_id, _))| {
                    let selected = self.model == format!("{provider_name}/{model_id}");
                    let label = format!("{provider_name}/{model_id}");
                    let (provider_id, model_id) = (provider_id.clone(), model_id.clone());
                    let detail = provider_name.clone();
                    self.render_list_item(
                        ("model", ix),
                        if selected { IconName::Check } else { IconName::Dash },
                        model_id.clone(),
                        Some(detail),
                        cx.listener(move |this, _, _, cx| {
                            this.model = label.clone();
                            this.popup = None;
                            cx.emit(ComposerEvent::SetModel {
                                provider_id: provider_id.clone(),
                                model_id: model_id.clone(),
                            });
                            cx.notify();
                        }),
                        cx,
                    )
                })
                .collect(),
            Popup::Reasoning => {
                let levels: Vec<String> = self
                    .models
                    .iter()
                    .find(|(_, _, model_id, _)| self.model.ends_with(&format!("/{model_id}")))
                    .map(|(_, _, _, levels)| levels.clone())
                    .unwrap_or_default();
                let mut items: Vec<AnyElement> = vec![self.render_list_item(
                    "reasoning-off",
                    if self.reasoning_level.is_none() {
                        IconName::Check
                    } else {
                        IconName::Dash
                    },
                    "关闭".to_string(),
                    None,
                    cx.listener(|this, _, _, cx| {
                        this.reasoning_level = None;
                        this.popup = None;
                        cx.emit(ComposerEvent::SetReasoning(None));
                        cx.notify();
                    }),
                    cx,
                )];
                items.extend(levels.iter().enumerate().map(|(ix, level)| {
                    let level = level.clone();
                    let selected = self.reasoning_level.as_deref() == Some(level.as_str());
                    self.render_list_item(
                        ("reasoning", ix),
                        if selected { IconName::Check } else { IconName::Dash },
                        level.clone(),
                        None,
                        cx.listener(move |this, _, _, cx| {
                            this.reasoning_level = Some(level.clone());
                            this.popup = None;
                            cx.emit(ComposerEvent::SetReasoning(Some(level.clone())));
                            cx.notify();
                        }),
                        cx,
                    )
                }));
                items
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
        let popup = self.render_popup(cx);

        div().w_full().p_3().child(
            v_flex()
                .w_full()
                .max_w(px(860.))
                .mx_auto()
                .gap_2()
                .px_3()
                .py_2()
                .rounded(cx.theme().radius)
                .border_1()
                .border_color(cx.theme().border)
                .bg(cx.theme().popover)
                .children(self.render_water_bar(cx))
                .when(self.hero_mode, |this| {
                    this.child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .pb_1()
                            .border_b_1()
                            .border_color(cx.theme().border)
                            .child(
                                Button::new("hero-cwd")
                                    .outline()
                                    .small()
                                    .icon(IconName::FolderOpen)
                                    .label(self.hero_cwd_label.clone())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.popup = match this.popup {
                                            Some((Popup::Cwd, _)) => None,
                                            _ => Some((Popup::Cwd, 0)),
                                        };
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("hero-branch")
                                    .outline()
                                    .small()
                                    .icon(IconName::Github)
                                    .label(if self.hero_is_git {
                                        self.hero_branch.clone().unwrap_or_else(|| "?".into())
                                    } else {
                                        "非 git 仓库".to_string()
                                    })
                                    .when(!self.hero_is_git, |this| this.disabled(true))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.popup = match this.popup {
                                            Some((Popup::Branch, _)) => None,
                                            _ => Some((Popup::Branch, 0)),
                                        };
                                        cx.notify();
                                    })),
                            ),
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
                            .child("⏳ 等待审批：请在上方审批卡中选择 允许 / 始终允许 / 拒绝"),
                    )
                })
                .child(
                    div()
                        .relative()
                        .w_full()
                        .child(
                            Textarea::new(&self.input)
                                .appearance(false)
                                .bordered(false),
                        )
                        .children(popup),
                )
                .child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .child(
                            Button::new("exec-mode")
                                .outline()
                                .small()
                                .label(format!("执行模式: {}", EXEC_MODES[self.exec_mode].0))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.popup = match this.popup {
                                        Some((Popup::ExecMode, _)) => None,
                                        _ => Some((Popup::ExecMode, 0)),
                                    };
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("model-picker")
                                .outline()
                                .small()
                                .label(format!("模型: {}", self.model))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.popup = match this.popup {
                                        Some((Popup::Model, _)) => None,
                                        _ => Some((Popup::Model, 0)),
                                    };
                                    cx.notify();
                                })),
                        )
                        .when(
                            self.models
                                .iter()
                                .find(|(_, _, model_id, _)| {
                                    self.model.ends_with(&format!("/{model_id}"))
                                })
                                .is_some_and(|(_, _, _, levels)| !levels.is_empty()),
                            |this| {
                                this.child(
                                    Button::new("reasoning-picker")
                                        .outline()
                                        .small()
                                        .label(format!(
                                            "思考: {}",
                                            self.reasoning_level.as_deref().unwrap_or("关")
                                        ))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.popup = match this.popup {
                                                Some((Popup::Reasoning, _)) => None,
                                                _ => Some((Popup::Reasoning, 0)),
                                            };
                                            cx.notify();
                                        })),
                                )
                            },
                        )
                        .child(div().flex_1())
                        .when(self.streaming, |this| {
                            this.child(
                                Button::new("stop")
                                    .danger()
                                    .small()
                                    .label("停止 ■")
                                    .on_click(cx.listener(|_, _: &ClickEvent, _, cx| {
                                        cx.emit(ComposerEvent::Stop);
                                    })),
                            )
                        })
                        .when(!self.streaming, |this| {
                            this.child(
                                Button::new("send")
                                    .primary()
                                    .small()
                                    .label("发送 ▶")
                                    .on_click(cx.listener(
                                        |this, _: &ClickEvent, window, cx| {
                                            this.send(window, cx);
                                        },
                                    )),
                            )
                        }),
                ),
        )
    }
}
