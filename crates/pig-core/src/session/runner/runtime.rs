use super::*;

/// The runtime config a session runs with: the system + user layers plus the
/// workspace's own trusted `.pigcode/config.toml` layer, with the trust token
/// and additive project sandbox layer attached (see [`crate::config::RuntimeConfig`]).
///
/// Stored behind an `Arc` so the runner entry and the Session share one
/// resolved view instead of synchronizing copies.
///
/// Process-wide surfaces (the settings page, the provider list, `GetConfig`)
/// keep reading the user-level config: the project layer is about the workspace
/// the session runs in, not about the application's own settings.
pub(super) fn layered_runtime(
    config: Option<&AppConfig>,
    config_path: &std::path::Path,
    cwd: &std::path::Path,
    credentials: Arc<crate::credentials::CredentialManager>,
) -> Option<Arc<crate::config::RuntimeConfig>> {
    match crate::config::RuntimeConfig::load(config_path, cwd, credentials.clone()) {
        Ok(runtime) => Some(Arc::new(runtime)),
        Err(error) => {
            tracing::warn!(
                "failed to load the layered config for {}: {error:?}",
                cwd.display()
            );
            config.cloned().map(|config| {
                Arc::new(crate::config::RuntimeConfig::from_snapshot(
                    config,
                    cwd,
                    credentials,
                ))
            })
        }
    }
}

/// Install the user-level sandbox settings (startup and after a config save).
///
/// Platform-neutral: the read-deny table is enforced on every backend (Seatbelt
/// on macOS, the MXC walker on Windows), so the seed rules reach macOS too. The
/// network choice stays Windows-only.
///
/// The workspace's own layer is not part of this: it is per workspace and lives
/// on the session state, applied on top at launch time.
pub(super) fn install_sandbox_settings(config: &AppConfig) {
    #[cfg(target_os = "windows")]
    crate::task::init_windows_sandbox_network(matches!(
        config.windows.sandbox_network,
        pig_protocol::WindowsSandboxNetworkChoice::Block
    ));
    crate::task::init_sandbox_settings(crate::task::SandboxSettings::from_config(config));
}
