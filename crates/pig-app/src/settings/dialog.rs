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
