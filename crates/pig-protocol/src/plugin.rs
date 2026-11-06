use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::HookEntry;

pub const PLUGIN_API_VERSION: u32 = 1;

/// The host's plugin-capability version, checked against a manifest's
/// `minHostVersion`. It tracks the app version in `nsh.json`: bump it when the
/// host contract changes in a way a plugin must opt into.
pub const PLUGIN_HOST_VERSION: &str = "1.0.0";

const fn api_version() -> u32 {
    PLUGIN_API_VERSION
}

const fn default_enabled() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginContributions {
    #[serde(default = "api_version")]
    pub api_version: u32,
    #[serde(default)]
    pub description: BTreeMap<String, String>,
    #[serde(default)]
    pub themes: Vec<PluginThemeContribution>,
    #[serde(default)]
    pub skills: Vec<PluginSkill>,
    #[serde(default)]
    pub mcp: Vec<PluginMcpServer>,
    #[serde(default)]
    pub tools: Vec<PluginTool>,
    #[serde(default)]
    pub commands: Vec<PluginCommand>,
    #[serde(default)]
    pub hooks: Vec<PluginHook>,
    #[serde(default)]
    pub agents: Vec<PluginAgentProfile>,
    #[serde(default)]
    pub credentials: Vec<PluginCredentialDeclaration>,
    #[serde(default)]
    pub http: Vec<PluginHttpOrigin>,
    #[serde(default)]
    pub config: PluginConfigDeclaration,
    #[serde(default)]
    pub actions: Vec<PluginCapability>,
}

impl PluginContributions {
    /// The manifest's declarations with the backend's runtime ones on top:
    /// a runtime entry replaces the manifest entry of the same identity, so a
    /// backend that refines its declared tool/command/hook does not appear
    /// twice to the host.
    pub fn merged(&self, runtime: &Self) -> Self {
        let mut merged = self.clone();
        merge_replacing(&mut merged.themes, &runtime.themes, |theme| {
            theme.id.clone()
        });
        merge_replacing(&mut merged.skills, &runtime.skills, |skill| {
            skill.path.clone()
        });
        merge_replacing(&mut merged.mcp, &runtime.mcp, |server| server.name.clone());
        merge_replacing(&mut merged.tools, &runtime.tools, |tool| tool.name.clone());
        merge_replacing(&mut merged.commands, &runtime.commands, |command| {
            command.id.clone()
        });
        merge_replacing(&mut merged.hooks, &runtime.hooks, |hook| {
            hook.id
                .clone()
                .unwrap_or_else(|| format!("{}:{}", hook.event.name(), hook.entry.command))
        });
        merge_replacing(&mut merged.agents, &runtime.agents, |agent| {
            agent.name.clone()
        });
        merged
    }
}

fn merge_replacing<T, F>(items: &mut Vec<T>, additions: &[T], key: F)
where
    T: Clone,
    F: Fn(&T) -> String,
{
    for addition in additions {
        let id = key(addition);
        items.retain(|existing| key(existing) != id);
        items.push(addition.clone());
    }
}

impl Default for PluginContributions {
    fn default() -> Self {
        Self {
            api_version: PLUGIN_API_VERSION,
            description: BTreeMap::new(),
            themes: Vec::new(),
            skills: Vec::new(),
            mcp: Vec::new(),
            tools: Vec::new(),
            commands: Vec::new(),
            hooks: Vec::new(),
            agents: Vec::new(),
            credentials: Vec::new(),
            http: Vec::new(),
            config: PluginConfigDeclaration::default(),
            actions: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginConfigDeclaration {
    #[serde(default)]
    pub items: Vec<PluginConfigItem>,
    #[serde(default)]
    pub read: Vec<String>,
    #[serde(default)]
    pub write: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginConfigItem {
    pub key: String,
    #[serde(default)]
    pub description: BTreeMap<String, String>,
    pub default: serde_json::Value,
    /// JSON Schema of the value (the host validates writes and renders its
    /// generic form from it). `None` accepts any value of the default's type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginThemeContribution {
    pub id: String,
    pub entry: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginThemeKind {
    #[default]
    Theme,
    Style,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginThemeCssVars {
    #[serde(default)]
    pub theme: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub light: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub dark: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginThemePart {
    #[serde(default)]
    pub class: Option<String>,
    #[serde(default)]
    pub style: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginThemeBackground {
    pub asset: String,
    #[serde(default)]
    pub opacity: Option<f32>,
    #[serde(default)]
    pub blur: Option<f32>,
    #[serde(default)]
    pub overlay: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginThemeDefinition {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub kind: PluginThemeKind,
    #[serde(default, rename = "targetThemes")]
    pub target_themes: Vec<String>,
    #[serde(default, rename = "cssVars")]
    pub css_vars: PluginThemeCssVars,
    #[serde(default)]
    pub parts: BTreeMap<String, PluginThemePart>,
    #[serde(default)]
    pub background: Option<PluginThemeBackground>,
}

pub const PLUGIN_THEME_SLOTS: &[&str] = &[
    "app.root",
    "app.background",
    "app.content",
    "title-bar.root",
    "sidebar.root",
    "sidebar.session",
    "composer.root",
    "composer.input",
    "chat.user",
    "chat.markdown",
    "markdown.code-block",
    "markdown.inline-code",
    "diff.root",
    "diff.gutter",
    "diff.line-added",
    "diff.line-deleted",
    "diff.word-added",
    "diff.word-deleted",
];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginSkill {
    pub path: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginMcpServer {
    pub name: String,
    pub config: serde_json::Value,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginToolAvailability {
    #[default]
    Static,
    Plugin,
}

/// Whether a plugin tool asks the user before it runs. `on_request` keeps the
/// host's ordinary rules (read-only tools never ask; otherwise the current
/// approval mode decides). `always` asks even where the mode would not, and
/// `never` is accepted for read-only tools only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginToolApproval {
    #[default]
    OnRequest,
    Always,
    Never,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginTool {
    pub name: String,
    pub description: String,
    /// JSON Schema of the arguments, validated by the host before the call.
    pub parameters: serde_json::Value,
    /// JSON Schema of the result value, validated by the host after the call.
    /// `None` accepts any JSON value within the size limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<serde_json::Value>,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub availability: PluginToolAvailability,
    /// Whether repeating the same call is safe (the host may retry it).
    #[serde(default)]
    pub idempotent: bool,
    #[serde(default)]
    pub approval: PluginToolApproval,
    /// Wall-clock budget of one call; clamped by the host (default 60000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Calls of this tool allowed in flight at once; clamped by the host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrency: Option<u32>,
    /// Inline result budget. Beyond it the full result is written to the
    /// session's tool-results directory and only a preview reaches the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_bytes: Option<u64>,
}

impl PluginTool {
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(
            self.timeout_ms
                .unwrap_or(PLUGIN_TOOL_DEFAULT_TIMEOUT_MS)
                .clamp(1, PLUGIN_TOOL_MAX_TIMEOUT_MS),
        )
    }

    pub fn concurrency(&self) -> usize {
        self.max_concurrency
            .unwrap_or(PLUGIN_TOOL_DEFAULT_CONCURRENCY as u32)
            .clamp(1, PLUGIN_TOOL_MAX_CONCURRENCY as u32) as usize
    }

    pub fn output_budget(&self) -> usize {
        self.max_output_bytes
            .unwrap_or(PLUGIN_TOOL_DEFAULT_OUTPUT_BYTES)
            .clamp(PLUGIN_TOOL_MIN_OUTPUT_BYTES, PLUGIN_TOOL_MAX_OUTPUT_BYTES) as usize
    }
}

pub const PLUGIN_TOOL_DEFAULT_TIMEOUT_MS: u64 = 60_000;
pub const PLUGIN_TOOL_MAX_TIMEOUT_MS: u64 = 600_000;
pub const PLUGIN_TOOL_DEFAULT_CONCURRENCY: usize = 8;
pub const PLUGIN_TOOL_MAX_CONCURRENCY: usize = 32;
pub const PLUGIN_TOOL_DEFAULT_OUTPUT_BYTES: u64 = 32 * 1024;
pub const PLUGIN_TOOL_MIN_OUTPUT_BYTES: u64 = 1024;
pub const PLUGIN_TOOL_MAX_OUTPUT_BYTES: u64 = 1024 * 1024;

/// The closed set of host hook events a package may contribute to. It mirrors
/// the user/project hook contract; a name outside it is rejected at install.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginHookEvent {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PermissionRequest,
    PostToolUse,
    PostToolUseFailure,
    Stop,
}

impl PluginHookEvent {
    /// Claude-form event name (the runner's `HookEvent::name`).
    pub const fn name(self) -> &'static str {
        match self {
            Self::SessionStart => "SessionStart",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::PreToolUse => "PreToolUse",
            Self::PermissionRequest => "PermissionRequest",
            Self::PostToolUse => "PostToolUse",
            Self::PostToolUseFailure => "PostToolUseFailure",
            Self::Stop => "Stop",
        }
    }
}

/// One hook entry a package contributes. `entry.source` is annotation-only and
/// always overwritten by the runtime with the package identity.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PluginHook {
    /// Stable id for a runtime-registered hook (a disposer removes it by this
    /// id). Manifest hooks may omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub event: PluginHookEvent,
    #[serde(flatten)]
    pub entry: HookEntry,
}

/// A subagent profile a package contributes (the same fields a Markdown
/// profile in `{data_dir}/agents` or `.pigcode/agents` declares).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginAgentProfile {
    /// `^[a-zA-Z0-9-]{3,50}$`, unique across the effective profile set.
    pub name: String,
    pub description: String,
    /// `None` or `["*"]` = every child tool; an explicit list narrows it.
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    /// `None` = the parent session's model; otherwise `providerId/modelId` or
    /// a bare model id of the default provider.
    #[serde(default)]
    pub model: Option<String>,
    /// Only effective together with an explicit model.
    #[serde(default)]
    pub thought_level: Option<String>,
    #[serde(default)]
    pub max_turns: Option<usize>,
    #[serde(default = "default_enabled")]
    pub inject_agents_md: bool,
    pub system_prompt: String,
}

/// A subagent profile projected for `agent.list` (no prompt body).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginAgentProfileInfo {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    #[serde(default)]
    pub model: Option<String>,
    /// `built_in`, `plugin`, `user` or `project`.
    pub source: String,
}

/// The immediate answer of `agent.start`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginAgentStarted {
    pub agent_id: String,
    pub task_id: String,
    pub profile: String,
    pub model: String,
    pub background: bool,
    pub status: String,
}

/// The settled state of a delegated subagent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginAgentOutcome {
    pub agent_id: String,
    /// `completed`, `stopped`, `failed` or `timeout`.
    pub status: String,
    /// The final result, trimmed for the plugin (the full text stays in
    /// `result_path` when the host has one).
    pub summary: String,
    #[serde(default)]
    pub result_path: Option<String>,
}

/// A read-only projection of a delegated subagent's conversation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginAgentConversation {
    pub agent_id: String,
    pub title: String,
    pub running: bool,
    pub items: Vec<crate::SubagentItem>,
}

/// A controlled one-shot model request from a plugin.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginModelResponse {
    pub text: String,
    #[serde(default)]
    pub usage: Option<PluginModelUsage>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginModelUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// One projected session event in a subscription.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PluginSessionEvent {
    Configured {
        model: String,
        provider_name: String,
        permissions: crate::PermissionSettings,
    },
    TurnStarted {
        turn_id: String,
    },
    UserMessage {
        text: String,
        files: usize,
    },
    AssistantText {
        text: String,
    },
    ToolStarted {
        item_id: String,
        tool: String,
        summary: String,
    },
    ToolFinished {
        item_id: String,
        is_error: bool,
        preview: String,
    },
    TurnComplete {
        duration_ms: u64,
        finish_reason: Option<crate::FinishReason>,
        stats: Option<crate::TurnUsageStats>,
    },
    TurnAborted,
    Error {
        fatal: bool,
        error: crate::CoreError,
    },
    Compacted {
        automatic: bool,
    },
    Agent {
        agent_id: String,
        profile: String,
        description: String,
        background: bool,
    },
    Permissions {
        permissions: crate::PermissionSettings,
    },
    PlanMode {
        enabled: bool,
    },
}

/// The answer to `session.subscribe`; `service.event` messages named
/// `session.event` then carry `{subscription, epoch, cursor, dropped, event}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginSessionSubscription {
    pub subscription: String,
    pub session_id: String,
    pub epoch: u64,
    pub cursor: u64,
    #[serde(default)]
    pub dropped: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginCommand {
    pub id: String,
    pub title: BTreeMap<String, String>,
    #[serde(default)]
    pub export: Option<String>,
    #[serde(default)]
    pub order: u32,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PluginCredentialDeclaration {
    Owned {
        id: String,
        label: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
    Provider {
        id: String,
        provider_id: String,
    },
}

impl PluginCredentialDeclaration {
    pub fn id(&self) -> &str {
        match self {
            Self::Owned { id, .. } | Self::Provider { id, .. } => id,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PluginCredentialGrant {
    pub package: String,
    pub digest: String,
    pub credential: String,
    pub provider_id: String,
    pub origin: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginHttpOrigin {
    pub id: String,
    pub url: String,
    pub methods: Vec<String>,
    #[serde(default)]
    pub credentials: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginHttpCredential {
    pub credential: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json_pointer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginHttpRequest {
    pub origin: String,
    pub method: String,
    #[serde(default)]
    pub query: BTreeMap<String, String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<PluginHttpCredential>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginHttpResponse {
    pub status: u16,
    pub body: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginModelCapabilities {
    pub native_web_search: bool,
    pub input_image: bool,
    pub input_pdf: bool,
    pub structured_output: bool,
    pub strict_tools: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginModelContext {
    pub provider_name: String,
    pub model_id: String,
    pub api_format: crate::ApiFormat,
    pub context_window: u64,
    pub max_output_tokens: u64,
    pub capabilities: PluginModelCapabilities,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PluginToolsQuery {
    pub request_id: String,
    pub session_id: String,
    pub package: String,
    pub digest: String,
    #[serde(default)]
    pub agent_id: Option<String>,
    pub model: PluginModelContext,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PluginToolsSelection {
    pub request_id: String,
    pub session_id: String,
    pub package: String,
    pub digest: String,
    pub result: Result<Vec<String>, crate::PluginProtocolError>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PluginToolCall {
    pub request_id: String,
    pub session_id: String,
    pub package: String,
    pub digest: String,
    pub tool: String,
    pub args: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PluginToolResult {
    pub request_id: String,
    pub session_id: String,
    pub package: String,
    pub digest: String,
    pub result: Result<serde_json::Value, crate::PluginProtocolError>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginCapability {
    ReadSettings,
    Navigate,
    ManageSkills,
    ManageMcp,
    ProvideMcp,
    ProvideTools,
    ProvideThemes,
    ProvideHooks,
    ProvideAgents,
    Http,
    ManageOwnSecrets,
    ReadConfig,
    WriteConfig,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PluginPackageInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    pub digest: String,
    #[serde(default)]
    pub content_digest: String,
    pub root: PathBuf,
    pub source: PluginSource,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Set when the manifest's `minHostVersion` is newer than this host. The
    /// package, its files, its configuration and the user's enable choice are
    /// kept; its backend and contributions are not activated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incompatible: Option<String>,
    #[serde(default)]
    pub backend: Option<String>,
    #[serde(default)]
    pub contributions: PluginContributions,
    #[serde(default)]
    pub capabilities: Vec<PluginCapabilityRequest>,
}

pub const PLUGIN_MANIFEST_FILE: &str = "pig-plugin.json";
pub const PLUGIN_BACKEND_BUNDLE_FILE: &str = ".pig-backend.nshc";
pub const PLUGIN_BACKEND_VERSION_FILE: &str = ".pig-backend-version";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct PluginManifest {
    pub api_version: u32,
    pub id: String,
    pub name: PluginLocalizedText,
    pub version: String,
    pub build: u64,
    #[serde(default)]
    pub backend: Option<String>,
    pub min_host_version: String,
    pub protocols: PluginProtocolVersions,
    #[serde(default)]
    pub capabilities: Vec<PluginCapabilityRequest>,
    #[serde(default)]
    pub contributions: PluginContributions,
}

/// The host-version check of a manifest's `minHostVersion`.
///
/// `Err` means a version string is not valid SemVer (a manifest error);
/// `Ok(None)` means the host satisfies the requirement; `Ok(Some(message))`
/// carries the diagnostic for a package that needs a newer host.
pub fn host_version_check(host: &str, min: &str) -> Result<Option<String>, String> {
    let host_version = semver::Version::parse(host)
        .map_err(|error| format!("invalid host version `{host}`: {error}"))?;
    let required = semver::Version::parse(min)
        .map_err(|error| format!("minHostVersion `{min}` is not a SemVer version: {error}"))?;
    if host_version >= required {
        Ok(None)
    } else {
        Ok(Some(format!(
            "requires host {min} or newer; this host is {host}"
        )))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PluginLocalizedText {
    Text(String),
    Localized(BTreeMap<String, String>),
}

impl PluginLocalizedText {
    pub fn fallback(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Localized(values) => values
                .get("zh-CN")
                .or_else(|| values.get("en"))
                .or_else(|| values.values().next())
                .cloned()
                .unwrap_or_default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginProtocolVersions {
    pub ui: u32,
    pub services: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginCapabilityRequest {
    pub name: String,
    #[serde(default)]
    pub targets: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledPluginMetadata {
    pub digest: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginSource {
    BuiltIn,
    #[default]
    User,
    Project {
        workspace: PathBuf,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginPackageSource {
    pub root: PathBuf,
    pub source: PluginSource,
}

pub const PLUGIN_MARKETPLACE_API_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginMarketplaceCatalog {
    pub api_version: u32,
    #[serde(default)]
    pub packages: Vec<PluginMarketplacePackage>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginMarketplacePackage {
    pub id: String,
    pub name: String,
    pub version: String,
    pub directory: String,
    #[serde(default)]
    pub description: BTreeMap<String, String>,
    pub digest: String,
    pub files: Vec<PluginMarketplaceFile>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginMarketplaceFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginMarketplaceInstall {
    pub package: String,
    pub digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedPluginDownload {
    pub package: String,
    pub digest: String,
    pub package_path: PathBuf,
}

/// A deterministic same-name conflict between enabled packages. The winner is
/// the first package by id; the diagnostic names both sides so it never
/// depends on discovery or load order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginContributionConflict {
    /// `tool`, `agent`, `skill`, `mcp` or `theme`.
    pub kind: String,
    pub name: String,
    pub winner: String,
    pub losers: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PluginSnapshot {
    #[serde(default)]
    pub generation: u64,
    #[serde(default)]
    pub packages: Vec<PluginPackageInfo>,
    #[serde(default)]
    pub runtime_contributions: BTreeMap<String, PluginContributions>,
    #[serde(default)]
    pub credential_grants: Vec<PluginCredentialGrant>,
    #[serde(default)]
    pub conflicts: Vec<PluginContributionConflict>,
    #[serde(default)]
    pub errors: Vec<PluginIssue>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginIssue {
    pub root: PathBuf,
    pub error: crate::CoreError,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginScope {
    User,
    Project { workspace: PathBuf },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginWorkspace {
    pub path: PathBuf,
    pub name: String,
    pub current: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PluginSkillInfo {
    pub name: String,
    pub dir_name: String,
    pub path: PathBuf,
    pub description: String,
    pub when_to_use: Option<String>,
    pub scope: PluginScope,
    pub package: Option<String>,
    pub enabled: bool,
    pub overrides_user: bool,
    pub has_frontmatter: bool,
    pub markdown: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PluginSkillsSnapshot {
    pub user_path: PathBuf,
    pub project_path: Option<PathBuf>,
    pub skills: Vec<PluginSkillInfo>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PluginMcpServerInfo {
    pub name: String,
    pub scope: PluginScope,
    pub package: Option<String>,
    pub config: serde_json::Value,
    pub summary: String,
    pub disabled: bool,
    pub timeout_ms: u64,
    pub invalid_reason: Option<String>,
    pub overrides_user: bool,
    pub read_only: bool,
    pub status: Option<crate::McpServerStatus>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PluginMcpSnapshot {
    pub user_path: PathBuf,
    pub project_path: Option<PathBuf>,
    pub servers: Vec<PluginMcpServerInfo>,
    pub session_id: Option<String>,
    pub status_applicable: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PluginServiceSnapshot {
    pub generation: u64,
    pub locale: String,
    pub scope: PluginScope,
    pub workspaces: Vec<PluginWorkspace>,
    #[serde(default)]
    pub session: Option<crate::PluginSessionProjection>,
    pub skills: PluginSkillsSnapshot,
    pub mcp: PluginMcpSnapshot,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "data")]
pub enum PluginAction {
    OpenPage {
        page_id: String,
    },
    Refresh,
    CreateSkill {
        scope: PluginScope,
        dir_name: String,
        markdown: String,
    },
    UpdateSkill {
        scope: PluginScope,
        dir_name: String,
        markdown: String,
    },
    DeleteSkill {
        scope: PluginScope,
        dir_name: String,
    },
    SetSkillEnabled {
        path: PathBuf,
        enabled: bool,
    },
    UpsertMcp {
        scope: PluginScope,
        name: String,
        config: serde_json::Value,
    },
    DeleteMcp {
        scope: PluginScope,
        name: String,
    },
    SetMcpEnabled {
        scope: PluginScope,
        name: String,
        disabled: bool,
    },
    RefreshMcp,
    ReadConfig {
        path: String,
    },
    WriteConfig {
        path: String,
        value: serde_json::Value,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PluginActionResult {
    pub request_id: String,
    pub package: String,
    pub digest: String,
    pub result: Result<serde_json::Value, crate::CoreError>,
    #[serde(default)]
    pub refresh_views: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HookKind;

    #[test]
    fn contribution_defaults_keep_optional_sections_empty() {
        let contribution: PluginContributions = serde_json::from_str("{}").unwrap();
        assert_eq!(contribution.api_version, PLUGIN_API_VERSION);
        assert!(contribution.themes.is_empty());
        assert!(contribution.skills.is_empty());
        assert!(contribution.mcp.is_empty());
        assert!(contribution.actions.is_empty());
    }

    #[test]
    fn configuration_declaration_parses_localized_metadata_and_permissions() {
        let contribution: PluginContributions = serde_json::from_value(serde_json::json!({
            "description": {"en": "Web search", "zh-CN": "网络搜索"},
            "config": {
                "items": [{
                    "key": "backend",
                    "description": {"en": "Provider"},
                    "default": "auto"
                }],
                "read": ["app.theme", "plugin.example.resources.endpoint"],
                "write": ["app.default_delivery"]
            },
            "actions": ["read_config", "write_config"]
        }))
        .unwrap();
        assert_eq!(contribution.description["en"], "Web search");
        assert_eq!(contribution.config.items[0].key, "backend");
        assert_eq!(contribution.config.read.len(), 2);
        assert!(
            contribution
                .actions
                .contains(&PluginCapability::WriteConfig)
        );
    }

    #[test]
    fn tool_availability_defaults_to_static() {
        let tool: PluginTool = serde_json::from_value(serde_json::json!({
            "name": "DocSearch",
            "description": "Search documentation",
            "parameters": {"type": "object"}
        }))
        .unwrap();
        assert_eq!(tool.availability, PluginToolAvailability::Static);
    }

    #[test]
    fn runtime_contributions_replace_the_manifest_entry_of_the_same_identity() {
        let manifest: PluginContributions = serde_json::from_value(serde_json::json!({
            "tools": [{"name": "Echo", "description": "Manifest", "parameters": {"type": "object"}}],
            "commands": [{"id": "open", "title": {"en": "Open"}}]
        }))
        .unwrap();
        let runtime: PluginContributions = serde_json::from_value(serde_json::json!({
            "tools": [
                {"name": "Echo", "description": "Runtime", "parameters": {"type": "object"}},
                {"name": "Extra", "description": "Extra", "parameters": {"type": "object"}}
            ],
            "commands": [{"id": "open", "title": {"en": "Open"}, "order": 3}]
        }))
        .unwrap();
        let merged = manifest.merged(&runtime);
        assert_eq!(merged.tools.len(), 2);
        assert_eq!(
            merged
                .tools
                .iter()
                .find(|tool| tool.name == "Echo")
                .map(|tool| tool.description.as_str()),
            Some("Runtime")
        );
        assert_eq!(merged.commands.len(), 1);
        assert_eq!(merged.commands[0].order, 3);
    }

    #[test]
    fn host_version_check_compares_semver_and_reports_newer_requirements() {
        assert_eq!(
            host_version_check("1.4.0", "1.0.0").expect("valid versions"),
            None
        );
        assert_eq!(host_version_check("1.0.0", "1.0.0").unwrap(), None);
        let newer = host_version_check("1.0.0", "2.0.0")
            .unwrap()
            .expect("newer requirement");
        assert!(newer.contains("2.0.0") && newer.contains("1.0.0"));
        assert_eq!(
            host_version_check("1.0.0", "1.0.0-beta.1").unwrap(),
            None,
            "a prerelease is older than its release"
        );
        assert!(host_version_check("1.0.0", "1.0").is_err());
        assert!(host_version_check("not-a-version", "1.0.0").is_err());
    }

    #[test]
    fn tool_contract_defaults_and_clamps_are_explicit() {
        let tool: PluginTool = serde_json::from_value(serde_json::json!({
            "name": "DocSearch",
            "description": "Search documentation",
            "parameters": {"type": "object"}
        }))
        .unwrap();
        assert_eq!(tool.approval, PluginToolApproval::OnRequest);
        assert!(!tool.idempotent);
        assert_eq!(tool.timeout_ms, None);
        assert_eq!(tool.concurrency(), PLUGIN_TOOL_DEFAULT_CONCURRENCY);
        assert_eq!(tool.output_budget(), 32 * 1024);
        let clamped: PluginTool = serde_json::from_value(serde_json::json!({
            "name": "DocSearch",
            "description": "Search documentation",
            "parameters": {"type": "object"},
            "timeout_ms": 90_000_000,
            "max_concurrency": 900,
            "max_output_bytes": 1
        }))
        .unwrap();
        assert_eq!(
            clamped.timeout().as_millis() as u64,
            PLUGIN_TOOL_MAX_TIMEOUT_MS
        );
        assert_eq!(clamped.concurrency(), PLUGIN_TOOL_MAX_CONCURRENCY);
        assert_eq!(
            clamped.output_budget(),
            PLUGIN_TOOL_MIN_OUTPUT_BYTES as usize
        );
    }

    #[test]
    fn plugin_hooks_reuse_the_hook_entry_contract() {
        let contributions: PluginContributions = serde_json::from_value(serde_json::json!({
            "actions": ["provide_hooks"],
            "hooks": [{
                "event": "pre_tool_use",
                "type": "process",
                "command": "check",
                "args": ["--strict"],
                "matcher": "Write|Edit",
                "timeout_ms": 5000
            }]
        }))
        .unwrap();
        let hook = &contributions.hooks[0];
        assert_eq!(hook.event, PluginHookEvent::PreToolUse);
        assert_eq!(hook.entry.kind, HookKind::Process);
        assert_eq!(hook.entry.args, ["--strict"]);
        assert_eq!(hook.event.name(), "PreToolUse");
    }

    #[test]
    fn actions_are_typed_not_arbitrary_commands() {
        let action: PluginAction = serde_json::from_value(serde_json::json!({
            "kind": "upsert_mcp",
            "data": {
                "scope": "user",
                "name": "docs",
                "config": {"url": "https://example.test/mcp"}
            }
        }))
        .unwrap();
        assert!(matches!(
            action,
            PluginAction::UpsertMcp {
                scope: PluginScope::User,
                name,
                ..
            } if name == "docs"
        ));
    }
}
