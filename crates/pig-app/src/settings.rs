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
use pig_protocol::{
    ApiFormat, AppConfig, McpServerStatus, ModelConfig, ProviderConfig, default_reasoning_params,
};
use std::cell::Cell;
use std::path::PathBuf;
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
    /// MCP 页刷新：AppView 重读 mcp.json 并向 core 查询连接清单
    RefreshMcp,
    /// 技能页刷新：AppView 重读技能目录
    RefreshSkills,
    Close,
}

impl EventEmitter<SettingsEvent> for SettingsView {}

mod dialog;
mod mcp;
mod pages;
mod providers;
mod skills;

pub(crate) use dialog::*;
pub(crate) use mcp::{
    McpConfigSnapshot, McpScope, McpSource, McpTransportKind, load_mcp_snapshot,
    workspace_display_name,
};
pub(crate) use skills::{SkillsSnapshot, load_skills_snapshot};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SettingsPage {
    General,
    Appearance,
    Models,
    BrowserControl,
    ComputerControl,
    WebSearch,
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

/// (页面, 图标, 名称)
type NavItem = (SettingsPage, IconName, &'static str);

/// (分组, [NavItem])——加页面只改这一处
const NAV: &[(&str, &[NavItem])] = &[
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
            (SettingsPage::WebSearch, IconName::Search, "网络搜索"),
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
    /// MCP 页配置快照（AppView 经 RefreshMcp 事件喂入；None = 尚未读取）
    mcp_snapshot: Option<McpConfigSnapshot>,
    /// 连接状态对应的会话（None = 未打开会话：只展示配置不展示状态）
    mcp_session: Option<String>,
    /// 状态查询进度：None = 等待回包；Some(None) = 会话尚未发起懒连接；
    /// Some(Some(statuses)) = 各 server 状态（含工具数与失败原因）
    mcp_connection: Option<Option<Vec<McpServerStatus>>>,
    /// MCP 页搜索框（按名称/命令/URL 过滤）
    mcp_search: Entity<InputState>,
    /// MCP 新建/编辑对话框（None = 关闭）
    mcp_dialog: Option<McpDialog>,
    /// mcp.json 写入失败提示（成功写入或下次刷新前保留）
    mcp_write_error: Option<String>,
    /// MCP 页作用域：用户级（默认）/ 指定工作区（AppView 按此加载快照）
    mcp_scope: McpScope,
    /// 当前会话的工作区（MCP 连接状态适用性与两页作用域下拉的「当前会话」标记）
    session_cwd: Option<PathBuf>,
    /// 可选工作区清单（路径 + 显示名，侧栏同口径：可见工作区 ∪ 会话 cwd；
    /// MCP/技能两页作用域下拉共用）
    scope_workspaces: Vec<(PathBuf, String)>,
    /// 作用域下拉弹层开合
    mcp_scope_popup: bool,
    /// 作用域按钮位置（deferred 弹层锚定用，每次 prepaint 更新）
    mcp_scope_btn_bounds: Rc<Cell<Bounds<Pixels>>>,
    /// 弹层 outside-close 时的按下位置（同一次按压的 click 按位置吞掉）
    mcp_scope_outside_close: Option<Point<Pixels>>,
    /// 技能页快照（AppView 经 RefreshSkills 事件喂入；None = 尚未读取）
    skills_snapshot: Option<SkillsSnapshot>,
    /// 技能页搜索框（按名称/描述过滤）
    skills_search: Entity<InputState>,
    /// 技能新建/编辑对话框（None = 关闭）
    skills_dialog: Option<SkillDialog>,
    /// 技能目录写入失败提示（成功写入或下次刷新前保留）
    skills_write_error: Option<String>,
    /// 技能页作用域：用户级（默认）/ 指定工作区
    skills_scope: McpScope,
    /// 技能页作用域下拉弹层三件套（同 mcp_scope_*）
    skills_scope_popup: bool,
    skills_scope_btn_bounds: Rc<Cell<Bounds<Pixels>>>,
    skills_scope_outside_close: Option<Point<Pixels>>,
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
        // MCP 搜索框：内容变化即重过滤列表
        let mcp_search = cx.new(|cx| InputState::new(window, cx).placeholder("搜索服务器…"));
        _subscriptions.push(cx.subscribe_in(
            &mcp_search,
            window,
            |_: &mut Self, _, _: &gpui_kit::component::input::InputEvent, _, cx| {
                cx.notify();
            },
        ));
        // 技能搜索框：内容变化即重过滤列表
        let skills_search = cx.new(|cx| InputState::new(window, cx).placeholder("搜索技能…"));
        _subscriptions.push(cx.subscribe_in(
            &skills_search,
            window,
            |_: &mut Self, _, _: &gpui_kit::component::input::InputEvent, _, cx| {
                cx.notify();
            },
        ));
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
            mcp_snapshot: None,
            mcp_session: None,
            mcp_connection: None,
            mcp_search,
            mcp_dialog: None,
            mcp_write_error: None,
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
            skills_scope: McpScope::User,
            skills_scope_popup: false,
            skills_scope_btn_bounds: Rc::new(Cell::new(Bounds::default())),
            skills_scope_outside_close: None,
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

    /// MCP 页数据喂入（AppView 刷新时调用）：重置换页/刷新前的连接查询结果；
    /// session_cwd 用于判断「查看的工作区 ≠ 会话工作区」时隐藏连接状态
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

    /// core 的 McpServerList 回包（AppView 已按当前会话过滤）
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
            SettingsPage::Mcp => v_flex()
                .gap_4()
                .child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .child(
                            v_flex()
                                .flex_1()
                                .gap_1()
                                .child(div().text_2xl().font_semibold().child("MCP 服务器"))
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(cx.theme().muted_foreground)
                                        .child("管理用户级与项目级 mcp.json；服务器在每个会话首个回合后按需连接。"),
                                ),
                        )
                        .child(div().w(px(180.)).child(Input::new(&self.mcp_search)))
                        .child(
                            Button::new("new-mcp")
                                .primary()
                                .icon(IconName::Plus)
                                .label("新建服务器")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_mcp_dialog(None, window, cx);
                                })),
                        )
                        .child(
                            Button::new("refresh-mcp")
                                .outline()
                                .icon(IconName::RotateCw)
                                .label("刷新")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.refresh_mcp(cx);
                                })),
                        ),
                )
                .child(self.render_mcp(cx))
                .into_any_element(),
            SettingsPage::WebSearch => v_flex()
                .gap_4()
                .child(
                    v_flex()
                        .gap_1()
                        .child(div().text_2xl().font_semibold().child("网络搜索"))
                        .child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child("WebSearch 工具的搜索后端状态，经环境变量配置。"),
                        ),
                )
                        .child(self.render_websearch(cx))
                        .into_any_element(),
            SettingsPage::Skills => v_flex()
                .gap_4()
                .child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .child(
                            v_flex()
                                .flex_1()
                                .gap_1()
                                .child(div().text_2xl().font_semibold().child("技能"))
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(cx.theme().muted_foreground)
                                        .child("管理用户级与项目级技能（SKILL.md）；清单注入系统提示词，正文由 Skill 工具按需加载，改动对新建会话生效（会话内清单冻结保缓存）。"),
                                ),
                        )
                        .child(div().w(px(180.)).child(Input::new(&self.skills_search)))
                        .child(
                            Button::new("new-skill")
                                .primary()
                                .icon(IconName::Plus)
                                .label("新建技能")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_skills_dialog(None, window, cx);
                                })),
                        ),
                )
                .child(self.render_skills(cx))
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
            .when(self.mcp_dialog.is_some(), |this| {
                this.child(div().absolute().inset_0().child(self.render_mcp_dialog(cx)))
            })
            .when(self.skills_dialog.is_some(), |this| {
                this.child(
                    div()
                        .absolute()
                        .inset_0()
                        .child(self.render_skills_dialog(cx)),
                )
            })
    }
}
