use gpui_kit::component::ThemeMode;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _, h_flex,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use pig_protocol::{ApiFormat, AppConfig, ModelConfig, ProviderConfig};

#[derive(Clone)]
pub enum SettingsEvent {
    Save(AppConfig),
    TestProvider(String),
    Close,
}

impl EventEmitter<SettingsEvent> for SettingsView {}

pub struct ModelDialog {
    /// None = 新增模型
    editing: Option<usize>,
    id: Entity<InputState>,
    context_window: Entity<InputState>,
    max_tokens: Entity<InputState>,
    advanced_open: bool,
    input_image: bool,
    input_video: bool,
    input_pdf: bool,
    cap_structured: bool,
    cap_web_search: bool,
    cap_system_msg: bool,
    enabled: bool,
    reasoning_levels: Vec<String>,
    /// 等级 id → 显示名（仅展示；随 chip 增删联动）
    reasoning_labels: std::collections::HashMap<String, String>,
    new_level: Entity<InputState>,
    new_label: Entity<InputState>,
    params_json: Entity<TextareaState>,
    params_error: Option<String>,
    snapshot: Option<ModelConfig>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SettingsPage {
    General,
    Appearance,
    Models,
    BrowserControl,
    ComputerControl,
    Shortcuts,
    Memory,
    Subagents,
    Plugins,
    Mcp,
    Skills,
    Commands,
    Hooks,
    UsageStats,
    Onboarding,
}

/// (分组, [(页面, 图标, 名称)])——加页面只改这一处
const NAV: &[(&str, &[(SettingsPage, IconName, &str)])] = &[
    (
        "基础设置",
        &[
            (SettingsPage::General, IconName::Settings, "常规"),
            (SettingsPage::Appearance, IconName::Palette, "外观"),
            (SettingsPage::Models, IconName::Bot, "模型设置"),
        ],
    ),
    (
        "Agent 能力",
        &[
            (SettingsPage::BrowserControl, IconName::Globe, "浏览器控制"),
            (SettingsPage::ComputerControl, IconName::Cpu, "电脑控制"),
            (SettingsPage::Shortcuts, IconName::Menu, "键盘快捷键"),
            (SettingsPage::Memory, IconName::MemoryStick, "记忆"),
            (SettingsPage::Subagents, IconName::Bot, "子智能体"),
            (SettingsPage::Plugins, IconName::Folder, "插件"),
            (SettingsPage::Mcp, IconName::Network, "MCP 服务器"),
            (SettingsPage::Skills, IconName::BookOpen, "技能"),
            (SettingsPage::Commands, IconName::SquareTerminal, "命令"),
            (SettingsPage::Hooks, IconName::Bell, "钩子"),
        ],
    ),
    (
        "数据与统计",
        &[
            (SettingsPage::UsageStats, IconName::ChartPie, "使用统计"),
            (SettingsPage::Onboarding, IconName::BookOpen, "引导"),
        ],
    ),
];

fn page_title(page: SettingsPage) -> &'static str {
    NAV.iter()
        .flat_map(|(_, items)| items.iter())
        .find(|(p, _, _)| *p == page)
        .map(|(_, _, label)| *label)
        .unwrap_or("")
}

pub struct SettingsView {
    page: SettingsPage,
    config: AppConfig,
    selected: Option<usize>,
    name_input: Entity<InputState>,
    base_url_input: Entity<InputState>,
    api_key_input: Entity<InputState>,
    api_key_masked: bool,
    delete_armed: bool,
    format_popup: bool,
    test_results: std::collections::HashMap<String, (bool, String)>,
    model_dialog: Option<ModelDialog>,
    save_generation: u64,
    form_dirty: bool,
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
        Self {
            page: SettingsPage::Models,
            config: AppConfig::default(),
            selected: None,
            name_input,
            base_url_input,
            api_key_input,
            api_key_masked: true,
            delete_armed: false,
            format_popup: false,
            test_results: Default::default(),
            model_dialog: None,
            save_generation: 0,
            form_dirty: true,
            _subscriptions,
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
        ok: bool,
        message: String,
        cx: &mut Context<Self>,
    ) {
        self.test_results
            .insert(provider_id.to_string(), (ok, message));
        cx.notify();
    }

    #[allow(dead_code)]
    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    fn selected_provider(&self) -> Option<&ProviderConfig> {
        self.config.providers.get(self.selected?)
    }

    /// 表单回填需要 window（set_value），标记后到 render 时应用
    fn sync_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(provider) = self.selected_provider().cloned() else {
            return;
        };
        let name = provider.name.clone();
        let base_url = provider.base_url.clone();
        let api_key = provider.api_key.clone();
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

    /// 表单变更 500ms 防抖后自动保存
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

    /// 把表单值写回 config 并发 Save。
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

    fn add_provider(&mut self, cx: &mut Context<Self>) {
        let ix = self.config.providers.len();
        self.config.providers.push(ProviderConfig {
            id: format!("custom-{}", ix + 1),
            name: "自定义供应商".into(),
            base_url: "https://".into(),
            api_key: String::new(),
            api_format: ApiFormat::OpenAiChat,
            enabled: true,
            models: vec![],
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

    // ---------- 模型弹窗 ----------

    fn open_model_dialog(
        &mut self,
        editing: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let model = editing.and_then(|ix| {
            self.selected_provider()
                .and_then(|p| p.models.get(ix))
                .cloned()
        });
        let snapshot = model.clone();
        let model = model.unwrap_or_else(|| ModelConfig::new("", 128_000, 8_192));
        let dialog = ModelDialog {
            editing,
            id: cx.new(|cx| InputState::new(window, cx).default_value(model.id.clone())),
            context_window: cx.new(|cx| {
                InputState::new(window, cx).default_value(model.context_window.to_string())
            }),
            max_tokens: cx.new(|cx| {
                InputState::new(window, cx).default_value(model.max_output_tokens.to_string())
            }),
            advanced_open: false,
            input_image: model.input_image,
            input_video: model.input_video,
            input_pdf: model.input_pdf,
            cap_structured: model.cap_structured,
            cap_web_search: model.cap_web_search,
            cap_system_msg: model.cap_system_msg,
            enabled: model.enabled,
            reasoning_levels: model.reasoning_levels.clone(),
            reasoning_labels: model.reasoning_labels.clone(),
            new_level: cx.new(|cx| InputState::new(window, cx).placeholder("等级名，如 high")),
            new_label: cx.new(|cx| InputState::new(window, cx).placeholder("显示名（可选），如 最高")),
            params_json: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(3, 8)
                    .default_value(if model.reasoning_params.is_empty() {
                        "{}".to_string()
                    } else {
                        serde_json::to_string_pretty(&model.reasoning_params).unwrap_or_default()
                    })
            }),
            params_error: None,
            snapshot,
        };
        self.model_dialog = Some(dialog);
        cx.notify();
    }

    fn save_model_dialog(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.model_dialog.take() else {
            return;
        };
        let Some(p_ix) = self.selected else { return };

        let id = dialog.id.read(cx).value().trim().to_string();
        let context_window = dialog.context_window.read(cx).value().trim().parse::<u64>();
        let max_tokens = dialog.max_tokens.read(cx).value().trim().parse::<u64>();
        let params_raw = dialog.params_json.read(cx).value().to_string();
        let params: Result<
            std::collections::HashMap<String, serde_json::Value>,
            serde_json::Error,
        > = serde_json::from_str(params_raw.trim());

        let error = if id.is_empty() {
            Some("模型 ID 不能为空".to_string())
        } else if context_window.is_err() || context_window.as_ref().ok() == Some(&0) {
            Some("上下文窗口必须是正整数".to_string())
        } else if max_tokens.is_err() || max_tokens.as_ref().ok() == Some(&0) {
            Some("最大输出 Token 必须是正整数".to_string())
        } else if let Err(e) = &params {
            Some(format!("推理参数映射不是合法 JSON 对象: {e}"))
        } else {
            None
        };
        if let Some(error) = error {
            let mut dialog = dialog;
            dialog.params_error = Some(error);
            self.model_dialog = Some(dialog);
            cx.notify();
            return;
        }

        let model = ModelConfig {
            id,
            enabled: dialog.enabled,
            context_window: context_window.unwrap(),
            max_output_tokens: max_tokens.unwrap(),
            input_image: dialog.input_image,
            input_video: dialog.input_video,
            input_pdf: dialog.input_pdf,
            cap_structured: dialog.cap_structured,
            cap_web_search: dialog.cap_web_search,
            // 无设置 UI：编辑时保留手配的原值，新建为 None
            web_search_tool: dialog
                .snapshot
                .as_ref()
                .and_then(|m| m.web_search_tool.clone()),
            cap_system_msg: dialog.cap_system_msg,
            reasoning_levels: dialog.reasoning_levels.clone(),
            // 显示名只保留仍存在的等级 id（防御chip外路径改列表）
            reasoning_labels: dialog
                .reasoning_labels
                .iter()
                .filter(|(id, label)| {
                    dialog.reasoning_levels.contains(*id) && !label.is_empty()
                })
                .map(|(id, label)| (id.clone(), label.clone()))
                .collect(),
            reasoning_params: params.unwrap(),
        };
        let provider = &mut self.config.providers[p_ix];
        match dialog.editing {
            Some(ix) => provider.models[ix] = model,
            None => provider.models.push(model),
        }
        cx.emit(SettingsEvent::Save(self.config.clone()));
        cx.notify();
    }
}
impl SettingsView {
    fn render_provider_row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let provider = &self.config.providers[ix];
        let selected = self.selected == Some(ix);
        let dot_color = if provider.enabled {
            cx.theme().success
        } else {
            cx.theme().muted_foreground
        };
        h_flex()
            .id(("provider", ix))
            .gap_2()
            .px_3()
            .py_2()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(if selected {
                cx.theme().primary
            } else {
                cx.theme().border
            })
            .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.select_provider(ix, cx);
            }))
            .child(div().size_2().rounded_full().bg(dot_color))
            .child(div().text_sm().flex_1().child(provider.name.clone()))
            .into_any_element()
    }

    fn render_detail(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(p_ix) = self.selected else {
            return div()
                .flex_1()
                .child("选择或添加一个供应商")
                .into_any_element();
        };
        let provider = self.config.providers[p_ix].clone();
        let provider_id = provider.id.clone();
        let enabled = provider.enabled;
        let is_anthropic = provider.api_format == ApiFormat::AnthropicMessages;
        let test_result = self.test_results.get(&provider_id).cloned();

        v_flex()
            .flex_1()
            .gap_3()
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(
                        div()
                            .text_lg()
                            .font_semibold()
                            .flex_1()
                            .child(provider.name.clone()),
                    )
                    .child(
                        Switch::new("provider-enabled")
                            .checked(enabled)
                            .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                this.config.providers[p_ix].enabled = *checked;
                                cx.emit(SettingsEvent::Save(this.config.clone()));
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("delete-provider")
                            .ghost()
                            .small()
                            .label(if self.delete_armed { "确认删除？" } else { "删除" })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if this.delete_armed {
                                    this.config.providers.remove(p_ix);
                                    this.selected = None;
                                    this.delete_armed = false;
                                    this.form_dirty = true;
                                    cx.emit(SettingsEvent::Save(this.config.clone()));
                                } else {
                                    this.delete_armed = true;
                                }
                                cx.notify();
                            })),
                    ),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(div().text_xs().text_color(cx.theme().muted_foreground).child("名称"))
                    .child(Input::new(&self.name_input)),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(div().text_xs().text_color(cx.theme().muted_foreground).child("Base URL"))
                    .child(Input::new(&self.base_url_input)),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(div().text_xs().text_color(cx.theme().muted_foreground).child("API 格式"))
                    .child(
                        div().relative().child(
                            Button::new("api-format")
                                .outline()
                                .w_full()
                                .label(if is_anthropic {
                                    "Anthropic Messages (/v1/messages)"
                                } else {
                                    "OpenAI Chat Completions (/v1/chat/completions)"
                                })
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.format_popup = !this.format_popup;
                                    cx.notify();
                                })),
                        )
                        .when(self.format_popup, |this| {
                            this.child(
                                div()
                                    .id("format-popup")
                                    .absolute()
                                    .top_full()
                                    .left_0()
                                    .right_0()
                                    .mt_1()
                                    .rounded(cx.theme().radius)
                                    .border_1()
                                    .border_color(cx.theme().border)
                                    .bg(cx.theme().popover)
                                    .py_1()
                                    .children(
                                        [
                                            ("OpenAI Chat Completions (/v1/chat/completions)", ApiFormat::OpenAiChat),
                                            ("Anthropic Messages (/v1/messages)", ApiFormat::AnthropicMessages),
                                        ]
                                        .map(|(label, format)| {
                                            div()
                                                .id(gpui_kit::SharedString::from(label.to_string()))
                                                .px_3()
                                                .py_1()
                                                .text_sm()
                                                .cursor_pointer()
                                                .hover(|this| this.bg(cx.theme().accent))
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.config.providers[p_ix].api_format = format;
                                                    this.format_popup = false;
                                                    cx.emit(SettingsEvent::Save(this.config.clone()));
                                                    cx.notify();
                                                }))
                                                .child(label)
                                        }),
                                    ),
                            )
                        }),
                    ),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(div().text_xs().text_color(cx.theme().muted_foreground).child("API Key"))
                    .child(
                        h_flex()
                            .gap_1()
                            .child(div().flex_1().child(Input::new(&self.api_key_input)))
                            .child(
                                Button::new("toggle-mask")
                                    .ghost()
                                    .small()
                                    .icon(if self.api_key_masked {
                                        IconName::EyeOff
                                    } else {
                                        IconName::Eye
                                    })
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.api_key_masked = !this.api_key_masked;
                                        let masked = this.api_key_masked;
                                        this.api_key_input.update(cx, |i, cx| {
                                            i.set_masked(masked, window, cx);
                                        });
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .when_some(test_result, |this, (ok, message)| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(if ok { cx.theme().success } else { cx.theme().danger })
                        .child(message),
                )
            })
            .child(
                h_flex()
                    .w_full()
                    .mt_2()
                    .child(
                        div().text_sm().font_semibold().flex_1().child("模型列表"),
                    )
                    .child(
                        Button::new("add-model")
                            .ghost()
                            .small()
                            .icon(IconName::Plus)
                            .label("添加模型")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_model_dialog(None, window, cx);
                            })),
                    ),
            )
            .children(provider.models.iter().enumerate().map(|(m_ix, model)| {
                let provider_id = provider_id.clone();
                h_flex()
                    .id(("model", m_ix))
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded(cx.theme().radius)
                    .border_1()
                    .border_color(cx.theme().border)
                    .child(div().text_sm().child(model.id.clone()))
                    .child(
                        div()
                            .text_xs()
                            .px_1()
                            .rounded_sm()
                            .bg(cx.theme().accent)
                            .child(Self::format_ctx(model.context_window)),
                    )
                    .when(model.input_image, |this| {
                        this.child(
                            div()
                                .text_xs()
                                .px_1()
                                .rounded_sm()
                                .bg(cx.theme().accent)
                                .child("视觉"),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        Button::new(("test", m_ix))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Globe)
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(SettingsEvent::TestProvider(provider_id.clone()));
                            })),
                    )
                    .child(
                        Button::new(("edit-model", m_ix))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Settings2)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_model_dialog(Some(m_ix), window, cx);
                            })),
                    )
                    .child(
                        Button::new(("del-model", m_ix))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Delete)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.config.providers[p_ix].models.remove(m_ix);
                                cx.emit(SettingsEvent::Save(this.config.clone()));
                                cx.notify();
                            })),
                    )
                    .child(
                        Switch::new(("model-enabled", m_ix))
                            .checked(model.enabled)
                            .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                this.config.providers[p_ix].models[m_ix].enabled = *checked;
                                cx.emit(SettingsEvent::Save(this.config.clone()));
                                cx.notify();
                            })),
                    )
            }))
            .into_any_element()
    }
}

impl SettingsView {
    fn render_model_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(dialog) = &self.model_dialog else {
            return div().into_any_element();
        };
        let snapshot_exists = dialog.snapshot.is_some();

        div()
            .absolute()
            .inset_0()
            .bg(gpui_kit::black().opacity(0.5))
            .flex()
            .items_center()
            .justify_center()
            .child(
                v_flex()
                    .id("model-dialog")
                    .w(px(560.))
                    .max_h(px(640.))
                    .overflow_y_scroll()
                    .gap_3()
                    .p_4()
                    .rounded(cx.theme().radius_lg)
                    .bg(cx.theme().popover)
                    .border_1()
                    .border_color(cx.theme().border)
                    .child(
                        div()
                            .text_lg()
                            .font_semibold()
                            .child(if snapshot_exists { "编辑模型配置" } else { "添加模型" }),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(div().text_xs().text_color(cx.theme().muted_foreground).child("模型 ID"))
                            .child(Input::new(&dialog.id)),
                    )
                    .child(
                        h_flex()
                            .gap_3()
                            .child(
                                v_flex()
                                    .flex_1()
                                    .gap_1()
                                    .child(div().text_xs().text_color(cx.theme().muted_foreground).child("上下文窗口"))
                                    .child(Input::new(&dialog.context_window)),
                            )
                            .child(
                                v_flex()
                                    .flex_1()
                                    .gap_1()
                                    .child(div().text_xs().text_color(cx.theme().muted_foreground).child("最大输出 Token"))
                                    .child(Input::new(&dialog.max_tokens)),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().text_sm().flex_1().child("启用该模型"))
                            .child(
                                Switch::new("model-enabled-dialog")
                                    .checked(dialog.enabled)
                                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                        if let Some(dialog) = &mut this.model_dialog {
                                            dialog.enabled = *checked;
                                        }
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(
                        v_flex()
                            .gap_2()
                            .child(
                                h_flex()
                                    .id("advanced-toggle")
                                    .gap_2()
                                    .cursor_pointer()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if let Some(dialog) = &mut this.model_dialog {
                                            dialog.advanced_open = !dialog.advanced_open;
                                        }
                                        cx.notify();
                                    }))
                                    .child(
                                        Icon::new(if dialog.advanced_open {
                                            IconName::ChevronDown
                                        } else {
                                            IconName::ChevronRight
                                        })
                                        .size_4()
                                        .text_color(cx.theme().muted_foreground),
                                    )
                                    .child(div().text_sm().child("高级配置")),
                            )
                            .when(dialog.advanced_open, |this| {
                                this.child(
                                    v_flex()
                                        .gap_2()
                                        .child(
                                            h_flex()
                                                .gap_3()
                                                .child(Checkbox::new("in-image").label("图片").checked(dialog.input_image).on_click(cx.listener(|this, v: &bool, _, cx| { if let Some(d) = &mut this.model_dialog { d.input_image = *v; } cx.notify(); })))
                                                .child(Checkbox::new("in-video").label("视频").checked(dialog.input_video).on_click(cx.listener(|this, v: &bool, _, cx| { if let Some(d) = &mut this.model_dialog { d.input_video = *v; } cx.notify(); })))
                                                .child(Checkbox::new("in-pdf").label("PDF").checked(dialog.input_pdf).on_click(cx.listener(|this, v: &bool, _, cx| { if let Some(d) = &mut this.model_dialog { d.input_pdf = *v; } cx.notify(); }))),
                                        )
                                        .child(
                                            h_flex()
                                                .gap_3()
                                                .child(Checkbox::new("cap-struct").label("结构化输出").checked(dialog.cap_structured).on_click(cx.listener(|this, v: &bool, _, cx| { if let Some(d) = &mut this.model_dialog { d.cap_structured = *v; } cx.notify(); })))
                                                .child(Checkbox::new("cap-web").label("原生联网搜索").checked(dialog.cap_web_search).on_click(cx.listener(|this, v: &bool, _, cx| { if let Some(d) = &mut this.model_dialog { d.cap_web_search = *v; } cx.notify(); })))
                                                .child(Checkbox::new("cap-sys").label("对话中系统消息").checked(dialog.cap_system_msg).on_click(cx.listener(|this, v: &bool, _, cx| { if let Some(d) = &mut this.model_dialog { d.cap_system_msg = *v; } cx.notify(); }))),
                                        )
                                        .child(
                                            v_flex()
                                                .gap_1()
                                                .child(div().text_xs().text_color(cx.theme().muted_foreground).child("推理等级"))
                                                .child(
                                                    h_flex()
                                                        .gap_1()
                                                        .children(dialog.reasoning_levels.iter().enumerate().map(|(ix, level)| {
                                                            let label = dialog.reasoning_labels.get(level).cloned();
                                                            let level_owned = level.clone();
                                                            h_flex()
                                                                .gap_1()
                                                                .px_2()
                                                                .py_0p5()
                                                                .rounded_full()
                                                                .bg(cx.theme().accent)
                                                                // 点 chip 文本：回填到输入行编辑（从列表移除，点 + 重新加入）
                                                                .child(
                                                                    div()
                                                                        .id(("edit-level", ix))
                                                                        .cursor_pointer()
                                                                        .on_click(cx.listener(move |this, _, window, cx| {
                                                                            if let Some(d) = &mut this.model_dialog {
                                                                                let label = d.reasoning_labels.remove(&level_owned).unwrap_or_default();
                                                                                d.reasoning_levels.remove(ix);
                                                                                d.new_level.update(cx, |i, cx| i.set_value(level_owned.clone(), window, cx));
                                                                                d.new_label.update(cx, |i, cx| i.set_value(label, window, cx));
                                                                            }
                                                                            cx.notify();
                                                                        }))
                                                                        .child(
                                                                            h_flex()
                                                                                .gap_1()
                                                                                .child(div().text_xs().child(level.clone()))
                                                                                .when_some(label, |this, label| {
                                                                                    this.child(
                                                                                        div()
                                                                                            .text_xs()
                                                                                            .text_color(cx.theme().muted_foreground)
                                                                                            .child(format!("· {label}")),
                                                                                    )
                                                                                }),
                                                                        ),
                                                                )
                                                                .child(
                                                                    div()
                                                                        .id(("del-level", ix))
                                                                        .cursor_pointer()
                                                                        .on_click(cx.listener(move |this, _, _, cx| {
                                                                            if let Some(d) = &mut this.model_dialog {
                                                                                let level = d.reasoning_levels.remove(ix);
                                                                                d.reasoning_labels.remove(&level);
                                                                            }
                                                                            cx.notify();
                                                                        }))
                                                                        .child(Icon::new(IconName::Close).size_3()),
                                                                )
                                                        }))
                                                        .child(
                                                            h_flex()
                                                                .gap_1()
                                                                .child(div().w(px(90.)).child(Input::new(&dialog.new_level).small()))
                                                                .child(div().w(px(110.)).child(Input::new(&dialog.new_label).small()))
                                                                .child(
                                                                    Button::new("add-level")
                                                                        .ghost()
                                                                        .xsmall()
                                                                        .icon(IconName::Plus)
                                                                        .on_click(cx.listener(|this, _, window, cx| {
                                                                            let (level, label) = this.model_dialog.as_ref().map(|d| {
                                                                                (
                                                                                    d.new_level.read(cx).value().trim().to_string(),
                                                                                    d.new_label.read(cx).value().trim().to_string(),
                                                                                )
                                                                            }).unwrap_or_default();
                                                                            if level.is_empty() { return; }
                                                                            if let Some(d) = &mut this.model_dialog {
                                                                                if !d.reasoning_levels.contains(&level) {
                                                                                    d.reasoning_levels.push(level.clone());
                                                                                }
                                                                                if label.is_empty() {
                                                                                    d.reasoning_labels.remove(&level);
                                                                                } else {
                                                                                    d.reasoning_labels.insert(level, label);
                                                                                }
                                                                                d.new_level.update(cx, |i, cx| i.set_value("", window, cx));
                                                                                d.new_label.update(cx, |i, cx| i.set_value("", window, cx));
                                                                            }
                                                                            cx.notify();
                                                                        })),
                                                                ),
                                                        ),
                                                ),
                                        )
                                        .child(
                                            v_flex()
                                                .gap_1()
                                                .child(div().text_xs().text_color(cx.theme().muted_foreground).child("推理参数映射（JSON：等级 → 请求体合并参数）"))
                                                .child(Textarea::new(&dialog.params_json))
                                                .when_some(dialog.params_error.clone(), |this, error| {
                                                    this.child(
                                                        div()
                                                            .text_xs()
                                                            .text_color(cx.theme().danger)
                                                            .child(error),
                                                    )
                                                }),
                                        ),
                                )
                            }),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("reset-dialog")
                                    .ghost()
                                    .small()
                                    .label("重置表单")
                                    .disabled(!snapshot_exists)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        let snapshot = this.model_dialog.as_ref().and_then(|d| d.snapshot.clone());
                                        let editing = this.model_dialog.as_ref().and_then(|d| d.editing);
                                        if snapshot.is_some() {
                                            this.open_model_dialog(editing, window, cx);
                                        }
                                        cx.notify();
                                    })),
                            )
                            .child(div().flex_1())
                            .child(
                                Button::new("cancel-dialog")
                                    .outline()
                                    .small()
                                    .label("取消")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.model_dialog = None;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("save-dialog")
                                    .primary()
                                    .small()
                                    .label("保存")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.save_model_dialog(cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }
}

impl SettingsView {
    fn render_nav(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut nav = v_flex()
            .w(px(220.))
            .h_full()
            .gap_1()
            .p_3()
            .border_r_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .id("back-to-workspace")
                    .gap_2()
                    .px_2()
                    .py_1()
                    .mb_2()
                    .rounded(cx.theme().radius)
                    .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
                    .on_click(cx.listener(|_, _, _, cx| {
                        cx.emit(SettingsEvent::Close);
                    }))
                    .child(Icon::new(IconName::ArrowLeft).size_4())
                    .child(div().text_sm().child("返回工作区")),
            );
        for (group, items) in NAV {
            nav = nav
                .child(
                    div()
                        .px_2()
                        .py_1()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(*group),
                )
                .children(items.iter().map(|(page, icon, label)| {
                    let selected = self.page == *page;
                    h_flex()
                        .id(gpui_kit::SharedString::from(label.to_string()))
                        .gap_2()
                        .px_2()
                        .py_1()
                        .rounded(cx.theme().radius)
                        .cursor_pointer()
                        .when(selected, |this| this.bg(cx.theme().accent))
                        .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.page = *page;
                            cx.notify();
                        }))
                        .child(
                            Icon::new(icon.clone())
                                .size_4()
                                .text_color(cx.theme().muted_foreground),
                        )
                        .child(div().text_sm().child(*label))
                }));
        }
        nav.into_any_element()
    }

    fn render_appearance(&self, cx: &mut Context<Self>) -> AnyElement {
        let follow_system = cx
            .try_global::<crate::ThemeFollowSystem>()
            .is_some_and(|flag| flag.0);
        let current_dark = cx.theme().mode.is_dark();
        h_flex()
            .gap_4()
            .children(
                [
                    (ThemeMode::Light, "亮色", IconName::Sun),
                    (ThemeMode::Dark, "暗色", IconName::Moon),
                ]
                .map(|(mode, label, icon)| {
                    let selected = !follow_system && mode.is_dark() == current_dark;
                    v_flex()
                        .id(gpui_kit::SharedString::from(label.to_string()))
                        .gap_2()
                        .w(px(180.))
                        .p_4()
                        .items_center()
                        .rounded(cx.theme().radius_lg)
                        .border_2()
                        .border_color(if selected {
                            cx.theme().primary
                        } else {
                            cx.theme().border
                        })
                        .cursor_pointer()
                        .hover(|this| this.bg(cx.theme().accent.opacity(0.4)))
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.set_global(crate::ThemeFollowSystem(false));
                            gpui_kit::component::Theme::change(mode, None, cx);
                        }))
                        .child(Icon::new(icon).size_8())
                        .child(div().text_sm().child(label))
                }),
            )
            .child(
                v_flex()
                    .id("跟随系统")
                    .gap_2()
                    .w(px(180.))
                    .p_4()
                    .items_center()
                    .rounded(cx.theme().radius_lg)
                    .border_2()
                    .border_color(if follow_system {
                        cx.theme().primary
                    } else {
                        cx.theme().border
                    })
                    .cursor_pointer()
                    .hover(|this| this.bg(cx.theme().accent.opacity(0.4)))
                    .on_click(cx.listener(|_, _, window, cx| {
                        cx.set_global(crate::ThemeFollowSystem(true));
                        gpui_kit::component::Theme::sync_system_appearance(Some(window), cx);
                    }))
                    .child(
                        h_flex()
                            .h_8()
                            .items_center()
                            .gap_1()
                            .child(Icon::new(IconName::Sun).size_6())
                            .child(Icon::new(IconName::Moon).size_6()),
                    )
                    .child(div().text_sm().child("跟随系统")),
            )
            .into_any_element()
    }

    fn render_placeholder(&self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .text_color(cx.theme().muted_foreground)
            .child(Icon::new(IconName::Inbox).size_8())
            .child(div().text_sm().child("即将推出"))
            .into_any_element()
    }
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.form_dirty {
            self.form_dirty = false;
            self.sync_form(window, cx);
        }

        let content: AnyElement = match self.page {
            SettingsPage::Models => v_flex()
                .gap_4()
                .child(
                    h_flex()
                        .w_full()
                        .child(
                            v_flex()
                                .flex_1()
                                .gap_1()
                                .child(div().text_2xl().font_semibold().child("模型设置"))
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(cx.theme().muted_foreground)
                                        .child("管理自定义模型供应商，配置后可在聊天时选择使用。"),
                                ),
                        )
                        .child(
                            Button::new("add-provider")
                                .primary()
                                .icon(IconName::Plus)
                                .label("添加供应商")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.add_provider(cx);
                                })),
                        ),
                )
                .child(
                    h_flex()
                        .w_full()
                        .items_start()
                        .gap_4()
                        .child(
                            v_flex().w(px(280.)).gap_2().children(
                                (0..self.config.providers.len())
                                    .map(|ix| self.render_provider_row(ix, cx)),
                            ),
                        )
                        .child(div().flex_1().child(self.render_detail(cx))),
                )
                .into_any_element(),
            SettingsPage::Appearance => v_flex()
                .gap_4()
                .child(div().text_2xl().font_semibold().child("外观"))
                .child(self.render_appearance(cx))
                .into_any_element(),
            other => v_flex()
                .size_full()
                .gap_4()
                .child(div().text_2xl().font_semibold().child(page_title(other)))
                .child(div().flex_1().child(self.render_placeholder(cx)))
                .into_any_element(),
        };

        div()
            .size_full()
            .relative()
            .bg(cx.theme().background)
            .key_context("settings")
            .on_action(cx.listener(|_, _: &crate::CloseSettings, _, cx| {
                cx.emit(SettingsEvent::Close);
            }))
            .child(
                h_flex().size_full().child(self.render_nav(cx)).child(
                    div()
                        .id("settings-content")
                        .flex_1()
                        .h_full()
                        .overflow_y_scroll()
                        .child(div().w_full().p_6().child(content)),
                ),
            )
            .when(self.model_dialog.is_some(), |this| {
                this.child(
                    div()
                        .absolute()
                        .inset_0()
                        .child(self.render_model_dialog(cx)),
                )
            })
    }
}
