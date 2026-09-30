use super::*;

pub struct ModelDialog {
    /// None = 新增模型
    pub(crate) editing: Option<usize>,
    pub(crate) id: Entity<InputState>,
    pub(crate) context_window: Entity<InputState>,
    pub(crate) max_tokens: Entity<InputState>,
    pub(crate) advanced_open: bool,
    pub(crate) input_image: bool,
    pub(crate) input_video: bool,
    pub(crate) input_pdf: bool,
    pub(crate) cap_structured: bool,
    pub(crate) cap_web_search: bool,
    pub(crate) cap_system_msg: bool,
    pub(crate) enabled: bool,
    pub(crate) reasoning_levels: Vec<String>,
    /// 等级 id → 显示名（仅展示；随 chip 增删联动）
    pub(crate) reasoning_labels: std::collections::HashMap<String, String>,
    /// 默认思考等级：新会话与切换模型的初始档；None = 不设置
    pub(crate) default_level: Option<String>,
    pub(crate) new_level: Entity<InputState>,
    pub(crate) new_label: Entity<InputState>,
    pub(crate) params_json: Entity<TextareaState>,
    pub(crate) params_error: Option<String>,
    pub(crate) snapshot: Option<ModelConfig>,
    /// 已发起过 models.dev 查询的模型 ID（同 ID 不重查，改了 ID 才会再查）
    pub(crate) looked_up_id: Option<String>,
    /// models.dev 查询状态（loading / 未收录提示）
    pub(crate) lookup_state: LookupState,
    /// 本次查询是否按「重置表单」语义填充：完全覆盖 + 缺字段回落默认值；
    /// 回车/失焦触发的查询走温和填充（数据源有才覆盖，手配参数 JSON 保留）
    pub(crate) lookup_overwrite: bool,
}

/// models.dev 查询进度：输入框旁的提示行三态
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum LookupState {
    #[default]
    Idle,
    Pending,
    /// 查询完成但未收录（网络失败同样落这里，回车可重试）
    NotFound,
}

/// MCP 新建/编辑对话框（表单/JSON 双模式，对齐 ZCode 的编辑器形态）
pub(crate) struct McpDialog {
    /// 正在编辑的 server 名（None = 新建；编辑中名称与作用域锁定）
    pub(crate) editing: Option<String>,
    /// 写入目标文件层级
    pub(crate) scope: McpSource,
    /// 项目级是否可选（未打开会话时项目级路径未知，不可用）
    pub(crate) project_available: bool,
    /// 传输类型（stdio 本地命令 / HTTP 远程端点）
    pub(crate) kind: McpTransportKind,
    pub(crate) name: Entity<InputState>,
    pub(crate) command: Entity<InputState>,
    /// stdio 参数（空格分隔；含空格的参数请走 JSON 模式）
    pub(crate) args: Entity<InputState>,
    pub(crate) url: Entity<InputState>,
    /// 超时毫秒（空 = 默认 30s）
    pub(crate) timeout: Entity<InputState>,
    /// 项目级目标的工作区显示名（作用域按钮展示「项目级（xxx）」；None = 未知
    pub(crate) project_workspace: Option<String>,
    pub(crate) advanced_open: bool,
    /// 环境变量（stdio）/ 请求头（HTTP）的 JSON 编辑框
    pub(crate) env_headers: Entity<TextareaState>,
    /// env/headers 各留一份草稿：切换传输类型时交换编辑框内容
    pub(crate) env_draft: String,
    pub(crate) headers_draft: String,
    pub(crate) json_mode: bool,
    pub(crate) json_text: Entity<TextareaState>,
    /// 删除两步确认
    pub(crate) delete_armed: bool,
    /// 编辑底稿：原条目 JSON，保存以其为底覆盖表单字段（oauth 等未知字段保真回写）
    pub(crate) base: serde_json::Value,
    /// 校验错误（保存/切换模式失败时填写，显示在页脚上方）
    pub(crate) error: Option<String>,
}

/// 技能新建/编辑对话框（表单编辑 SKILL.md 的 frontmatter + 正文）
pub(crate) struct SkillDialog {
    /// 正在编辑的技能目录名（None = 新建；编辑中名称与作用域锁定）
    pub(crate) editing: Option<String>,
    /// 编辑目标目录（保存原目录重写；新建时 None）
    pub(crate) target_dir: Option<PathBuf>,
    /// 新建时的写入目标层级
    pub(crate) scope: pig_core::skills::SkillSource,
    /// 项目级是否可选（未选工作区时项目级路径未知，不可用）
    pub(crate) project_available: bool,
    /// 项目级目标的工作区显示名（作用域按钮展示「项目级（xxx）」；None = 未知）
    pub(crate) project_workspace: Option<String>,
    pub(crate) name: Entity<InputState>,
    pub(crate) description: Entity<TextareaState>,
    /// frontmatter when_to_use（可选）
    pub(crate) when_to_use: Entity<InputState>,
    /// SKILL.md 正文
    pub(crate) body: Entity<TextareaState>,
    /// 保真回写的额外 frontmatter 键（license 等，来自编辑底稿）
    pub(crate) extra_frontmatter: Vec<(String, String)>,
    /// 删除两步确认
    pub(crate) delete_armed: bool,
    /// 校验错误（保存失败时填写，显示在页脚上方）
    pub(crate) error: Option<String>,
}
