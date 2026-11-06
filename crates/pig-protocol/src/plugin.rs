use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const PLUGIN_API_VERSION: u32 = 1;

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
    pub credentials: Vec<PluginCredentialDeclaration>,
    #[serde(default)]
    pub http: Vec<PluginHttpOrigin>,
    #[serde(default)]
    pub config: PluginConfigDeclaration,
    #[serde(default)]
    pub actions: Vec<PluginCapability>,
}

impl PluginContributions {
    pub fn merged(&self, runtime: &Self) -> Self {
        let mut merged = self.clone();
        merged.themes.extend(runtime.themes.clone());
        merged.skills.extend(runtime.skills.clone());
        merged.mcp.extend(runtime.mcp.clone());
        merged.tools.extend(runtime.tools.clone());
        merged.commands.extend(runtime.commands.clone());
        merged
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginTool {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub availability: PluginToolAvailability,
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
    pub result: Result<Vec<String>, crate::CoreError>,
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
    pub result: Result<serde_json::Value, crate::CoreError>,
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
