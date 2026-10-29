use std::path::Path;
use std::sync::Arc;

use pig_protocol::{AppConfig, CoreError, ModelConfig, ProviderConfig};
use pig_provider::ResolvedModel;

use super::layering::load_layered_parts;
use super::project::{ProjectSandboxLayer, project_sandbox_layer};
use super::trust::WorkspaceTrust;
use crate::credentials::CredentialManager;

/// The single resolved runtime view of one workspace's configuration.
///
/// [`Self::load`] performs the same system → user → trusted-project layering as
/// [`load_layered`] and keeps the trust decision next to the config, so root
/// sessions, resumed sessions (`Session::load`) and subagents can all resolve
/// provider/model/capabilities through this one object instead of re-deriving
/// trust or re-reading project files. The runner stores it behind an `Arc`, so
/// the session and its entry share the same resolved view rather than syncing
/// copies.
///
/// API keys are read from the shared credential cache only at resolution time;
/// they never enter the persisted or transmitted [`AppConfig`]. Base URLs are
/// normalized before endpoint-capability checks.
///
/// The contained [`AppConfig`] is the effective (possibly project-merged)
/// snapshot. It is **not** what `SaveConfig` should persist: settings run on the
/// user-level config.
#[derive(Clone)]
pub struct RuntimeConfig {
    config: AppConfig,
    trust: WorkspaceTrust,
    project_sandbox: ProjectSandboxLayer,
    credentials: Arc<CredentialManager>,
}

impl RuntimeConfig {
    /// Load the effective configuration for `workspace`.
    pub fn load(
        config_path: &Path,
        workspace: &Path,
        credentials: Arc<CredentialManager>,
    ) -> Result<Self, CoreError> {
        let parts = load_layered_parts(config_path, workspace)?;
        Ok(Self {
            config: parts.config,
            trust: parts.trust,
            project_sandbox: parts.project_sandbox,
            credentials,
        })
    }

    /// Build from an already loaded config snapshot, deciding trust from its
    /// `[hooks] trusted_projects` list and reading the project sandbox layer.
    /// The snapshot is used as the effective provider/model source as passed:
    /// for trusted project overrides, pass the layered snapshot from
    /// [`Self::load`] (this constructor does not re-read `config.toml`).
    pub fn from_snapshot(
        config: AppConfig,
        workspace: &Path,
        credentials: Arc<CredentialManager>,
    ) -> Self {
        let trust = WorkspaceTrust::for_config(Some(&config), workspace);
        let project_sandbox = project_sandbox_layer(trust.root(), trust.is_trusted());
        Self {
            config,
            trust,
            project_sandbox,
            credentials,
        }
    }

    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    pub fn trust(&self) -> &WorkspaceTrust {
        &self.trust
    }

    pub fn project_sandbox(&self) -> &ProjectSandboxLayer {
        &self.project_sandbox
    }

    pub fn workspace(&self) -> &Path {
        self.trust.root()
    }

    /// Resolve the session model: an explicit selection when given, otherwise
    /// `default_provider`/`default_model`. Keeps the lenient fallback (first
    /// enabled provider/model) so existing sessions resume on the same
    /// endpoint; an explicit provider that does not resolve falls back with a
    /// warning instead of silently.
    ///
    /// `default_level` is the thinking level used when there is no explicit
    /// request (a stored selection without a level stays without one).
    pub fn resolve_model(
        &self,
        request: Option<&ModelRequest>,
        default_level: Option<&str>,
    ) -> Result<ResolvedModel, RuntimeConfigError> {
        let requested_provider = request.and_then(|request| request.provider_id.as_deref());
        let model_id = request
            .and_then(|request| request.model_id.as_deref())
            .unwrap_or(&self.config.default_model);
        let level = match request {
            Some(request) => request.reasoning_level.clone(),
            None => default_level.map(str::to_string),
        };
        let provider = match requested_provider {
            Some(provider_id) => {
                match self
                    .config
                    .providers
                    .iter()
                    .find(|provider| provider.enabled && provider.id == provider_id)
                {
                    Some(provider) => provider,
                    None => {
                        let fallback = self
                            .config
                            .providers
                            .iter()
                            .find(|provider| provider.enabled)
                            .ok_or_else(|| {
                                RuntimeConfigError::no_model("No enabled provider is configured")
                            })?;
                        tracing::warn!(
                            requested = provider_id,
                            fallback = %fallback.id,
                            "the selected provider does not exist or is not enabled; falling back"
                        );
                        fallback
                    }
                }
            }
            None => self
                .config
                .providers
                .iter()
                .find(|provider| provider.enabled && provider.id == self.config.default_provider)
                .or_else(|| {
                    self.config
                        .providers
                        .iter()
                        .find(|provider| provider.enabled)
                })
                .ok_or_else(|| RuntimeConfigError::no_model("No enabled provider is configured"))?,
        };
        let model = provider
            .models
            .iter()
            .find(|model| model.id == model_id)
            .or_else(|| provider.models.first())
            .ok_or_else(|| {
                RuntimeConfigError::no_model(format!(
                    "Provider \"{}\" has no models configured",
                    provider.id
                ))
            })?;
        build_resolved_model(provider, model, level, false, "", &self.credentials)
    }

    /// Strictly resolve a subagent profile's model (`providerId/modelId` or a
    /// bare model id in the default provider). `None` inherits the parent
    /// endpoint wholesale.
    ///
    /// A bare id containing `/` is checked against the default provider's exact
    /// model ids first: model ids may themselves contain a slash, so
    /// `org/model` must not be split into a provider named `org` when the
    /// default provider defines that exact model. Resolution failures are
    /// errors — subagent configuration must surface.
    pub fn resolve_subagent_model(
        &self,
        parent: &ResolvedModel,
        spec: Option<&str>,
        thought_level: Option<&str>,
        profile_name: &str,
    ) -> Result<ResolvedModel, RuntimeConfigError> {
        let Some(spec) = spec else {
            return Ok(parent.clone());
        };
        let default_provider = self
            .config
            .providers
            .iter()
            .find(|provider| provider.enabled && provider.id == self.config.default_provider);
        let (provider, model_id) = match spec.split_once('/') {
            Some((provider_id, model_id)) => {
                if let Some(provider) = default_provider
                    && provider.models.iter().any(|model| model.id == spec)
                {
                    (provider, spec)
                } else {
                    let provider = self
                        .config
                        .providers
                        .iter()
                        .find(|provider| provider.enabled && provider.id == provider_id)
                        .ok_or_else(|| {
                            RuntimeConfigError::new(format!(
                                "Subagent {profile_name} specified provider \"{provider_id}\", \
                                 which does not exist or is not enabled. Available models: {}",
                                available_model_list(&self.config)
                            ))
                        })?;
                    (provider, model_id)
                }
            }
            None => {
                let provider = default_provider.ok_or_else(|| {
                    RuntimeConfigError::new(format!(
                        "Default provider \"{}\" does not exist or is not enabled. Available \
                         models: {}",
                        self.config.default_provider,
                        available_model_list(&self.config)
                    ))
                })?;
                (provider, spec)
            }
        };
        let model = provider
            .models
            .iter()
            .find(|model| model.id == model_id)
            .ok_or_else(|| {
                RuntimeConfigError::new(format!(
                    "Provider {} has no model \"{model_id}\" (subagent {profile_name}). Available \
                     models: {}",
                    provider.id,
                    available_model_list(&self.config)
                ))
            })?;
        build_resolved_model(
            provider,
            model,
            thought_level.map(str::to_string),
            true,
            &format!(" (subagent {profile_name})"),
            &self.credentials,
        )
    }
}

impl std::fmt::Debug for RuntimeConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RuntimeConfig")
            .field("workspace", &self.trust.root())
            .field("trusted", &self.trust.is_trusted())
            .field("provider_count", &self.config.providers.len())
            .finish_non_exhaustive()
    }
}

/// An explicit model selection, mirroring the session's stored selection
/// without depending on session internals.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelRequest {
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub reasoning_level: Option<String>,
}

/// A provider resolved for the wire: expanded secret, normalized URL.
#[derive(Clone, PartialEq, Eq)]
pub struct ResolvedProvider {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub api_key: String,
    pub api_format: pig_protocol::ApiFormat,
    pub enabled: bool,
}

impl std::fmt::Debug for ResolvedProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResolvedProvider")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .field("api_format", &self.api_format)
            .field("enabled", &self.enabled)
            .finish()
    }
}

/// What kind of runtime configuration failure occurred. Callers map this to a
/// protocol error instead of inspecting the display string.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeConfigErrorKind {
    /// No enabled provider, or the selected provider has no model to run.
    NoModelConfigured,
    /// A configured provider/model/level or credential is unusable.
    Configuration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeConfigError {
    detail: String,
    kind: RuntimeConfigErrorKind,
}

impl RuntimeConfigError {
    pub fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
            kind: RuntimeConfigErrorKind::Configuration,
        }
    }

    fn no_model(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
            kind: RuntimeConfigErrorKind::NoModelConfigured,
        }
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }

    pub fn kind(&self) -> RuntimeConfigErrorKind {
        self.kind
    }
}

impl std::fmt::Display for RuntimeConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for RuntimeConfigError {}

impl From<RuntimeConfigError> for String {
    fn from(error: RuntimeConfigError) -> Self {
        error.detail
    }
}

/// The runtime form of a configured base URL: surrounding whitespace and
/// trailing slashes removed. The path (including `/v1`) is preserved; request
/// joining is the provider crate's job.
pub fn normalize_base_url(value: &str) -> String {
    value.trim().trim_end_matches('/').to_string()
}

/// Resolve one configured provider for the wire without needing a workspace.
/// The API key comes from the shared credential cache, never from the stored
/// configuration. Disabled providers are accepted so the settings page can
/// test an entry before enabling it.
pub fn resolve_provider_config(
    provider: &ProviderConfig,
    credentials: &CredentialManager,
) -> Result<ResolvedProvider, RuntimeConfigError> {
    let api_key = credentials.cached_provider(provider).map_err(|error| {
        RuntimeConfigError::new(format!(
            "Provider \"{}\" credential error: {}",
            provider.id,
            error.detail()
        ))
    })?;
    Ok(ResolvedProvider {
        id: provider.id.clone(),
        name: provider.name.clone(),
        base_url: normalize_base_url(&provider.base_url),
        api_key,
        api_format: provider.api_format,
        enabled: provider.enabled,
    })
}

fn build_resolved_model(
    provider: &ProviderConfig,
    model: &ModelConfig,
    level: Option<String>,
    strict_level: bool,
    context: &str,
    credentials: &CredentialManager,
) -> Result<ResolvedModel, RuntimeConfigError> {
    let provider = resolve_provider_config(provider, credentials)?;
    let reasoning_params = match level.as_deref() {
        Some(level) => match model.reasoning_params.get(level) {
            Some(params) => Some(params.clone()),
            None if strict_level => {
                let mut levels: Vec<&str> =
                    model.reasoning_params.keys().map(String::as_str).collect();
                levels.sort_unstable();
                let available = if levels.is_empty() {
                    "(none)".to_string()
                } else {
                    levels.join(", ")
                };
                return Err(RuntimeConfigError::new(format!(
                    "Model \"{}\" has no reasoning level \"{level}\"; available levels: \
                     {available}{context}",
                    model.id
                )));
            }
            None => None,
        },
        None => None,
    };
    Ok(ResolvedModel {
        base_url: provider.base_url.clone(),
        api_key: provider.api_key,
        model: model.id.clone(),
        context_window: model.context_window,
        max_output_tokens: model.max_output_tokens,
        api_format: provider.api_format,
        reasoning_params,
        cap_structured: model.cap_structured,
        cap_strict_tools: model.cap_strict_tools,
        cap_web_search: model.cap_web_search,
        web_search_tool: model.web_search_tool.clone(),
        input_image: model.input_image,
        input_pdf: model.input_pdf,
        anthropic_cache_control: pig_provider::anthropic_cache_supported(&provider.base_url),
        replay_native_search: pig_provider::native_search_replay_supported(
            provider.api_format,
            &provider.base_url,
        ),
        prompt_cache_key: None,
        provider_name: provider.name,
        tool_settings: pig_provider::ToolCallSettings::new(model.parallel_tool_calls),
    })
}

fn available_model_list(config: &AppConfig) -> String {
    let ids: Vec<String> = config
        .providers
        .iter()
        .filter(|provider| provider.enabled)
        .flat_map(|provider| {
            provider
                .models
                .iter()
                .map(|model| format!("{}/{}", provider.id, model.id))
        })
        .collect();
    if ids.is_empty() {
        "(no enabled providers)".to_string()
    } else {
        ids.join(", ")
    }
}
