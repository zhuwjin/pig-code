use gpui_kit::base::{Align, AxisExt as _, ElementExt as _, Placement, Positioner};
use gpui_kit::component::ThemeMode;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::group_box::{GroupBox, GroupBoxVariant, GroupBoxVariants as _};
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::component::select::{SearchableVec, Select, SelectEvent, SelectState};
use gpui_kit::component::setting::{
    RenderOptions, SettingField, SettingFieldElement, SettingGroup, SettingItem, SettingPage,
    Settings,
};
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

/// 字体下拉首项：映射 config 里 ui_font/mono_font 的 None（跟随平台默认）。
/// 不是合法字体名，不会与本机字体撞名
const FONT_DEFAULT_LABEL: &str = "系统默认";

/// 主题模式下拉的三个选项（跟随系统 = 恢复跟随并立即对齐系统外观）
const THEME_FOLLOW_LABEL: &str = "跟随系统";
const THEME_DARK_LABEL: &str = "暗色";
const THEME_LIGHT_LABEL: &str = "亮色";

/// 字体下拉状态类型（条目为字体家族名字符串，可搜索）
type TextSelectState = SelectState<SearchableVec<String>>;

/// 字体设置槽位：界面字体 / 等宽字体
#[derive(Clone, Copy)]
enum FontSlot {
    Ui,
    Mono,
}

/// API 格式下拉的两个选项文案（回填与选项构造共用）
pub(crate) const FORMAT_OPENAI_LABEL: &str = "OpenAI Chat Completions (/v1/chat/completions)";
pub(crate) const FORMAT_ANTHROPIC_LABEL: &str = "Anthropic Messages (/v1/messages)";

/// Select 式设置字段（官方 SettingFieldElement）：与输入框同款触发器风格
///（text_sm、左对齐，规避 Button 的 16px 居中观感）；字体两项带搜索。
/// SelectState 是有状态 Entity，由 SettingsView 持有跨帧存活（render_field
/// 每帧调用不能现场建），选中事件经订阅回到本视图
struct SearchSelectField {
    select: Entity<TextSelectState>,
    /// 横排布局下的触发器宽度；None = 跟随容器全宽
    width: Option<Pixels>,
    menu_width: Pixels,
}

impl SettingFieldElement for SearchSelectField {
    type Element = Select<SearchableVec<String>>;

    fn render_field(&self, options: &RenderOptions, _: &mut Window, _: &mut App) -> Self::Element {
        Select::new(&self.select)
            .placeholder(FONT_DEFAULT_LABEL)
            .when(options.layout().is_vertical(), |this| this.w_full())
            .when_some(self.width, |this, w| this.w(w))
            .with_size(options.size())
            .menu_width(self.menu_width)
    }
}

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
mod providers;
mod skills;

pub(crate) use dialog::*;
pub(crate) use mcp::{
    McpConfigSnapshot, McpScope, McpSource, McpTransportKind, load_mcp_snapshot,
    workspace_display_name,
};
pub(crate) use skills::{SkillsSnapshot, load_skills_snapshot};

pub struct SettingsView {
    config: AppConfig,
    selected: Option<usize>,
    name_input: Entity<InputState>,
    base_url_input: Entity<InputState>,
    api_key_input: Entity<InputState>,
    api_key_masked: bool,
    delete_armed: bool,
    /// 详情表单 API 格式下拉（与输入框同款触发器；原 outline 按钮 16px 居中与表单不协调）
    format_select: Entity<TextSelectState>,
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
    /// 主题模式可能与全局状态脱节（设置页关闭期间系统外观被切），
    /// 打开设置页时置位，render 前同步主题模式下拉
    pub(crate) appearance_dirty: bool,
    /// 外观页下拉（选项 = 「系统默认」+ 本机已装字体，见 FONT_DEFAULT_LABEL）
    ui_font_select: Entity<TextSelectState>,
    mono_font_select: Entity<TextSelectState>,
    /// 主题模式下拉（跟随系统/暗色/亮色，不可搜索；与字体下拉同款视觉）
    theme_select: Entity<TextSelectState>,
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
        // 外观页字体下拉：首项「系统默认」+ 本机已装字体（枚举进程内缓存，仅首次 ~百毫秒）；
        // 选中事件走 set_font（写配置+立即生效+保存），官方字段无 setter 通道
        let ui_font_select = Self::new_font_select(window, cx);
        let mono_font_select = Self::new_font_select(window, cx);
        for (slot, select) in [
            (FontSlot::Ui, &ui_font_select),
            (FontSlot::Mono, &mono_font_select),
        ] {
            _subscriptions.push(cx.subscribe_in(
                select,
                window,
                move |this: &mut Self, _, event: &SelectEvent<SearchableVec<String>>, _, cx| {
                    if let SelectEvent::Confirm(Some(family)) = event {
                        this.set_font(slot, family.clone(), cx);
                    }
                },
            ));
        }
        // 主题模式下拉：三选项不可搜索（与字体下拉同款 Select 视觉）；确认即切换、
        // 不落盘（会话级）。回填靠 appearance_dirty（外部状态可变）
        let theme_select = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(vec![
                    THEME_FOLLOW_LABEL.to_string(),
                    THEME_DARK_LABEL.to_string(),
                    THEME_LIGHT_LABEL.to_string(),
                ]),
                None,
                window,
                cx,
            )
        });
        _subscriptions.push(cx.subscribe_in(
            &theme_select,
            window,
            |_: &mut Self, _, event: &SelectEvent<SearchableVec<String>>, _, cx| {
                if let SelectEvent::Confirm(Some(mode)) = event {
                    Self::apply_theme_mode(mode, cx);
                }
            },
        ));
        // API 格式下拉（两选项不可搜索）：确认即写回当前选中供应商并保存
        let format_select = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(vec![
                    FORMAT_OPENAI_LABEL.to_string(),
                    FORMAT_ANTHROPIC_LABEL.to_string(),
                ]),
                None,
                window,
                cx,
            )
        });
        _subscriptions.push(cx.subscribe_in(
            &format_select,
            window,
            |this: &mut Self, _, event: &SelectEvent<SearchableVec<String>>, _, cx| {
                if let SelectEvent::Confirm(Some(label)) = event {
                    this.set_api_format(label, cx);
                }
            },
        ));
        Self {
            config: AppConfig::default(),
            selected: None,
            name_input,
            base_url_input,
            api_key_input,
            api_key_masked: true,
            delete_armed: false,
            format_select,
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
            appearance_dirty: true,
            ui_font_select,
            mono_font_select,
            theme_select,
            _subscriptions,
        }
    }

    /// 建字体下拉状态：可搜索，无预选（占位「系统默认」，set_config 后由 sync_form 回填）
    fn new_font_select(window: &mut Window, cx: &mut Context<Self>) -> Entity<TextSelectState> {
        let mut items = vec![FONT_DEFAULT_LABEL.to_string()];
        items.extend(crate::font::installed_font_names(cx).iter().cloned());
        cx.new(|cx| SelectState::new(SearchableVec::new(items), None, window, cx).searchable(true))
    }

    /// 主题模式下拉与全局状态对齐（设置页关闭期间可能已被系统外观改）
    fn sync_appearance(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mode = Self::current_theme_mode(cx);
        self.theme_select.update(cx, |state, cx| {
            if state.selected_value() != Some(&mode) {
                state.set_selected_value(&mode, window, cx);
            }
        });
    }

    /// 字体下拉确认：哨兵项归一为 None，写回配置、立即生效并保存
    ///（core 落盘后的 ConfigSnapshot 回流会再应用一次，幂等无副作用）
    fn set_font(&mut self, slot: FontSlot, family: String, cx: &mut Context<Self>) {
        let resolved = (family != FONT_DEFAULT_LABEL).then_some(family);
        match slot {
            FontSlot::Ui => self.config.ui_font = resolved,
            FontSlot::Mono => self.config.mono_font = resolved,
        }
        crate::font::apply_config_fonts(&self.config, cx);
        cx.emit(SettingsEvent::Save(self.config.clone()));
        cx.notify();
    }

    /// API 格式下拉确认：写回当前选中供应商并保存；推理参数建议按格式生成，
    /// 不在此联动（弹窗里按当前格式生成）
    fn set_api_format(&mut self, label: &str, cx: &mut Context<Self>) {
        let Some(ix) = self.selected else { return };
        let format = if label == FORMAT_ANTHROPIC_LABEL {
            ApiFormat::AnthropicMessages
        } else {
            ApiFormat::OpenAiChat
        };
        if self.config.providers[ix].api_format != format {
            self.config.providers[ix].api_format = format;
            cx.emit(SettingsEvent::Save(self.config.clone()));
            cx.notify();
        }
    }

    /// 主题模式 dropdown 确认：跟随系统 = 恢复同步并立即对齐当前系统外观
    ///（window_appearance 免 window，官方 SettingField setter 只给 &mut App）；
    /// 亮/暗 = 脱离跟随固定模式。主题不落盘（会话级设置）
    fn apply_theme_mode(mode: &str, cx: &mut App) {
        match mode {
            THEME_FOLLOW_LABEL => {
                cx.set_global(crate::ThemeFollowSystem(true));
                gpui_kit::component::Theme::change(cx.window_appearance(), None, cx);
            }
            THEME_DARK_LABEL => {
                cx.set_global(crate::ThemeFollowSystem(false));
                gpui_kit::component::Theme::change(ThemeMode::Dark, None, cx);
            }
            THEME_LIGHT_LABEL => {
                cx.set_global(crate::ThemeFollowSystem(false));
                gpui_kit::component::Theme::change(ThemeMode::Light, None, cx);
            }
            _ => {}
        }
    }

    /// 主题模式 dropdown 当前值：跟随系统 > 暗色 > 亮色
    fn current_theme_mode(cx: &App) -> String {
        let follow = cx
            .try_global::<crate::ThemeFollowSystem>()
            .is_some_and(|flag| flag.0);
        if follow {
            THEME_FOLLOW_LABEL.to_string()
        } else if cx.theme().mode.is_dark() {
            THEME_DARK_LABEL.to_string()
        } else {
            THEME_LIGHT_LABEL.to_string()
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
        // 字体下拉回填：None 显示为「系统默认」（无供应商时也要同步，先于早退）
        let ui_font = self
            .config
            .ui_font
            .clone()
            .unwrap_or_else(|| FONT_DEFAULT_LABEL.to_string());
        let mono_font = self
            .config
            .mono_font
            .clone()
            .unwrap_or_else(|| FONT_DEFAULT_LABEL.to_string());
        self.ui_font_select.update(cx, |state, cx| {
            if state.selected_value() != Some(&ui_font) {
                state.set_selected_value(&ui_font, window, cx);
            }
        });
        self.mono_font_select.update(cx, |state, cx| {
            if state.selected_value() != Some(&mono_font) {
                state.set_selected_value(&mono_font, window, cx);
            }
        });
        let Some(provider) = self.selected_provider().cloned() else {
            return;
        };
        let name = provider.name.clone();
        let base_url = provider.base_url.clone();
        let api_key = provider.api_key.clone();
        let format_label = if provider.api_format == ApiFormat::AnthropicMessages {
            FORMAT_ANTHROPIC_LABEL
        } else {
            FORMAT_OPENAI_LABEL
        }
        .to_string();
        self.format_select.update(cx, |state, cx| {
            if state.selected_value() != Some(&format_label) {
                state.set_selected_value(&format_label, window, cx);
            }
        });
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
        if self.appearance_dirty {
            self.appearance_dirty = false;
            self.sync_appearance(window, cx);
        }

        // 官方 Settings 组件：自带侧栏（搜索 + 页导航，选中态按 id 持久）与
        // 页面标题/描述/滚动；字段与自定义内容经 WeakEntity 回到本视图发事件
        let weak = cx.entity().downgrade();
        let settings = Settings::new("pig-settings")
            .with_group_variant(GroupBoxVariant::Fill)
            .pages(vec![
            // 外观：三个下拉统一用 Select（同宽 220/同款触发器与菜单视觉，
            // 字体两项可搜索）；主题模式为有状态字段，回填经 appearance_dirty
            SettingPage::new("外观")
                .icon(IconName::Palette)
                .description("主题模式与全局字体。")
                .group(
                    SettingGroup::new()
                        .title("界面")
                        .items(vec![
                            SettingItem::new(
                                "主题模式",
                                SettingField::element(SearchSelectField {
                                    select: self.theme_select.clone(),
                                    width: Some(px(220.)),
                                    menu_width: px(300.),
                                }),
                            )
                            .description("应用的整体配色；跟随系统时随系统外观自动切换。"),
                            self.font_item(FontSlot::Ui),
                        ]),
                )
                .group(
                    SettingGroup::new()
                        .title("代码")
                        .items(vec![self.font_item(FontSlot::Mono)]),
                ),
            Self::content_page(
                "模型设置",
                IconName::Bot,
                "管理自定义模型供应商，配置后可在聊天时选择使用。",
                &["model", "provider", "模型", "供应商"],
                &weak,
                Self::render_models_page,
            ),
            Self::content_page(
                "MCP 服务器",
                IconName::Network,
                "管理用户级与项目级 mcp.json；服务器在每个会话首个回合后按需连接。",
                &["mcp", "服务器"],
                &weak,
                Self::render_mcp_page,
            ),
            Self::content_page(
                "技能",
                IconName::BookOpen,
                "管理用户级与项目级技能（SKILL.md）；清单注入系统提示词，正文由 Skill 工具按需加载，改动对新建会话生效。",
                &["skill", "技能"],
                &weak,
                Self::render_skills_page,
            ),
            Self::content_page(
                "网络搜索",
                IconName::Search,
                "WebSearch 工具的搜索后端状态，经环境变量配置。",
                &["websearch", "搜索"],
                &weak,
                Self::render_websearch,
            ),
        ]);

        div()
            .size_full()
            .relative()
            .bg(cx.theme().background)
            .key_context("settings")
            .on_action(cx.listener(|_, _: &crate::CloseSettings, _, cx| {
                cx.emit(SettingsEvent::Close);
            }))
            .child(settings)
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

impl SettingsView {
    // ---------- 官方 Settings 组件的页面构建 ----------

    /// 字体设置条目：scrollable_dropdown（本机字体数百项，弹层内滚动），
    /// 哨兵「系统默认」映射 config 的 None；选项列表进程内缓存
    /// 字体设置条目：自定义可搜索字段（FontSettingField），状态实体由本视图持有，
    /// 选中经订阅走 set_font（写配置 + 立即生效 + 保存）
    fn font_item(&self, slot: FontSlot) -> SettingItem {
        let select = match slot {
            FontSlot::Ui => self.ui_font_select.clone(),
            FontSlot::Mono => self.mono_font_select.clone(),
        };
        SettingItem::new(
            match slot {
                FontSlot::Ui => "界面字体",
                FontSlot::Mono => "等宽字体",
            },
            SettingField::element(SearchSelectField {
                select,
                width: Some(px(220.)),
                menu_width: px(300.),
            }),
        )
        .description(match slot {
            FontSlot::Ui => "界面文本使用的字体，选择后立即生效并保存。",
            FontSlot::Mono => "代码块、diff 与命令行使用的字体，选择后立即生效并保存。",
        })
    }

    /// 自定义内容页：现有整页渲染塞进单个条目（官方组件接管导航/标题/滚动）；
    /// keywords 让无标题的自定义条目仍可被侧栏搜索命中
    fn content_page(
        title: &str,
        icon: IconName,
        description: &str,
        keywords: &[&str],
        weak: &WeakEntity<Self>,
        content: fn(&mut Self, &mut Context<Self>) -> AnyElement,
    ) -> SettingPage {
        let weak = weak.clone();
        SettingPage::new(title)
            .icon(icon)
            .description(description)
            .group(
                SettingGroup::new().item(
                    SettingItem::render(move |_, _, cx: &mut App| {
                        weak.update(cx, content)
                            .unwrap_or_else(|_| div().into_any_element())
                    })
                    .keywords(keywords.iter().copied()),
                ),
            )
    }

    /// 模型设置页内容：左列（供应商列表）+ 右列（详情表单）各用官方 GroupBox
    ///（Fill 变体，与官方 Settings 分组同一卡面色）；添加供应商按钮收进左列顶部
    fn render_models_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        h_flex()
            .w_full()
            .items_start()
            .gap_4()
            .child(
                GroupBox::new()
                    .id("providers")
                    .fill()
                    .w(px(240.))
                    .title(
                        div()
                            .text_sm()
                            .font_medium()
                            .text_color(cx.theme().muted_foreground)
                            .child("供应商"),
                    )
                    .child(
                        Button::new("add-provider")
                            .outline()
                            .small()
                            .w_full()
                            .icon(IconName::Plus)
                            .label("添加供应商")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.add_provider(cx);
                            })),
                    )
                    .children(
                        (0..self.config.providers.len()).map(|ix| self.render_provider_row(ix, cx)),
                    ),
            )
            .child(
                GroupBox::new()
                    .id("provider-detail")
                    .fill()
                    .flex_1()
                    .min_w_0()
                    .child(self.render_detail(cx)),
            )
            .into_any_element()
    }

    /// MCP 页内容：页头（搜索/新建/刷新）+ 服务器列表
    fn render_mcp_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .gap_4()
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child("用户级与项目级条目合并展示，同名项目级覆盖。"),
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
            .into_any_element()
    }

    /// 技能页内容：页头（搜索/新建）+ 技能列表
    fn render_skills_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .gap_4()
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child("清单注入系统提示词，正文按需加载；改动对新建会话生效。"),
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
            .into_any_element()
    }
}
