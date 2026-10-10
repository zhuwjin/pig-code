use std::path::PathBuf;

use pig_protocol::{AppConfig, CoreError};

pub fn default_path() -> PathBuf {
    pig_utils::data_dir().join("config.toml")
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
    let mut config = toml::from_str::<AppConfig>(&raw).map_err(|e| CoreError::ConfigParseFile {
        path: path.display().to_string(),
        detail: e.to_string(),
    })?;
    warn_duplicate_provider_ids(&config);
    expand_env_keys(&mut config);
    Ok(config)
}

fn expand_env_keys(config: &mut AppConfig) {
    for provider in &mut config.providers {
        provider.api_key = expand_env(&provider.api_key);
        provider.base_url = provider.base_url.trim_end_matches('/').to_string();
    }
}

pub fn expand_env(value: &str) -> String {
    let mut out = value.to_string();
    while let Some(start) = out.find("${") {
        let Some(end) = out[start..].find('}') else {
            break;
        };
        let var = out[start + 2..start + end].to_string();
        let replacement = std::env::var(&var).unwrap_or_default();
        out.replace_range(start..start + end + 1, &replacement);
    }
    out
}

pub fn save(path: &std::path::Path, config: &AppConfig) -> Result<(), CoreError> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let raw = toml::to_string_pretty(config).map_err(|e| CoreError::ConfigSerialize {
        detail: e.to_string(),
    })?;
    std::fs::write(path, raw).map_err(|e| CoreError::ConfigWrite {
        path: path.display().to_string(),
        detail: e.to_string(),
    })
}
