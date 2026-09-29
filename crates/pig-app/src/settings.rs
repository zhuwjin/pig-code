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

mod dialog;
mod pages;
mod providers;

pub(crate) use dialog::*;
pub(crate) use pages::*;
pub(crate) use providers::*;

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
