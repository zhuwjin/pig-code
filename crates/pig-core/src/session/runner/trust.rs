use super::*;

/// Persist one workspace's trust decision and load the runtime it authorizes.
///
/// Trust is committed to disk **before** anything is activated. A failed save
/// leaves the session untouched; a failed post-save runtime load leaves the
/// decision durable for the next session but this one stays untrusted — never
/// hooks/allow/skills without the matching sandbox/trust state. The returned
/// runtime is the exact layered view built from the successfully saved file.
pub(super) fn persist_workspace_trust(
    config: &mut Option<AppConfig>,
    config_path: &std::path::Path,
    cwd: &std::path::Path,
    credentials: Arc<crate::credentials::CredentialManager>,
) -> Result<Arc<crate::config::RuntimeConfig>, CoreError> {
    let Some(live_config) = config.as_mut() else {
        return Err(CoreError::ConfigWrite {
            path: config_path.display().to_string(),
            detail: "the configuration could not be loaded; fix config.toml before trusting a \
                     workspace"
                .to_string(),
        });
    };
    let canonical = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let key = canonical.display().to_string();
    let mut candidate = live_config.clone();
    if !candidate.hooks.trusted_projects.contains(&key) {
        candidate.hooks.trusted_projects.push(key);
    }
    crate::config::save(config_path, &candidate)?;
    // Load the exact saved file before adopting it in memory: a reload failure
    // leaves the session and its in-memory user layer untouched.
    let runtime = crate::config::RuntimeConfig::load(config_path, cwd, credentials)?;
    // From here the durable decision and the in-memory user layer agree.
    *live_config = candidate;
    Ok(Arc::new(runtime))
}
