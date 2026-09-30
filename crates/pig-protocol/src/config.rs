use super::*;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelConfig {
    pub id: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub context_window: u64,
    pub max_output_tokens: u64,
    // 输入类型（文本恒有）
    #[serde(default)]
    pub input_image: bool,
    #[serde(default)]
    pub input_video: bool,
    #[serde(default)]
    pub input_pdf: bool,
    // 能力标记（存储为主，暂不全部消费）
    #[serde(default)]
    pub cap_structured: bool,
    #[serde(default)]
    pub cap_web_search: bool,
    /// 原生联网搜索工具定义：Anthropic 缺省 web_search_20250305；OpenAI 兼容
    /// 端点需显式配置（如智谱 {"type":"web_search","web_search":{...}}）
    #[serde(default)]
    pub web_search_tool: Option<serde_json::Value>,
    #[serde(default)]
    pub cap_system_msg: bool,
    /// 可选推理等级，如 ["low","high","max"]
    #[serde(default)]
    pub reasoning_levels: Vec<String>,
    /// 默认思考等级：新会话未指定等级、切换模型等级不适配时的初始档；
    /// None = 未设置（沿用现状：启发式兜底/关）
    #[serde(default)]
    pub default_reasoning_level: Option<String>,
    /// 等级 id → 界面显示名（如 max → "最高"）；纯展示层，请求仍按 id 合并参数
    #[serde(default)]
    pub reasoning_labels: std::collections::HashMap<String, String>,
    /// 等级 → 合并进请求体的 JSON
    #[serde(default)]
    pub reasoning_params: std::collections::HashMap<String, serde_json::Value>,
}

fn default_true() -> bool {
    true
}

impl ModelConfig {
    pub fn new(id: &str, context_window: u64, max_output_tokens: u64) -> Self {
        Self {
            id: id.to_string(),
            enabled: true,
            context_window,
            max_output_tokens,
            input_image: false,
            input_video: false,
            input_pdf: false,
            cap_structured: false,
            cap_web_search: false,
            web_search_tool: None,
            cap_system_msg: false,
            reasoning_levels: vec![],
            default_reasoning_level: None,
            reasoning_labels: Default::default(),
            reasoning_params: Default::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub id: String,
    pub name: String,
    pub base_url: String,
    /// 支持 ${ENV_VAR} 引用环境变量
    pub api_key: String,
    pub api_format: ApiFormat,
    pub enabled: bool,
    pub models: Vec<ModelConfig>,
}

/// 回合 token 用量：input = 未缓存命中的输入，cache_read = 缓存命中的输入
///（命中率 = cache_read / (input + cache_read)），output = 输出；
/// duration_ms 为回合墙钟耗时（含工具执行/审批等待），api_ms 为纯 provider
/// 请求耗时，ttft_ms 为其中等待首个输出 token 的时间之和、api_steps 为请求
/// 次数（平均首字 = ttft_ms / api_steps）；输出速度按 (api_ms - ttft_ms)
/// 计算，即不含首字的纯解码速度
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TurnUsageStats {
    pub input: u64,
    pub cache_read: u64,
    pub output: u64,
    pub duration_ms: u64,
    #[serde(default)]
    pub api_ms: u64,
    #[serde(default)]
    pub ttft_ms: u64,
    #[serde(default)]
    pub api_steps: u64,
}

/// models.dev 的模型元数据（自动填充用）
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelRegistryInfo {
    /// 数据源里的完整 ID（provider/model）
    pub full_id: String,
    /// 上下文窗口（limit.context，缺省回退 limit.input）
    pub context: Option<u64>,
    /// 单独的最大输入限制（多数模型没有）
    pub input: Option<u64>,
    /// 最大输出 token（limit.output）
    pub output: Option<u64>,
    /// 是否支持推理
    pub reasoning: bool,
    /// 推理等级（reasoning_options 里 effort 类型的 values；仅 toggle 的为空）
    pub reasoning_levels: Vec<String>,
    /// 输入模态（modalities.input，如 ["text","image"]）；空 = 数据源未给，UI 不动能力勾选
    #[serde(default)]
    pub input_modalities: Vec<String>,
    /// 结构化输出支持；None = 数据源未给，UI 不动勾选
    #[serde(default)]
    pub structured_output: Option<bool>,
}

/// models.dev 只给等级名，参数形态按供应商 API 格式生成（对齐 ZCode 内置规则）：
/// 等级名直接透传（不归一）；OpenAI Chat → 四字段兼容包（thinking/enable_thinking/
/// reasoning_effort/reasoning.effort，不同后端认不同字段）；Anthropic Messages →
/// 关档 thinking.type=disabled，开档 thinking.type=enabled + output_config.effort
/// （GLM-5/DeepSeek-V4/Claude-5 这代模型的形态；不使用 budget_tokens）。
pub fn default_reasoning_params(
    levels: &[String],
    api_format: ApiFormat,
) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    for level in levels {
        let off = level == "none" || level == "disabled";
        let params = match api_format {
            ApiFormat::OpenAiChat => {
                // "enabled" 是开关型等级，落到 effort high（ZCode 兜底同款）
                let effort = if off {
                    "none"
                } else if level == "enabled" {
                    "high"
                } else {
                    level.as_str()
                };
                serde_json::json!({
                    "thinking": { "type": if off { "disabled" } else { "enabled" } },
                    "enable_thinking": !off,
                    "reasoning_effort": effort,
                    "reasoning": { "effort": effort },
                })
            }
            ApiFormat::AnthropicMessages => {
                if off {
                    serde_json::json!({ "thinking": { "type": "disabled" } })
                } else {
                    let effort = if level == "enabled" {
                        "high"
                    } else {
                        level.as_str()
                    };
                    serde_json::json!({
                        "thinking": { "type": "enabled" },
                        "output_config": { "effort": effort },
                    })
                }
            }
        };
        map.insert(level.clone(), params);
    }
    map
}

#[cfg(test)]
mod reasoning_params_tests {
    use super::*;

    #[test]
    fn openai_params_pass_level_through_with_compat_fields() {
        let levels: Vec<String> = ["none", "low", "max", "enabled"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let params = default_reasoning_params(&levels, ApiFormat::OpenAiChat);
        // none：四字段全关档
        assert_eq!(params["none"]["thinking"]["type"], "disabled");
        assert_eq!(params["none"]["enable_thinking"], false);
        assert_eq!(params["none"]["reasoning_effort"], "none");
        // 等级透传，max 不归一
        assert_eq!(params["low"]["reasoning_effort"], "low");
        assert_eq!(params["max"]["reasoning_effort"], "max");
        assert_eq!(params["max"]["reasoning"]["effort"], "max");
        assert_eq!(params["max"]["thinking"]["type"], "enabled");
        // 开关型等级 enabled → effort high
        assert_eq!(params["enabled"]["reasoning_effort"], "high");
    }

    #[test]
    fn anthropic_params_use_output_config_effort_without_budget() {
        let levels: Vec<String> = ["none", "low", "max"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let params = default_reasoning_params(&levels, ApiFormat::AnthropicMessages);
        assert_eq!(params["none"]["thinking"]["type"], "disabled");
        assert!(params["none"].get("output_config").is_none());
        assert_eq!(params["low"]["thinking"]["type"], "enabled");
        assert_eq!(params["low"]["output_config"]["effort"], "low");
        // max 透传，不映射预算
        assert_eq!(params["max"]["output_config"]["effort"], "max");
        let flat = serde_json::to_string(&params).unwrap();
        assert!(!flat.contains("budget_tokens"), "不再生成 budget_tokens");
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AppConfig {
    pub providers: Vec<ProviderConfig>,
    pub default_provider: String,
    pub default_model: String,
    /// 界面字体家族名（GPUI 字体名，如 "PingFang SC"）；
    /// None = 系统默认（.SystemUIFont），未安装的字体按默认处理（防 GPUI panic）
    #[serde(default)]
    pub ui_font: Option<String>,
    /// 等宽字体家族名（代码块/diff/命令行，如 "JetBrains Mono"）；None = 平台默认
    /// （macOS Menlo / Windows Consolas / Linux DejaVu Sans Mono，缺装时上游自动换备选）
    #[serde(default)]
    pub mono_font: Option<String>,
}
