use super::*;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelConfig {
    pub id: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub context_window: u64,
    pub max_output_tokens: u64,
    // Input types (text is always available)
    #[serde(default)]
    pub input_image: bool,
    #[serde(default)]
    pub input_video: bool,
    #[serde(default)]
    pub input_pdf: bool,
    // Capability flags (stored primarily, not all consumed yet)
    #[serde(default)]
    pub cap_structured: bool,
    /// Endpoint honors OpenAI strict function-calling semantics (or Anthropic
    /// strict tool_use): tool schemas are strictified and sent with strict on,
    /// the endpoint then guarantees well-formed tool arguments by constrained
    /// decoding (OpenAI/DeepSeek/GLM chat endpoints support it; Kimi does not)
    #[serde(default)]
    pub cap_strict_tools: bool,
    #[serde(default)]
    pub cap_web_search: bool,
    /// Native web search tool definition: Anthropic defaults to web_search_20250305;
    /// OpenAI-compatible endpoints need explicit configuration (e.g. Zhipu
    /// {"type":"web_search","web_search":{...}})
    #[serde(default)]
    pub web_search_tool: Option<serde_json::Value>,
    #[serde(default)]
    pub cap_system_msg: bool,
    /// Optional reasoning levels, e.g. ["low","high","max"]
    #[serde(default)]
    pub reasoning_levels: Vec<String>,
    /// Default reasoning level: the initial level when a new session specifies none,
    /// or when a model switch's level doesn't fit; None = unset (keep current
    /// behavior: heuristic fallback/off)
    #[serde(default)]
    pub default_reasoning_level: Option<String>,
    /// Level id → UI display name (e.g. max → "highest"); display-only, requests
    /// still merge params by id
    #[serde(default)]
    pub reasoning_labels: std::collections::HashMap<String, String>,
    /// Level → JSON merged into the request body
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
            cap_strict_tools: false,
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
    /// Supports ${ENV_VAR} environment variable references
    pub api_key: String,
    pub api_format: ApiFormat,
    pub enabled: bool,
    pub models: Vec<ModelConfig>,
    /// Key management page URL (brought in by preset providers; shown as the
    /// settings page's "get key" entry, not used in requests)
    #[serde(default)]
    pub key_url: Option<String>,
}

/// Turn token usage: input = cache-miss input tokens, cache_read = cache-hit input
/// tokens (hit rate = cache_read / (input + cache_read)), output = output tokens;
/// duration_ms is the turn's wall clock (including tool execution/approval waits),
/// api_ms is pure provider request time, ttft_ms is the summed time waiting for
/// the first output token and api_steps is the request count (average TTFT =
/// ttft_ms / api_steps); output speed is computed from (api_ms - ttft_ms), i.e.
/// pure decode speed excluding the first token
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

/// models.dev model metadata (for auto-fill)
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelRegistryInfo {
    /// Full ID in the source (provider/model)
    pub full_id: String,
    /// Context window (limit.context, falling back to limit.input when absent)
    pub context: Option<u64>,
    /// Separate max input limit (most models don't have one)
    pub input: Option<u64>,
    /// Max output tokens (limit.output)
    pub output: Option<u64>,
    /// Whether reasoning is supported
    pub reasoning: bool,
    /// Reasoning levels (values of the effort-type entry in reasoning_options;
    /// empty for toggle-only)
    pub reasoning_levels: Vec<String>,
    /// Input modalities (modalities.input, e.g. ["text","image"]); empty = the
    /// source doesn't say, UI leaves capability checkboxes untouched
    #[serde(default)]
    pub input_modalities: Vec<String>,
    /// Structured output support; None = the source doesn't say, UI leaves
    /// checkboxes untouched
    #[serde(default)]
    pub structured_output: Option<bool>,
    /// Strict tool schemas: derived from the provider identity (pi's
    /// capability table — OpenAI/DeepSeek/Z.ai chat endpoints honor strict,
    /// the rest stays untouched); None = leave the checkbox alone
    #[serde(default)]
    pub strict_tools: Option<bool>,
}

/// models.dev only provides level names; the parameter shape is generated per the
/// provider's API format (aligned with ZCode's built-in rules): level names pass
/// through as-is (no normalization); OpenAI Chat → a four-field compatibility
/// bundle (thinking/enable_thinking/reasoning_effort/reasoning.effort, different
/// backends honor different fields); Anthropic Messages → off levels use
/// thinking.type=disabled, on levels use thinking.type=enabled plus
/// output_config.effort (the shape of the GLM-5/DeepSeek-V4/Claude-5 generation;
/// no budget_tokens).
pub fn default_reasoning_params(
    levels: &[String],
    api_format: ApiFormat,
) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    for level in levels {
        let off = level == "none" || level == "disabled";
        let params = match api_format {
            ApiFormat::OpenAiChat => {
                // "enabled" is a toggle-style level, mapped to effort high
                // (same as ZCode's fallback)
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
            // Responses takes a single nested reasoning object (codex sends the
            // same shape); "auto" summaries are what streams reasoning deltas.
            // Off = no reasoning field at all (the API has no effort "none").
            ApiFormat::OpenAiResponses => {
                if off {
                    serde_json::json!({})
                } else {
                    let effort = if level == "enabled" {
                        "high"
                    } else {
                        level.as_str()
                    };
                    serde_json::json!({
                        "reasoning": { "effort": effort, "summary": "auto" },
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
        // none: all four fields in the off state
        assert_eq!(params["none"]["thinking"]["type"], "disabled");
        assert_eq!(params["none"]["enable_thinking"], false);
        assert_eq!(params["none"]["reasoning_effort"], "none");
        // Levels pass through; max is not normalized
        assert_eq!(params["low"]["reasoning_effort"], "low");
        assert_eq!(params["max"]["reasoning_effort"], "max");
        assert_eq!(params["max"]["reasoning"]["effort"], "max");
        assert_eq!(params["max"]["thinking"]["type"], "enabled");
        // Toggle-style level enabled → effort high
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
        // max passes through, no budget mapping
        assert_eq!(params["max"]["output_config"]["effort"], "max");
        let flat = serde_json::to_string(&params).unwrap();
        assert!(
            !flat.contains("budget_tokens"),
            "budget_tokens should no longer be generated"
        );
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AppConfig {
    pub providers: Vec<ProviderConfig>,
    pub default_provider: String,
    pub default_model: String,
    /// UI font family name (GPUI font name, e.g. "PingFang SC");
    /// None = system default (.SystemUIFont); uninstalled fonts are treated as
    /// default (guards against GPUI panic)
    #[serde(default)]
    pub ui_font: Option<String>,
    /// Monospace font family name (code blocks/diff/command lines, e.g.
    /// "JetBrains Mono"); None = platform default (macOS Menlo / Windows Consolas /
    /// Linux DejaVu Sans Mono; the upstream swaps in a fallback when missing)
    #[serde(default)]
    pub mono_font: Option<String>,
    /// Shell path launched by the embedded terminal (e.g. "/bin/zsh",
    /// "/opt/homebrew/bin/fish"); None = system default shell ($SHELL → passwd
    /// login shell / Windows pwsh→powershell→cmd). Takes effect for terminal
    /// tabs created afterwards
    #[serde(default)]
    pub terminal_shell: Option<String>,
    /// UI language ("zh-CN" / "en"); None = follow system (zh* → zh-CN,
    /// everything else → en)
    #[serde(default)]
    pub language: Option<String>,
}
