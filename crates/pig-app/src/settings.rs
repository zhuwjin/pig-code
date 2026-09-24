use gpui_kit::base::{Align, ElementExt as _, Placement, Positioner};
use gpui_kit::component::ThemeMode;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _, h_flex,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use pig_protocol::{ApiFormat, AppConfig, ModelConfig, ProviderConfig, default_reasoning_params};
use std::cell::Cell;
use std::rc::Rc;

/// 新建模型的默认上下文/输出上限：自动填充时数据源缺字段也回落到这组值
const NEW_MODEL_CONTEXT: u64 = 128_000;
const NEW_MODEL_MAX_OUTPUT: u64 = 8_192;

#[derive(Clone)]
pub enum SettingsEvent {
    Save(AppConfig),
    TestProvider(String),
    /// 模型 ID 输入完成（回车/失焦），查 models.dev 元数据
    LookupModel(String),
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
    /// 默认思考等级：新会话与切换模型的初始档；None = 不设置
    default_level: Option<String>,
    new_level: Entity<InputState>,
    new_label: Entity<InputState>,
    params_json: Entity<TextareaState>,
    params_error: Option<String>,
    snapshot: Option<ModelConfig>,
    /// 已发起过 models.dev 查询的模型 ID（同 ID 不重查，改了 ID 才会再查）
    looked_up_id: Option<String>,
    /// models.dev 查询状态（loading / 未收录提示）
    lookup_state: LookupState,
    /// 本次查询是否按「重置表单」语义填充：完全覆盖 + 缺字段回落默认值；
    /// 回车/失焦触发的查询走温和填充（数据源有才覆盖，手配参数 JSON 保留）
    lookup_overwrite: bool,
}

/// models.dev 查询进度：输入框旁的提示行三态
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum LookupState {
    #[default]
    Idle,
    Pending,
    /// 查询完成但未收录（网络失败同样落这里，回车可重试）
    NotFound,
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
    /// API 格式按钮的位置：deferred 弹层锚定用（每次 prepaint 更新）
    format_btn_bounds: Rc<Cell<Bounds<Pixels>>>,
    /// 最近一次被弹层 outside-close 关掉时的按下位置：弹层打开时点按钮，
    /// outside-close 先把它关掉，同一次按压的 click 紧跟着到达——按同一位置吞掉
    format_outside_close: Option<Point<Pixels>>,
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
            format_btn_bounds: Rc::new(Cell::new(Bounds::default())),
            format_outside_close: None,
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
        // id 必须全库唯一：按「数量+1」生成会在删除过供应商后与存量撞车
        //（撞车后所有按 id 的查找都命中第一个：模型解析落到错误供应商的
        // 兜底模型、label 张冠李戴、会话 meta 混乱）
        let mut n = 1usize;
        let id = loop {
            let candidate = format!("custom-{n}");
            if !self.config.providers.iter().any(|p| p.id == candidate) {
                break candidate;
            }
            n += 1;
        };
        let ix = self.config.providers.len();
        self.config.providers.push(ProviderConfig {
            id,
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
        let model =
            model.unwrap_or_else(|| ModelConfig::new("", NEW_MODEL_CONTEXT, NEW_MODEL_MAX_OUTPUT));
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
            // 已被删出等级表的默认档不算数
            default_level: model
                .default_reasoning_level
                .clone()
                .filter(|lv| model.reasoning_levels.contains(lv)),
            new_level: cx.new(|cx| InputState::new(window, cx).placeholder("等级名，如 high")),
            new_label: cx
                .new(|cx| InputState::new(window, cx).placeholder("显示名（可选），如 最高")),
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
            looked_up_id: None,
            lookup_state: LookupState::Idle,
            lookup_overwrite: false,
        };
        // 模型 ID 输入完成（回车/失焦）→ 查 models.dev 自动填充。
        // 对话框每次重开都新建输入框，旧订阅靠 entity id 排除
        let id_input = dialog.id.clone();
        self._subscriptions.push(cx.subscribe_in(
            &id_input,
            window,
            |this, emitter, event: &gpui_kit::component::input::InputEvent, _, cx| {
                let Some(dialog) = &this.model_dialog else {
                    return;
                };
                if dialog.id.entity_id() != emitter.entity_id() {
                    return;
                }
                match event {
                    gpui_kit::component::input::InputEvent::PressEnter { .. }
                    | gpui_kit::component::input::InputEvent::Blur => {
                        this.maybe_lookup_model(false, cx);
                    }
                    // 输入变化：清掉上一次查询的状态提示
                    gpui_kit::component::input::InputEvent::Change => {
                        let dirty = this
                            .model_dialog
                            .as_mut()
                            .is_some_and(|d| d.lookup_state != LookupState::Idle);
                        if dirty {
                            this.model_dialog
                                .as_mut()
                                .map(|d| d.lookup_state = LookupState::Idle);
                            cx.notify();
                        }
                    }
                    _ => {}
                }
            },
        ));
        self.format_popup = false;
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
            self.format_popup = false;
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
            // 默认档必须仍在等级表内
            default_reasoning_level: dialog
                .default_level
                .clone()
                .filter(|lv| dialog.reasoning_levels.contains(lv)),
            // 显示名只保留仍存在的等级 id（防御chip外路径改列表）
            reasoning_labels: dialog
                .reasoning_labels
                .iter()
                .filter(|(id, label)| dialog.reasoning_levels.contains(*id) && !label.is_empty())
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

    /// 模型 ID 输入完成：非空且与上次查询不同才发起 models.dev 查询。
    /// overwrite=true 为「重置表单」语义（查询结果完全覆盖 + 缺字段回落默认值）
    fn maybe_lookup_model(&mut self, overwrite: bool, cx: &mut Context<Self>) {
        let Some(dialog) = &self.model_dialog else {
            return;
        };
        let id = dialog.id.read(cx).value().trim().to_string();
        if id.is_empty() || dialog.looked_up_id.as_deref() == Some(id.as_str()) {
            return;
        }
        self.model_dialog
            .as_mut()
            .expect("dialog checked above")
            .looked_up_id = Some(id.clone());
        let dialog = self.model_dialog.as_mut().expect("dialog checked above");
        dialog.lookup_state = LookupState::Pending;
        dialog.lookup_overwrite = overwrite;
        cx.emit(SettingsEvent::LookupModel(id));
    }

    /// models.dev 查询结果回填弹窗。只在事件对应弹窗当前编辑的 ID 时应用。
    /// 重置触发的查询（lookup_overwrite）：字段 = 数据源值 ?? 新建默认值，参数 JSON 无条件重生成；
    /// 回车/失焦触发的查询：温和填充——数据源有才覆盖，缺字段不动用户值，手配参数 JSON 保留
    pub fn apply_model_info(
        &mut self,
        id: &str,
        info: Option<pig_protocol::ModelRegistryInfo>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let overwrite = self
            .model_dialog
            .as_ref()
            .is_some_and(|d| d.lookup_overwrite);
        let Some(dialog) = &mut self.model_dialog else {
            return;
        };
        if dialog.id.read(cx).value().trim() != id {
            return;
        }
        let Some(info) = info else {
            // None = 网络失败或数据源未收录：解锁该 ID，回车可重试
            // （core 侧有 10 分钟节流，重试不会连打网络）
            dialog.looked_up_id = None;
            dialog.lookup_state = LookupState::NotFound;
            cx.notify();
            return;
        };
        dialog.lookup_state = LookupState::Idle;
        // 上下文窗口 / 最大输出：重置语义缺字段回落新建默认值；回车语义缺则不动
        let context_src = info.context.or(info.input);
        if overwrite || context_src.is_some() {
            let context = context_src.unwrap_or(NEW_MODEL_CONTEXT);
            dialog
                .context_window
                .update(cx, |i, cx| i.set_value(context.to_string(), window, cx));
        }
        if overwrite || info.output.is_some() {
            let output = info.output.unwrap_or(NEW_MODEL_MAX_OUTPUT);
            dialog
                .max_tokens
                .update(cx, |i, cx| i.set_value(output.to_string(), window, cx));
        }
        if overwrite || !info.reasoning_levels.is_empty() {
            dialog
                .reasoning_labels
                .retain(|k, _| info.reasoning_levels.contains(k));
            // 常用等级配中文显示名，其余显示 id 本身
            for (level, label) in [
                ("none", "无"),
                ("minimal", "极简"),
                ("low", "低"),
                ("medium", "中"),
                ("high", "高"),
                ("xhigh", "超高"),
                ("max", "最高"),
            ] {
                if info.reasoning_levels.iter().any(|l| l == level) {
                    dialog.reasoning_labels.insert(level.into(), label.into());
                }
            }
            dialog.reasoning_levels = info.reasoning_levels.clone();
            // 等级表变了：指向已删等级的默认档作废
            dialog.default_level = dialog
                .default_level
                .take()
                .filter(|lv| info.reasoning_levels.contains(lv));
        }
        // 推理参数 JSON：重置语义无条件按等级 + API 格式重新生成；
        // 回车语义只在未手配（空对象）时生成建议值
        let params_current = dialog.params_json.read(cx).value().trim().to_string();
        let untouched = serde_json::from_str::<serde_json::Value>(&params_current)
            .map(|v| v.as_object().is_some_and(|m| m.is_empty()))
            .unwrap_or(true);
        if overwrite || untouched {
            let api_format = self
                .selected
                .and_then(|ix| self.config.providers.get(ix))
                .map(|p| p.api_format)
                .unwrap_or(ApiFormat::OpenAiChat);
            let params = default_reasoning_params(&info.reasoning_levels, api_format);
            let has_params = !params.is_empty();
            let raw = serde_json::to_string_pretty(&serde_json::Value::Object(params))
                .unwrap_or_default();
            dialog
                .params_json
                .update(cx, |t, cx| t.set_value(raw, window, cx));
            if has_params {
                dialog.advanced_open = true;
            }
        }
        // 输入模态：重置语义缺数据按全 false；回车语义数据源未给则不动勾选
        if overwrite || !info.input_modalities.is_empty() {
            let has = |m: &str| info.input_modalities.iter().any(|v| v == m);
            dialog.input_image = has("image");
            dialog.input_video = has("video");
            dialog.input_pdf = has("pdf");
        }
        if overwrite || info.structured_output.is_some() {
            dialog.cap_structured = info.structured_output.unwrap_or(false);
        }
        cx.notify();
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
                            .label(if self.delete_armed {
                                "确认删除？"
                            } else {
                                "删除"
                            })
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
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("名称"),
                    )
                    .child(Input::new(&self.name_input)),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("Base URL"),
                    )
                    .child(Input::new(&self.base_url_input)),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("API 格式"),
                    )
                    .child(
                        div()
                            .on_prepaint({
                                let cell = self.format_btn_bounds.clone();
                                move |bounds, _, _| cell.set(bounds)
                            })
                            .child(
                                Button::new("api-format")
                                    .outline()
                                    .w_full()
                                    .label(if is_anthropic {
                                        "Anthropic Messages (/v1/messages)"
                                    } else {
                                        "OpenAI Chat Completions (/v1/chat/completions)"
                                    })
                                    .on_click(cx.listener(|this, event: &ClickEvent, _, cx| {
                                        // 弹层打开时点按钮：按下先触发弹层的 outside-close
                                        // （记录按下位置），紧随的 click 按同一位置吞掉，
                                        // 避免收起又马上弹开（main.rs 右侧面板菜单同款处理）
                                        let down_pos = match event {
                                            ClickEvent::Mouse(e) => Some(e.down.position),
                                            _ => None,
                                        };
                                        if this
                                            .format_outside_close
                                            .take()
                                            .is_some_and(|pos| Some(pos) == down_pos)
                                        {
                                            return;
                                        }
                                        this.format_popup = !this.format_popup;
                                        cx.notify();
                                    })),
                            )
                            .when(self.format_popup, |this| {
                                this.child(self.render_format_popup(p_ix, cx))
                            }),
                    ),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("API Key"),
                    )
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
                        .text_color(if ok {
                            cx.theme().success
                        } else {
                            cx.theme().danger
                        })
                        .child(message),
                )
            })
            .child(
                h_flex()
                    .w_full()
                    .mt_2()
                    .child(div().text_sm().font_semibold().flex_1().child("模型列表"))
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
    /// API 格式下拉：deferred 到窗口层绘制，`Positioner::side(Bottom)` 锚定按钮正下方
    /// （与 main.rs 标签页 "+" 菜单同一模式）。详情列在 overflow_y_scroll 容器内，
    /// absolute 弹层会被滚动区裁剪，且后续表单兄弟（API Key 输入框等带背景元素）
    /// 按文档序画在其上，看起来就是弹层没有背景。
    fn render_format_popup(&self, p_ix: usize, cx: &mut Context<Self>) -> AnyElement {
        deferred(
            Positioner::side(self.format_btn_bounds.get())
                .placement(Placement::Bottom)
                .align(Align::Start)
                .offset(px(4.))
                .margin(px(8.))
                .occlude()
                .child(
                    v_flex()
                        .id("format-popup")
                        .w(px(360.))
                        .py_1()
                        .rounded(cx.theme().radius)
                        .border_1()
                        .border_color(cx.theme().border)
                        .bg(cx.theme().popover)
                        .shadow_lg()
                        .on_mouse_down_out(cx.listener(|this, event: &MouseDownEvent, _, cx| {
                            this.format_popup = false;
                            this.format_outside_close = Some(event.position);
                            cx.notify();
                        }))
                        .children(
                            [
                                (
                                    "OpenAI Chat Completions (/v1/chat/completions)",
                                    ApiFormat::OpenAiChat,
                                ),
                                (
                                    "Anthropic Messages (/v1/messages)",
                                    ApiFormat::AnthropicMessages,
                                ),
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
                ),
        )
        .with_priority(1)
        .into_any_element()
    }

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
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child("模型 ID"),
                                    )
                                    .child(match dialog.lookup_state {
                                        LookupState::Pending => h_flex()
                                            .gap_1()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(Spinner::new().small())
                                            .child("正在查询 models.dev…")
                                            .into_any_element(),
                                        LookupState::NotFound => div()
                                            .text_xs()
                                            .text_color(cx.theme().warning)
                                            .child("models.dev 未收录该 ID，回车可重试")
                                            .into_any_element(),
                                        LookupState::Idle => div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .opacity(0.7)
                                            .child("输入后回车，自动填充上下文/输出/推理等级（models.dev）")
                                            .into_any_element(),
                                    }),
                            )
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
                                                                                if d.default_level.as_deref() == Some(level_owned.as_str()) {
                                                                                    d.default_level = None;
                                                                                }
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
                                                                                if d.default_level.as_deref() == Some(&level) {
                                                                                    d.default_level = None;
                                                                                }
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
                                                .child(div().text_xs().text_color(cx.theme().muted_foreground).child("默认思考等级（新会话与切换模型的初始档）"))
                                                .child(
                                                    h_flex()
                                                        .gap_1()
                                                        .children(
                                                            // 首项「不设置」= None，其余为等级表各档
                                                            [None]
                                                                .into_iter()
                                                                .chain(dialog.reasoning_levels.iter().cloned().map(Some))
                                                                .enumerate()
                                                                .map(|(ix, opt)| {
                                                                    let selected = dialog.default_level == opt;
                                                                    let label = opt.clone()
                                                                        .map(|lv| dialog.reasoning_labels.get(&lv).cloned().filter(|s| !s.is_empty()).unwrap_or(lv))
                                                                        .unwrap_or_else(|| "不设置".to_string());
                                                                    div()
                                                                        .id(("default-level", ix))
                                                                        .px_2()
                                                                        .py_0p5()
                                                                        .rounded_full()
                                                                        .cursor_pointer()
                                                                        .when(selected, |this| this.bg(cx.theme().accent))
                                                                        .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
                                                                        .on_click(cx.listener(move |this, _, _, cx| {
                                                                            if let Some(d) = &mut this.model_dialog {
                                                                                d.default_level = opt.clone();
                                                                            }
                                                                            cx.notify();
                                                                        }))
                                                                        .child(div().text_xs().child(label))
                                                                })
                                                                .collect::<Vec<_>>(),
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
                                        let snapshot =
                                            this.model_dialog.as_ref().and_then(|d| d.snapshot.clone());
                                        let editing = this.model_dialog.as_ref().and_then(|d| d.editing);
                                        if snapshot.is_some() {
                                            this.open_model_dialog(editing, window, cx);
                                            // 恢复快照后按当前模型 ID 重新走 models.dev
                                            // 自动填充（重置语义：完全覆盖 + 缺字段回落默认）
                                            this.maybe_lookup_model(true, cx);
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
