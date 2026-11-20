use super::*;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelConfig {
    pub id: String,
    /// Optional display name shown in model pickers/chips; empty/None falls
    /// back to the model ID
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Optional brand icon id (a LobeHub catalog slug). None = auto-match from
    /// the model id/display name, falling back to the default model glyph
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub context_window: u64,
    pub max_output_tokens: u64,
    // Input types (text is always available)
    #[serde(default)]
    pub input_image: bool,
    /// PDF document input through the media tool path (ReadMediaFile)
    #[serde(default)]
    pub input_pdf: bool,
    // Capability flags
    #[serde(default)]
    pub cap_structured: bool,
    /// Enable gate for tools that opt into strict schemas. Only declared
    /// `strict: true` tools are strictified (and only when the model/schema
    /// supports it); undeclared tools keep their ordinary schema.
    #[serde(default)]
    pub cap_strict_tools: bool,
    /// Native hosted web search (Responses/Anthropic). Chat Completions can
    /// use search tools contributed by plugins instead.
    #[serde(default)]
    pub cap_web_search: bool,
    /// Native web search tool definition: Anthropic defaults to web_search_20250305;
    /// Responses defaults to {"type":"web_search"} and compatible endpoints can
    /// supply an explicit definition
    #[serde(default)]
    pub web_search_tool: Option<serde_json::Value>,
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
    /// Advanced endpoint-compatibility override for parallel tool calls.
    /// Leave this true for normal use; `false` maps to Chat/Responses
    /// `parallel_tool_calls: false` and Anthropic
    /// `tool_choice.disable_parallel_tool_use: true`.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub parallel_tool_calls: bool,
}

fn default_true() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

impl ModelConfig {
    pub fn new(id: &str, context_window: u64, max_output_tokens: u64) -> Self {
        Self {
            id: id.to_string(),
            name: None,
            icon: None,
            enabled: true,
            context_window,
            max_output_tokens,
            input_image: false,
            input_pdf: false,
            cap_structured: false,
            cap_strict_tools: false,
            cap_web_search: false,
            web_search_tool: None,
            reasoning_levels: vec![],
            default_reasoning_level: None,
            reasoning_labels: Default::default(),
            reasoning_params: Default::default(),
            parallel_tool_calls: true,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub id: String,
    pub name: String,
    pub base_url: String,
    #[serde(default = "default_true")]
    pub requires_api_key: bool,
    pub api_format: ApiFormat,
    pub enabled: bool,
    pub models: Vec<ModelConfig>,
    /// Provider brand icon id (a LobeHub catalog slug such as `deepseek`).
    /// None = auto-match from name/base_url/model ids, falling back to the
    /// settings page's server glyph; not used in requests
    #[serde(default)]
    pub icon: Option<String>,
    /// Key management page URL (brought in by preset providers; shown as the
    /// settings page's "get key" entry, not used in requests)
    #[serde(default)]
    pub key_url: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCredentialStatus {
    NotRequired,
    Configured,
    Missing,
    Unavailable,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn expose_secret(&self) -> &str {
        &self.0
    }

    pub fn into_secret(self) -> String {
        self.0
    }
}

impl From<String> for SecretString {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("<redacted>")
    }
}

/// Turn token usage: input = cache-miss input tokens, cache_read = cache-hit input
/// tokens (hit rate = cache_read / (input + cache_read)), output = output tokens;
/// duration_ms is the turn's wall clock (including tool execution/approval waits),
/// api_ms is model time over the turn's steps (each step from its start to the
/// stream's end), ttft_ms is the summed wait for the first output token and
/// `ttft_steps` the steps that produced one (average TTFT = ttft_ms /
/// ttft_steps); output speed is computed from (api_ms - ttft_ms), i.e. pure
/// decode speed excluding the first token, and a tool call's name or argument
/// delta counts as a first token (the harness's rule)
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
    /// Steps that recorded a first token (the average TTFT divisor)
    #[serde(default)]
    pub ttft_steps: u64,
    #[serde(default)]
    pub api_steps: u64,
    /// Sum of every tool call's own span (permission gate, approval wait
    /// included, plus execution). Concurrent read-only calls overlap, so this
    /// is a sum of spans rather than a wall-clock segment; the harness's tool
    /// time pairs call/result the same way.
    #[serde(default)]
    pub tool_ms: u64,
    /// Wall-clock time the turn ended (unix ms; 0 = unknown, e.g. a record
    /// written before this field existed). The message action row prints it as
    /// the turn's end time; replay restores it from the rollout's TurnStats,
    /// and SQLite keeps the same moment in turn_usage.ended_at_ms.
    #[serde(default)]
    pub ended_at_ms: u64,
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

/// Off-style reasoning levels: these ids mean "thinking disabled". Used by
/// `default_reasoning_params` for the generated off bundle, and by the UI to
/// show no reasoning suffix for them on the composer chip.
pub fn is_off_level(level: &str) -> bool {
    level == "none" || level == "disabled"
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
        let off = is_off_level(level);
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
mod model_config_tests {
    use super::*;

    #[test]
    fn display_name_is_optional_and_round_trips() {
        // Old configs without the field still parse (None = show the model ID)
        let legacy: ModelConfig =
            serde_json::from_str(r#"{"id":"m","context_window":1000,"max_output_tokens":100}"#)
                .unwrap();
        assert!(legacy.name.is_none());
        // Unset names are omitted from the serialized form
        let raw = serde_json::to_string(&legacy).unwrap();
        assert!(
            !raw.contains("\"name\""),
            "unset name must be skipped: {raw}"
        );
        let named = ModelConfig {
            name: Some("Readable name".into()),
            ..legacy
        };
        let back: ModelConfig =
            serde_json::from_str(&serde_json::to_string(&named).unwrap()).unwrap();
        assert_eq!(back.name.as_deref(), Some("Readable name"));
    }

    #[test]
    fn retired_input_keys_are_ignored_and_input_pdf_is_kept() {
        let model: ModelConfig = serde_json::from_str(
            r#"{"id":"m","context_window":1000,"max_output_tokens":100,
                "input_image":true,"input_video":true,"input_pdf":true,
                "cap_system_msg":true}"#,
        )
        .expect("legacy input keys parse");
        assert!(model.input_image);
        assert!(model.input_pdf);
        let serialized = serde_json::to_string(&model).unwrap();
        assert!(!serialized.contains("input_video"), "{serialized}");
        assert!(!serialized.contains("cap_system_msg"), "{serialized}");
        assert!(serialized.contains("input_pdf"), "{serialized}");
    }

    #[test]
    fn parallel_policy_defaults_and_round_trips() {
        let model: ModelConfig =
            serde_json::from_str(r#"{"id":"m","context_window":1000,"max_output_tokens":100}"#)
                .unwrap();
        assert!(model.parallel_tool_calls);
        let raw = serde_json::to_string(&model).unwrap();
        assert!(!raw.contains("parallel_tool_calls"), "{raw}");

        let configured: ModelConfig = serde_json::from_value(serde_json::json!({
            "id": "m",
            "context_window": 1000,
            "max_output_tokens": 100,
            "parallel_tool_calls": false,
        }))
        .unwrap();
        assert!(!configured.parallel_tool_calls);
        let serialized = serde_json::to_value(&configured).unwrap();
        assert_eq!(serialized["parallel_tool_calls"], false);
    }
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

/// Developer-only diagnostics and instrumentation; off for normal use.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeveloperConfig {
    /// Overlay a live frames-per-second readout in the app's corner.
    #[serde(default)]
    pub show_fps: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentsConfig {
    /// Maximum number of delegation edges below a root session. Zero disables
    /// delegation; one preserves the original root → child-only behavior.
    #[serde(default = "default_max_delegation_depth")]
    pub max_delegation_depth: u32,
    /// Maximum distinct child agents in one root session. Resuming an existing
    /// child does not consume another slot; zero prevents new children.
    #[serde(default = "default_max_agents_per_session")]
    pub max_agents_per_session: usize,
}

const fn default_max_delegation_depth() -> u32 {
    1
}

const fn default_max_agents_per_session() -> usize {
    64
}

impl Default for AgentsConfig {
    fn default() -> Self {
        Self {
            max_delegation_depth: default_max_delegation_depth(),
            max_agents_per_session: default_max_agents_per_session(),
        }
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
    /// Saved color theme id registered by the UI; None = the default Pig
    /// light/dark pair. An unknown id falls back to the default theme.
    #[serde(default)]
    pub theme: Option<String>,
    /// Appearance override: "light", "dark", or "system"; None = system.
    #[serde(default)]
    pub theme_mode: Option<String>,
    /// Model-io trace full-input mode (diagnostic mode): skip the tool-result
    /// length cap so each retained request's input is recorded whole. The
    /// per-session trace file size cap still applies and may reset old traces.
    #[serde(default)]
    pub model_io_full_retention: bool,
    /// Default delivery intent for messages sent while a turn is running
    /// (Enter in the composer): Queue = run as the next turn after the current
    /// one ends, Steer = inject into the running turn at the next safe
    /// boundary. Ctrl+Enter always uses the opposite intent
    #[serde(default)]
    pub default_delivery: crate::MessageDelivery,
    /// Subagent delegation limits.
    #[serde(default)]
    pub agents: AgentsConfig,
    /// Lifecycle hooks (see HooksConfig); disabled/empty by default
    #[serde(default)]
    pub hooks: HooksConfig,
    /// Cross-session workspace memory (see MemoryConfig); disabled by default
    #[serde(default)]
    pub memory: MemoryConfig,
    /// Platform-neutral sandbox settings (see SandboxConfig). The read-deny
    /// rules here apply to every backend.
    #[serde(default)]
    pub sandbox: SandboxConfig,
    /// Windows-specific execution settings (see WindowsConfig)
    #[serde(default)]
    pub windows: WindowsConfig,
    /// Developer options (UI diagnostics); off by default
    #[serde(default)]
    pub developer: DeveloperConfig,
}

impl AppConfig {
    /// The read-deny rules the sandbox enforces (`[sandbox] deny_read`).
    pub fn effective_deny_read(&self) -> Vec<String> {
        self.sandbox.deny_read.clone()
    }
}

#[cfg(test)]
mod agents_config_tests {
    use super::*;

    #[test]
    fn delegation_depth_defaults_to_one_and_deserializes() {
        assert_eq!(AppConfig::default().agents.max_delegation_depth, 1);
        assert_eq!(AppConfig::default().agents.max_agents_per_session, 64);
        let config: AppConfig = serde_json::from_value(serde_json::json!({
            "providers": [],
            "default_provider": "",
            "default_model": "",
            "agents": { "max_delegation_depth": 2, "max_agents_per_session": 32 },
        }))
        .expect("agents config");
        assert_eq!(config.agents.max_delegation_depth, 2);
        assert_eq!(config.agents.max_agents_per_session, 32);
    }
}

/// Platform-neutral sandbox settings.
///
/// These are the rules every backend enforces, on every platform. The
/// sensitive-file groups (`.env` family, SSH private keys, cloud credentials)
/// are not built in: a fresh install seeds them into the user's own
/// `config.toml` as `[[sandbox.deny]]` entries, so every one of them is
/// ordinary configuration that can be edited or deleted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxConfig {
    /// Master switch for OS-level command sandboxing across Windows, macOS, and Linux.
    /// Defaults to `true` (fail-closed when enabled on an unsupported host). Set to
    /// `false` in user/system config to explicitly opt out of OS sandboxing; project
    /// configs can never disable it.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Extra read-deny rules: a literal path or a glob (`**/*.pem`,
    /// `secrets/**`). Relative rules anchor at the workspace; `~` expands to
    /// the home directory.
    ///
    /// Shorthand for `filesystem` entries with `deny` access; the settings page
    /// edits this list. Rules from both keys are enforced together.
    #[serde(default)]
    pub deny_read: Vec<String>,
    /// Structured read-deny groups: `[[sandbox.deny]]` tables, each with its
    /// own globs and the exceptions that lift *only* that group's denials:
    ///
    /// ```toml
    /// [[sandbox.deny]]
    /// globs = ["**/.env", "**/.env.*"]
    /// exceptions = ["**/.env.example"]
    /// ```
    ///
    /// `None` (the key absent) means the file predates the seeding and the
    /// defaults are written on the next launch; a present list — even an empty
    /// one — is respected as written, so deleting every entry switches the
    /// protection off. `exceptions` stay readable, `write_exceptions` regain
    /// write access as well.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deny: Option<Vec<SandboxDenyGroup>>,
    /// Filesystem access entries, in Codex's `permissions.<profile>.filesystem`
    /// shape: the key is a path, a glob, a `dir/**` subtree or a special path
    /// (`:root`, `:workspace_roots`, `:tmpdir`), the value is the access to
    /// grant there. Deeper entries win over shallower ones and a denial wins
    /// over a grant on the same path, so
    ///
    /// ```toml
    /// [sandbox.filesystem]
    /// "~/vault" = "deny"
    /// "~/vault/notes.md" = "read"
    /// ```
    ///
    /// denies the directory while keeping that one file readable. A glob key
    /// (`**/*.pem`, `secrets/**`) is deny-only, exactly as in Codex; use a
    /// literal path or a trailing `/**` for a read/write subtree.
    #[serde(default)]
    pub filesystem: std::collections::BTreeMap<String, SandboxFsPermission>,
    /// Directories under the user profile that are denied **whole** in the
    /// model shell's sandbox, relative to the profile (`.ssh`, `.config`,
    /// `AppData/Roaming/gcloud`, …).
    ///
    /// `None` (the key absent) keeps the built-in list; a list replaces it, so
    /// `credential_dirs = []` turns the protection off and adding one entry
    /// means naming the whole set. A directory listed here can only be reached
    /// one file at a time: an approval for a file inside it is offered, an
    /// approval for the directory itself is refused.
    ///
    /// A project-level config cannot change this list (see
    /// `ProjectSandboxLayer`): a repository must not decide which of the user's
    /// credential stores stay protected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_dirs: Option<Vec<String>>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            deny_read: Vec::new(),
            deny: None,
            filesystem: std::collections::BTreeMap::new(),
            credential_dirs: None,
        }
    }
}

/// One `[[sandbox.deny]]` group: the globs that deny, plus the globs that lift
/// this group's denials (and only this group's).
///
/// The exceptions are scoped on purpose. Flattening every rule into one deny
/// list would make a `.env.example` exception lift an unrelated user rule that
/// matches the same path, so an exception travels with the group that owns it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxDenyGroup {
    /// Globs to deny. Matching is case-insensitive, `*` and `?` never cross a
    /// directory separator, and `**/` matches any depth.
    #[serde(default)]
    pub globs: Vec<String>,
    /// Globs this group does **not** deny (`.env.example`, `*.pub`). An
    /// exception restores read access only: it never makes a path writable
    /// where no write grant covers it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exceptions: Vec<String>,
    /// Exceptions that regain write access too (a configured write entry inside
    /// a denied subtree).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub write_exceptions: Vec<String>,
}

/// One `[sandbox.filesystem]` value: either the access itself, or a nested
/// table scoping further paths under the key (Codex's `Scoped` form, which only
/// supports `deny`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SandboxFsPermission {
    Access(SandboxFsAccess),
    Scoped(std::collections::BTreeMap<String, SandboxFsAccess>),
}

/// Filesystem access a `[sandbox.filesystem]` entry grants.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxFsAccess {
    Read,
    Write,
    Deny,
}

/// Windows-only settings. Ignored on other platforms (the file is shared).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct WindowsConfig {
    /// Network access for sandboxed model Bash: `allow` (default) or `block`.
    /// `block` is enforced by PSEC/Seatbelt; `PIGCODE_SANDBOX_NETWORK` still
    /// overrides it.
    #[serde(default)]
    pub sandbox_network: WindowsSandboxNetworkChoice,
}

/// Network access for sandboxed model Bash (`[windows] sandbox_network`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WindowsSandboxNetworkChoice {
    /// Inherit the host's network, matching the default sandbox profile.
    #[default]
    Allow,
    /// Deny all network access; fails closed on tiers that cannot enforce it.
    Block,
}

/// Cross-session workspace memory (ZCode's model-managed memory, ported):
/// one fact per markdown file + a MEMORY.md index under
/// `{data_dir}/memories/projects/{slug}-{hash}/`. The system prompt carries
/// usage instructions + the index (gated by this switch), and the model
/// reads/writes the fact files with its plain file tools. Opt-in (disabled
/// by default, same as ZCode's desktop default).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MemoryConfig {
    /// Master switch: inject the memory section into new sessions and open
    /// the memory directory as an approval-free read-write root
    #[serde(default)]
    pub enabled: bool,
}

/// One configured lifecycle hook. Flat entry (one table = one hook): the
/// event is the TOML array key it lives under (`[[hooks.pre_tool_use]]`).
/// Field order matters for TOML serialization: the `custom` table must stay
/// last (values before tables).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HookEntry {
    /// Command kind: a shell command string (default), or `command` is the
    /// program name run directly as argv with `args` (no shell interpretation)
    #[serde(rename = "type", default, skip_serializing_if = "HookKind::is_command")]
    pub kind: HookKind,
    /// Shell command string (kind=command, run through the session shell: Unix
    /// bash→sh, Windows Git Bash chain) or program name (kind=process, run as
    /// argv); either way the event payload arrives as one-line JSON on stdin
    pub command: String,
    /// argv for kind=process (one element per argument, no shell quoting)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Shell for kind=command. `auto` (default) keeps the session chain
    /// (Windows Git Bash → cmd, Unix bash → sh); an explicit choice runs that
    /// installed shell instead. Invalid for kind=process.
    #[serde(default, skip_serializing_if = "HookShell::is_auto")]
    pub shell: HookShell,
    /// Tool-name matcher (tool events only): None/empty/"*" matches everything;
    /// `^[A-Za-z0-9_|]+$` is an exact pipe-separated name list; anything else
    /// is a regex (an invalid regex never matches). SessionStart matches
    /// against its source; UserPromptSubmit/Stop ignore the matcher
    #[serde(default)]
    pub matcher: Option<String>,
    /// Per-hook timeout override in ms (falls back to HooksConfig.timeout_ms,
    /// hard cap 600000)
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Fire-and-forget: the process is spawned but its output and any decision
    /// fields are ignored (terminal state is only logged)
    #[serde(default)]
    pub r#async: bool,
    /// Entry switch (default true)
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Optional label shown in the UI while the hook runs
    #[serde(default)]
    pub status_message: Option<String>,
    /// Runtime-injected provenance; never parsed from or written to disk (the
    /// user config keeps source unset, the core-side merged snapshot annotates)
    #[serde(skip)]
    pub source: Option<HookSource>,
    /// Extension fields the runtime ignores but round-trips (ZCode `custom`;
    /// kept as JSON so the settings form edits it verbatim)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom: Option<serde_json::Map<String, serde_json::Value>>,
}

/// How a hook entry's command runs
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookKind {
    /// `command` is a shell string (session shell resolution, `sh -c` style)
    #[default]
    Command,
    /// `command` is a program name spawned directly with `args` (no shell)
    Process,
}

impl HookKind {
    fn is_command(&self) -> bool {
        matches!(self, HookKind::Command)
    }
}

/// Shell used to run a `kind = "command"` hook entry.
///
/// `auto` keeps the host chain (Windows Git Bash → cmd, Unix bash → sh). The
/// explicit values are platform-gated: the settings UI lists only shells
/// installed on the host, and a configured value that is missing at run time
/// fails the hook instead of silently falling back.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HookShell {
    #[default]
    Auto,
    /// Windows: Git for Windows' bash (the auto default there)
    GitBash,
    /// Windows: cmd.exe
    Cmd,
    /// Windows PowerShell (powershell.exe)
    #[serde(rename = "powershell")]
    PowerShell,
    /// PowerShell 7 (pwsh)
    Pwsh,
    /// Unix: bash
    Bash,
    /// Unix: sh
    Sh,
    /// Unix: zsh
    Zsh,
}

impl HookShell {
    fn is_auto(&self) -> bool {
        matches!(self, Self::Auto)
    }

    /// Canonical config value and UI label.
    pub fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::GitBash => "git-bash",
            Self::Cmd => "cmd",
            Self::PowerShell => "powershell",
            Self::Pwsh => "pwsh",
            Self::Bash => "bash",
            Self::Sh => "sh",
            Self::Zsh => "zsh",
        }
    }
}

/// Where a hook entry was declared (runtime annotation on HookEntry)
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookSource {
    /// ~/.pigcode/config.toml
    User,
    /// .pigcode/hooks.toml (gated by the workspace trust dialog)
    Project,
    /// An enabled package's contribution. The identity is re-checked on every
    /// run, so disabling, updating or uninstalling the package revokes it.
    Plugin { package: String, digest: String },
}

/// Lifecycle hooks configuration (Claude Code-compatible stdin/exit-code
/// contract). User level lives under `[hooks]` in ~/.pigcode/config.toml; the
/// project file `.pigcode/hooks.toml` uses the same `[hooks]` root shape and
/// is merged per event (user entries first, project entries after). Global
/// knobs (enabled/timeout_ms/max_output_bytes) come from the user level only.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HooksConfig {
    /// Master switch (default false): when off no hook runs at all, including
    /// trusted project hooks
    #[serde(default)]
    pub enabled: bool,
    /// Default per-hook timeout in ms
    #[serde(default = "default_hook_timeout_ms")]
    pub timeout_ms: u64,
    /// Per-stream stdout/stderr capture cap in bytes
    #[serde(default = "default_hook_max_output_bytes")]
    pub max_output_bytes: usize,
    /// Canonical workspace root paths whose .pigcode/hooks.toml has been
    /// trusted through the first-use confirmation dialog
    #[serde(default)]
    pub trusted_projects: Vec<String>,
    #[serde(default)]
    pub session_start: Vec<HookEntry>,
    #[serde(default)]
    pub user_prompt_submit: Vec<HookEntry>,
    #[serde(default)]
    pub pre_tool_use: Vec<HookEntry>,
    #[serde(default)]
    pub permission_request: Vec<HookEntry>,
    #[serde(default)]
    pub post_tool_use: Vec<HookEntry>,
    #[serde(default)]
    pub post_tool_use_failure: Vec<HookEntry>,
    #[serde(default)]
    pub stop: Vec<HookEntry>,
}

fn default_hook_timeout_ms() -> u64 {
    60_000
}

fn default_hook_max_output_bytes() -> usize {
    32 * 1024
}

impl Default for HooksConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            timeout_ms: default_hook_timeout_ms(),
            max_output_bytes: default_hook_max_output_bytes(),
            trusted_projects: Vec::new(),
            session_start: Vec::new(),
            user_prompt_submit: Vec::new(),
            pre_tool_use: Vec::new(),
            permission_request: Vec::new(),
            post_tool_use: Vec::new(),
            post_tool_use_failure: Vec::new(),
            stop: Vec::new(),
        }
    }
}

#[cfg(test)]
mod windows_config_tests {
    use super::*;

    fn parse(windows: &str) -> AppConfig {
        let json = format!(
            r#"{{"providers":[],"default_provider":"","default_model":"","windows":{{{windows}}}}}"#
        );
        serde_json::from_str(&json).expect("windows config parses")
    }

    fn parse_sandbox(sandbox_json: &str) -> SandboxConfig {
        let json = format!(
            r#"{{"providers":[],"default_provider":"","default_model":"","sandbox":{sandbox_json}}}"#
        );
        serde_json::from_str::<AppConfig>(&json)
            .expect("sandbox config parses")
            .sandbox
    }

    /// The `[sandbox.filesystem]` table is Codex's `filesystem` map: a path,
    /// glob or special path maps to an access, and a nested table scopes
    /// further paths under the key.
    #[test]
    fn filesystem_entries_parse_in_codex_shape() {
        let sandbox = parse_sandbox(
            r#"{"filesystem":{
                "/abs/vault":"deny",
                "~/notes":"read",
                "secrets/**":"deny",
                "**/*.pem":"deny",
                ":root":"read",
                "/abs/tree":{"sub":"deny"}
            }}"#,
        );
        assert_eq!(
            sandbox.filesystem.get("/abs/vault"),
            Some(&SandboxFsPermission::Access(SandboxFsAccess::Deny))
        );
        assert_eq!(
            sandbox.filesystem.get("~/notes"),
            Some(&SandboxFsPermission::Access(SandboxFsAccess::Read))
        );
        assert_eq!(
            sandbox.filesystem.get("**/*.pem"),
            Some(&SandboxFsPermission::Access(SandboxFsAccess::Deny))
        );
        assert_eq!(
            sandbox.filesystem.get(":root"),
            Some(&SandboxFsPermission::Access(SandboxFsAccess::Read))
        );
        assert_eq!(
            sandbox.filesystem.get("/abs/tree"),
            Some(&SandboxFsPermission::Scoped(
                std::collections::BTreeMap::from([("sub".to_string(), SandboxFsAccess::Deny)])
            ))
        );
    }

    /// The credential list distinguishes "absent" (keep the built-in list) from
    /// "explicitly empty" (turn the protection off).
    #[test]
    fn the_credential_list_distinguishes_absent_from_empty() {
        assert_eq!(parse_sandbox("{}").credential_dirs, None);
        assert_eq!(
            parse_sandbox(r#"{"credential_dirs":[]}"#).credential_dirs,
            Some(Vec::new())
        );
        assert_eq!(
            parse_sandbox(r#"{"credential_dirs":[".ssh",".config"]}"#).credential_dirs,
            Some(vec![".ssh".to_string(), ".config".to_string()])
        );
    }

    /// `[[sandbox.deny]]` keeps "key absent" (the seeding writes the defaults
    /// on the next launch) apart from "present and empty" (the user deleted
    /// every group deliberately).
    #[test]
    fn deny_groups_distinguish_absent_from_empty() {
        assert_eq!(parse_sandbox("{}").deny, None);
        assert_eq!(parse_sandbox(r#"{"deny":[]}"#).deny, Some(Vec::new()));

        let sandbox = parse_sandbox(
            r#"{"deny":[{"globs":["**/.env","**/.env.*"],
                          "exceptions":["**/.env.example"],
                          "write_exceptions":["**/.env.example"]}]}"#,
        );
        assert_eq!(
            sandbox.deny,
            Some(vec![SandboxDenyGroup {
                globs: vec!["**/.env".to_string(), "**/.env.*".to_string()],
                exceptions: vec!["**/.env.example".to_string()],
                write_exceptions: vec!["**/.env.example".to_string()],
            }])
        );
    }

    /// A config from a build that still had the built-in switch loads fine: the
    /// retired key is unknown and therefore ignored (no shipped users, so no
    /// migration is attempted).
    #[test]
    fn a_retired_builtin_switch_is_ignored() {
        assert_eq!(
            parse_sandbox(r#"{"disabled_builtin_deny":["env"]}"#).deny,
            None
        );
    }

    /// An absent key stays absent when the config is written back, so a save
    /// can never turn "seed me" into "the user deleted every group".
    #[test]
    fn an_absent_deny_key_is_not_serialized() {
        let value = serde_json::to_value(SandboxConfig::default()).expect("serialize");
        assert!(value.get("deny").is_none(), "{value}");
        let present = SandboxConfig {
            deny: Some(Vec::new()),
            ..SandboxConfig::default()
        };
        let value = serde_json::to_value(present).expect("serialize");
        assert_eq!(value.get("deny"), Some(&serde_json::json!([])));
    }

    /// An unknown access word is a load error rather than a silent grant.
    #[test]
    fn an_unknown_access_word_is_rejected() {
        let json = r#"{"providers":[],"default_provider":"","default_model":"",
            "sandbox":{"filesystem":{"/abs":"maybe"}}}"#;
        assert!(
            serde_json::from_str::<AppConfig>(json).is_err(),
            "an unknown access word must not parse"
        );
    }

    #[test]
    fn sandbox_network_defaults_when_absent() {
        let config = parse("");
        assert_eq!(
            config.windows.sandbox_network,
            WindowsSandboxNetworkChoice::Allow
        );
    }

    /// Keys from builds that still had a selectable sandbox tier or shell are
    /// ignored instead of failing the load: the Windows tier is MXC's process
    /// security environment, and the sandboxed shell is the PowerShell chain.
    #[test]
    fn retired_windows_keys_are_ignored() {
        let config = parse(r#""sandbox":"elevated","sandbox_shell":"git-bash""#);
        assert_eq!(
            config.windows.sandbox_network,
            WindowsSandboxNetworkChoice::Allow
        );
    }

    #[test]
    fn sandbox_network_accepts_allow_and_block() {
        assert_eq!(
            parse(r#""sandbox_network":"block""#)
                .windows
                .sandbox_network,
            WindowsSandboxNetworkChoice::Block
        );
        assert_eq!(
            parse(r#""sandbox_network":"allow""#)
                .windows
                .sandbox_network,
            WindowsSandboxNetworkChoice::Allow
        );
    }
}

#[cfg(test)]
mod hook_shell_config_tests {
    use super::*;

    #[test]
    fn hook_shell_defaults_to_auto_and_only_explicit_values_serialize() {
        let legacy: HookEntry =
            serde_json::from_str(r#"{"command":"echo hi"}"#).expect("legacy entry");
        assert_eq!(legacy.shell, HookShell::Auto);

        let explicit: HookEntry =
            serde_json::from_str(r#"{"command":"echo hi","shell":"git-bash"}"#)
                .expect("explicit shell");
        assert_eq!(explicit.shell, HookShell::GitBash);

        let powershell: HookEntry =
            serde_json::from_str(r#"{"command":"echo hi","shell":"powershell"}"#)
                .expect("powershell shell");
        assert_eq!(powershell.shell, HookShell::PowerShell);

        let auto = serde_json::to_string(&legacy).expect("serialize auto");
        assert!(!auto.contains("shell"), "auto is not written: {auto}");
        let value = serde_json::to_string(&explicit).expect("serialize explicit");
        assert!(
            value.contains(r#""shell":"git-bash""#),
            "explicit shell round-trips: {value}"
        );
    }
}
