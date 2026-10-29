//! Configuration loading, layering, workspace trust and runtime resolution.
//!
//! The module is split by responsibility: defaults/seeding, file layering,
//! trust, the project-only sandbox overlay, runtime provider/model resolution,
//! and atomic persistence. Public paths are re-exported here so callers keep
//! using `crate::config::…` regardless of the internal file split.

mod defaults;
mod layering;
mod persistence;
mod project;
mod runtime;
#[cfg(test)]
mod tests;
mod trust;

pub use defaults::{default_deny_groups, default_path, ensure_seeded_deny_rules};
pub use layering::{PROJECT_CONFIG, is_trusted_workspace, load, load_layered, system_path};
pub use persistence::{
    ensure_private_data_dir, ensure_real_directory, require_real_directory, save,
    write_private_file, write_settings_file,
};
pub use project::{ProjectSandboxLayer, project_sandbox_layer, trusted_sandbox_layer};
pub use runtime::{
    ModelRequest, ResolvedProvider, RuntimeConfig, RuntimeConfigError, RuntimeConfigErrorKind,
    normalize_base_url, resolve_provider_config,
};
pub use trust::{WorkspaceTrust, config_trusts_workspace};
