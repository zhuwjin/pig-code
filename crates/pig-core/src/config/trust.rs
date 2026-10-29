use std::path::{Path, PathBuf};

use pig_protocol::AppConfig;

/// One workspace's trust decision, decided once from the merged system + user
/// configuration and then carried by every project-file loader.
///
/// Trust is deliberately a value, not a bool passed around: it binds the
/// canonical workspace root to the decision, so a trust token minted for one
/// workspace cannot be reused to admit another workspace's `.pigcode` files.
/// The decision source is `[hooks] trusted_projects` — the same list that
/// admits project hooks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceTrust {
    root: PathBuf,
    trusted: bool,
}

impl WorkspaceTrust {
    /// Mint a decision for `workspace`; the root is canonicalized once.
    pub fn new(workspace: &Path, trusted: bool) -> Self {
        Self {
            root: canonical_workspace(workspace),
            trusted,
        }
    }

    /// Decide from a typed config snapshot (`[hooks] trusted_projects`).
    /// A missing snapshot is untrusted.
    pub fn for_config(config: Option<&AppConfig>, workspace: &Path) -> Self {
        let trusted = config.is_some_and(|config| config_trusts_workspace(config, workspace));
        Self::new(workspace, trusted)
    }

    pub fn untrusted(workspace: &Path) -> Self {
        Self::new(workspace, false)
    }

    /// Canonical workspace root this decision belongs to.
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn is_trusted(&self) -> bool {
        self.trusted
    }

    /// Whether this decision admits project files of `workspace`: it must be
    /// trusted **and** name the same canonical root.
    pub fn admits(&self, workspace: &Path) -> bool {
        self.trusted && canonical_workspace(workspace) == self.root
    }
}

pub(super) fn canonical_workspace(workspace: &Path) -> PathBuf {
    workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf())
}

/// Whether a typed config trusts `workspace`: the `[hooks] trusted_projects`
/// list, compared canonically. One trust per workspace admits the project's
/// hooks, MCP servers, agent profiles, skills, permission rules and
/// `.pigcode/config.toml` layer, as in Codex.
pub fn config_trusts_workspace(config: &AppConfig, workspace: &Path) -> bool {
    let workspace = canonical_workspace(workspace);
    config
        .hooks
        .trusted_projects
        .iter()
        .any(|entry| Path::new(entry) == workspace)
}
