use std::path::{Path, PathBuf};

use pig_protocol::{AppConfig, CoreError};

use super::project::ProjectSandboxLayer;
use super::trust::{WorkspaceTrust, canonical_workspace};

/// A workspace's own layer, next to the other project files
/// (`.pigcode/hooks.toml`, `.pigcode/mcp.json`, `.pigcode/permissions.toml`).
pub const PROJECT_CONFIG: &str = ".pigcode/config.toml";

/// The machine-wide layer, applied under the user's own file. Same role as
/// Codex's system config (`/etc/codex/config.toml`, `%ProgramData%\OpenAI\Codex\config.toml`).
///
/// `None` on a host without a conventional location.
pub fn system_path() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        let program_data = std::env::var_os("ProgramData")?;
        Some(
            PathBuf::from(program_data)
                .join("PigCode")
                .join("config.toml"),
        )
    }
    #[cfg(not(target_os = "windows"))]
    {
        Some(PathBuf::from("/etc/pigcode/config.toml"))
    }
}

/// Merge `overlay` into `base`, giving the overlay precedence: tables merge key
/// by key, everything else is replaced.
///
/// This is Codex's `merge_toml_values`, and the array rule is the one worth
/// remembering: a list in the higher layer **replaces** the lower layer's list
/// instead of appending to it.
fn merge_toml(base: &mut toml::Value, overlay: &toml::Value) {
    match (base, overlay) {
        (toml::Value::Table(base), toml::Value::Table(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(key) {
                    Some(existing) => merge_toml(existing, value),
                    None => {
                        base.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (base, overlay) => *base = overlay.clone(),
    }
}

/// Restrict a trusted project layer to **selecting** an existing provider and
/// model from the user-level configuration.
///
/// A provider entry holds the endpoint and the API key, so a repository must
/// never be able to inject or replace one (that would silently route requests,
/// and their credentials, to an author-controlled host). The project may still
/// set `default_provider`/`default_model`, but only to ids the lower layers
/// already define; anything else is dropped with a warning so the workspace
/// keeps running on the user's own selection.
fn restrict_project_provider_selection(
    project: &mut toml::map::Map<String, toml::Value>,
    base: &toml::Value,
) {
    if project.remove("providers").is_some() {
        tracing::warn!(
            "ignoring `[[providers]]` from a project config: a project may select an existing \
             provider/model, never inject or replace provider endpoints or credentials"
        );
    }

    let providers = base
        .get("providers")
        .and_then(toml::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let provider_named = |id: &str| {
        providers
            .iter()
            .find(|provider| provider.get("id").and_then(toml::Value::as_str) == Some(id))
    };

    if let Some(selected) = project
        .get("default_provider")
        .and_then(toml::Value::as_str)
        .map(str::to_string)
        && provider_named(&selected).is_none()
    {
        tracing::warn!(
            "ignoring the project `default_provider` value `{selected}`: it does not name a \
             provider configured by the user"
        );
        project.remove("default_provider");
    }

    let Some(model) = project
        .get("default_model")
        .and_then(toml::Value::as_str)
        .map(str::to_string)
    else {
        return;
    };
    let provider_id = project
        .get("default_provider")
        .and_then(toml::Value::as_str)
        .or_else(|| base.get("default_provider").and_then(toml::Value::as_str));
    let model_exists = provider_id
        .and_then(provider_named)
        .and_then(|provider| provider.get("models"))
        .and_then(toml::Value::as_array)
        .is_some_and(|models| {
            models
                .iter()
                .any(|entry| entry.get("id").and_then(toml::Value::as_str) == Some(model.as_str()))
        });
    if !model_exists {
        tracing::warn!(
            "ignoring the project `default_model` value `{model}`: it does not name a model of \
             the selected user-level provider"
        );
        project.remove("default_model");
    }
}

/// Duplicate provider ids make every id-based lookup hit the first one: model resolution lands on
/// the wrong provider's fallback model, and session meta/labels get mismatched. Warn loudly at load
/// time so nobody has to hunt for this.
fn warn_duplicate_provider_ids(config: &AppConfig) {
    let mut seen = std::collections::HashSet::new();
    for provider in &config.providers {
        if !seen.insert(&provider.id) {
            tracing::warn!(
                "warning: duplicate provider id \"{}\" ({} shares it with another provider; delete and recreate the affected providers in settings)",
                provider.id,
                provider.name
            );
        }
    }
}

pub fn load(path: &std::path::Path) -> Result<AppConfig, CoreError> {
    // Missing file = empty config (not an error); only parse failures are errors
    if !path.exists() {
        return Ok(AppConfig::default());
    }
    let raw = std::fs::read_to_string(path).map_err(|e| CoreError::ConfigRead {
        path: path.display().to_string(),
        detail: e.to_string(),
    })?;
    let config = toml::from_str::<AppConfig>(&raw).map_err(|e| CoreError::ConfigParseFile {
        path: path.display().to_string(),
        detail: e.to_string(),
    })?;
    warn_duplicate_provider_ids(&config);
    Ok(config)
}

/// The effective config for a workspace: the machine-wide layer, then the
/// user's own file, then the workspace's `.pigcode/config.toml` **when the
/// workspace is trusted**.
///
/// A project layer is arbitrary TOML written by whoever authored the
/// repository, so it is admitted by the same trust decision that admits project
/// hooks (`[hooks] trusted_projects`), and until that decision is made the
/// workspace runs on the user's own configuration. A layer that cannot be read
/// or parsed is skipped with a warning rather than failing the session: the
/// user-level rules are the more restrictive side, so continuing without the
/// project's additions is the safe direction (the same call
/// `.pigcode/permissions.toml` already makes).
///
/// The project `[sandbox]`, `[windows]` and `[hooks]` tables are **not** merged
/// into the returned config. Their workspace-scoped security forms are exposed
/// separately by [`ProjectSandboxLayer`], where they can only add restrictions;
/// letting the ordinary layer merge replace `sandbox.deny_read`, a user's
/// network block or the hooks master switch would let a repository weaken the
/// boundary. Project hook entries live in `.pigcode/hooks.toml` and are gated
/// separately.
///
/// The project `providers` array is stripped as well: endpoint and credential
/// injection is a repository-controlled redirect. Project
/// `default_provider`/`default_model` values are kept only when they select a
/// provider/model the user-level layers already define.
///
/// Stored values are returned exactly as written: `${ENV_VAR}` references are
/// not expanded and URLs are not normalized. Use [`RuntimeConfig`] for the
/// runtime view.
///
/// `workspace` is the session's cwd; an untrusted or missing project file is
/// simply not a layer.
pub fn load_layered(path: &Path, workspace: &Path) -> Result<AppConfig, CoreError> {
    load_layered_parts(path, workspace).map(|parts| parts.config)
}

pub(super) struct LayeredParts {
    pub(super) config: AppConfig,
    pub(super) trust: WorkspaceTrust,
    pub(super) project_sandbox: ProjectSandboxLayer,
}

pub(super) fn load_layered_parts(path: &Path, workspace: &Path) -> Result<LayeredParts, CoreError> {
    // Every layer is merged over the built-in defaults, so a layer only has to
    // name the keys it changes (Codex's `ConfigToml` behaves the same way).
    let mut merged = toml::Value::try_from(AppConfig::default())
        .unwrap_or_else(|_| toml::Value::Table(toml::map::Map::new()));
    if let Some(system) = read_layer(system_path().as_deref()) {
        merge_toml(&mut merged, &system);
    }
    if let Some(user) = read_layer(Some(path)) {
        merge_toml(&mut merged, &user);
    }
    // The trust decision comes from the layers below the project layer: a
    // repository cannot admit itself.
    let trust = WorkspaceTrust::new(workspace, is_trusted_workspace(&merged, workspace));
    let project_path = trust.root().join(PROJECT_CONFIG);
    let mut project_sandbox = ProjectSandboxLayer::default();
    if trust.is_trusted() {
        if let Some(mut project) = read_layer(Some(&project_path)) {
            project_sandbox = ProjectSandboxLayer::parse(&project, workspace);
            if let Some(table) = project.as_table_mut() {
                // Security overlays are applied add-only through the layer;
                // ordinary merging would replace the user's values. The hooks
                // table is stripped too: its master switch and trust list are
                // user-level decisions, and project hook entries live in
                // `.pigcode/hooks.toml`, gated separately.
                table.remove("sandbox");
                table.remove("windows");
                table.remove("hooks");
                // Providers carry the endpoint and the credential: a project
                // may select an existing user provider/model, never inject or
                // replace a provider array.
                restrict_project_provider_selection(table, &merged);
            }
            merge_toml(&mut merged, &project);
        }
    } else if project_path.is_file() {
        tracing::debug!(
            "the project config {} is not trusted yet; the workspace runs on the user's configuration",
            project_path.display()
        );
    }
    let config = merged
        .try_into::<AppConfig>()
        .map_err(|error| CoreError::ConfigParseFile {
            path: path.display().to_string(),
            detail: error.to_string(),
        })?;
    warn_duplicate_provider_ids(&config);
    Ok(LayeredParts {
        config,
        trust,
        project_sandbox,
    })
}

/// Read one layer. A missing file is an empty table; an unreadable or
/// unparsable one is reported and treated as empty.
pub(super) fn read_layer(path: Option<&Path>) -> Option<toml::Value> {
    let path = path?;
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            tracing::warn!(
                "ignoring unreadable config layer {}: {error}",
                path.display()
            );
            return None;
        }
    };
    match raw.parse::<toml::Value>() {
        Ok(value) => Some(value),
        Err(error) => {
            tracing::warn!(
                "ignoring unparsable config layer {}: {error}",
                path.display()
            );
            None
        }
    }
}

/// Whether `workspace` is in the `[hooks] trusted_projects` list of the merged
/// layers below the project layer. Paths are compared canonically, exactly as
/// the hook runner admits a project's hooks.toml.
pub fn is_trusted_workspace(base: &toml::Value, workspace: &Path) -> bool {
    let Some(trusted) = base
        .get("hooks")
        .and_then(|hooks| hooks.get("trusted_projects"))
        .and_then(toml::Value::as_array)
    else {
        return false;
    };
    let workspace = canonical_workspace(workspace);
    trusted
        .iter()
        .filter_map(toml::Value::as_str)
        .any(|entry| Path::new(entry) == workspace)
}
