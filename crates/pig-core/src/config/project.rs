use std::collections::BTreeMap;
use std::path::Path;

use pig_protocol::{AppConfig, SandboxFsAccess, SandboxFsPermission};

use super::layering::{PROJECT_CONFIG, read_layer};
use super::trust::{WorkspaceTrust, canonical_workspace};

/// The `[sandbox]` table a workspace's own config contributes, in a form that
/// can only **add** restrictions to the user-level settings.
///
/// A project layer is repository-authored data, so every field is sanitized
/// before it reaches the policy:
///
/// * `deny_read` and normal-path `Scoped` tables are folded into
///   [`Self::filesystem`] as `deny` entries. Flattening a scope to one child
///   key keeps it additive even when the user already has a whole-root denial
///   or a scope under the same path: a user `deny` is never replaced by a
///   narrower project shape, and child denials from both levels are unioned by
///   the shared filesystem compiler.
/// * `[sandbox.filesystem]` keeps only denials: `read`/`write` grants are
///   dropped with a warning. A project grant could otherwise widen writes or
///   lift a user denial with a deeper read entry.
/// * A scoped denial under a special path (`:root`, `:workspace_roots`, …)
///   keeps its nested shape because it cannot be flattened to one path; the
///   merge preserves a user whole-root denial there too.
/// * `[windows] sandbox_network` can turn the block **on**, never off.
///
/// The structured `[[sandbox.deny]]` groups and `credential_dirs` are not part
/// of the layer at all: both are machine-level decisions.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectSandboxLayer {
    pub filesystem: Option<BTreeMap<String, SandboxFsPermission>>,
    /// `[windows] sandbox_network = "block"` from the project layer. A project
    /// `allow` is ignored (it cannot undo the user's block).
    pub network_block: Option<bool>,
}

impl ProjectSandboxLayer {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    /// Parse a project config file's `[sandbox]` and `[windows]` tables.
    /// Malformed or granting values are dropped with a warning, and the layer
    /// keeps only additive restrictions (the user-level value stays in force).
    pub fn parse(root: &toml::Value, workspace: &Path) -> Self {
        let mut entries: BTreeMap<String, SandboxFsPermission> = BTreeMap::new();
        if let Some(table) = root.get("sandbox").and_then(toml::Value::as_table) {
            if table.contains_key("deny") {
                tracing::warn!(
                    "ignoring `[[sandbox.deny]]` from a project config: the sensitive-file \
                     groups are edited in the user's own configuration"
                );
            }
            if table.contains_key("credential_dirs") {
                tracing::warn!(
                    "ignoring `[sandbox] credential_dirs` from a project config: which credential \
                     stores stay protected is a machine-level decision"
                );
            }
            if let Some(filesystem) = table.get("filesystem") {
                match filesystem
                    .clone()
                    .try_into::<BTreeMap<String, SandboxFsPermission>>()
                {
                    Ok(configured) => add_project_filesystem(&mut entries, configured, workspace),
                    Err(error) => {
                        tracing::warn!(
                            "ignoring the project `[sandbox.filesystem]` table: {error}"
                        );
                    }
                }
            }
            if let Some(rules) = table.get("deny_read") {
                match rules.clone().try_into::<Vec<String>>() {
                    Ok(rules) => {
                        for rule in rules {
                            keep_project_deny(&mut entries, &rule, workspace);
                        }
                    }
                    Err(error) => {
                        tracing::warn!("ignoring the project `[sandbox] deny_read` list: {error}");
                    }
                }
            }
        }
        let network_block = match root
            .get("windows")
            .and_then(toml::Value::as_table)
            .and_then(|windows| windows.get("sandbox_network"))
            .and_then(toml::Value::as_str)
        {
            Some(choice) if choice.eq_ignore_ascii_case("block") => Some(true),
            Some(choice) if choice.eq_ignore_ascii_case("allow") => {
                tracing::warn!(
                    "ignoring `[windows] sandbox_network = \"allow\"` from a project config: a \
                     project can block network access, never unblock it"
                );
                None
            }
            Some(choice) => {
                tracing::warn!(
                    "ignoring the project `[windows] sandbox_network` value `{choice}`: expected \
                     \"block\" or \"allow\""
                );
                None
            }
            None => None,
        };
        let filesystem = (!entries.is_empty()).then_some(entries);
        Self {
            filesystem,
            network_block,
        }
    }
}

/// Fold a project filesystem table into additive deny entries.
///
/// A normal-path scope is flattened to one child key per denial so it can
/// never collide with (and replace) the user's entry at the scope root. A
/// special-path scope keeps its nested shape; `SandboxSettings::with_project`
/// unions it additively and preserves a user whole-root denial.
fn add_project_filesystem(
    entries: &mut BTreeMap<String, SandboxFsPermission>,
    configured: BTreeMap<String, SandboxFsPermission>,
    workspace: &Path,
) {
    for (key, permission) in configured {
        match permission {
            SandboxFsPermission::Access(SandboxFsAccess::Deny) => {
                keep_project_deny(entries, &key, workspace);
            }
            SandboxFsPermission::Access(access) => {
                tracing::warn!(
                    "ignoring project `[sandbox.filesystem]` `{key}` with `{}` access: a project \
                     may only add denials",
                    fs_access_name(access)
                );
            }
            SandboxFsPermission::Scoped(children) => {
                if key.trim().starts_with(':') {
                    keep_project_scope(entries, key, children, workspace);
                } else {
                    for (subpath, access) in children {
                        if access != SandboxFsAccess::Deny {
                            tracing::warn!(
                                "ignoring project `[sandbox.filesystem]` `{key}` scope \
                                 `{subpath}` with `{}` access: a project may only add denials",
                                fs_access_name(access)
                            );
                            continue;
                        }
                        keep_project_deny(entries, &flatten_scope(&key, &subpath), workspace);
                    }
                }
            }
        }
    }
}

/// Keep a special-path scope; per-child paths remain relative to their base.
fn keep_project_scope(
    entries: &mut BTreeMap<String, SandboxFsPermission>,
    key: String,
    children: BTreeMap<String, SandboxFsAccess>,
    workspace: &Path,
) {
    let mut denied = BTreeMap::new();
    for (subpath, access) in children {
        if access != SandboxFsAccess::Deny {
            tracing::warn!(
                "ignoring project `[sandbox.filesystem]` `{key}` scope `{subpath}` with `{}` \
                 access: a project may only add denials",
                fs_access_name(access)
            );
            continue;
        }
        if subpath.trim().is_empty() {
            tracing::warn!("ignoring an empty project `[sandbox.filesystem]` scope under `{key}`");
            continue;
        }
        denied.insert(subpath, access);
    }
    if denied.is_empty() {
        return;
    }
    let permission = SandboxFsPermission::Scoped(denied);
    if let Err(error) = crate::task::validate_project_filesystem_entry(&key, &permission, workspace)
    {
        tracing::warn!("ignoring the project `[sandbox.filesystem]` scope under `{key}`: {error}");
        return;
    }
    entries.insert(key, permission);
}

/// Add one project deny rule, validated by the shared filesystem compiler.
///
/// Compiling one entry in isolation keeps an unenforceable repository rule
/// from poisoning the whole table (which would silently disable the file
/// tools' veto view for every user rule).
fn keep_project_deny(
    entries: &mut BTreeMap<String, SandboxFsPermission>,
    rule: &str,
    workspace: &Path,
) {
    let key = rule.trim();
    if key.is_empty() {
        return;
    }
    let permission = SandboxFsPermission::Access(SandboxFsAccess::Deny);
    if let Err(error) = crate::task::validate_project_filesystem_entry(key, &permission, workspace)
    {
        tracing::warn!(
            "ignoring the project deny rule `{key}`: it cannot be enforced as a denial ({error})"
        );
        return;
    }
    entries.insert(key.to_string(), permission);
}

/// One child of a normal-path scope as a standalone deny key. The compiler
/// anchors a relative result at the workspace exactly as it would have anchored
/// the nested entry under its base.
fn flatten_scope(key: &str, subpath: &str) -> String {
    let subpath = subpath.trim();
    if Path::new(subpath).is_absolute() {
        subpath.to_string()
    } else {
        format!(
            "{}/{}",
            key.trim_end_matches('/'),
            subpath.trim_start_matches('/')
        )
    }
}

fn fs_access_name(access: SandboxFsAccess) -> &'static str {
    match access {
        SandboxFsAccess::Read => "read",
        SandboxFsAccess::Write => "write",
        SandboxFsAccess::Deny => "deny",
    }
}

/// The workspace's own sandbox layer, read only when the config trusts it.
pub fn trusted_sandbox_layer(config: Option<&AppConfig>, workspace: &Path) -> ProjectSandboxLayer {
    let trust = WorkspaceTrust::for_config(config, workspace);
    project_sandbox_layer(trust.root(), trust.is_trusted())
}

/// Load the workspace's own sandbox layer, if the workspace is trusted.
///
/// `trusted` is decided by the caller from the merged lower layers, so this
/// function never reads a project file the trust decision has not admitted.
pub fn project_sandbox_layer(workspace: &Path, trusted: bool) -> ProjectSandboxLayer {
    if !trusted {
        return ProjectSandboxLayer::default();
    }
    let path = canonical_workspace(workspace).join(PROJECT_CONFIG);
    match read_layer(Some(&path)) {
        Some(value) => ProjectSandboxLayer::parse(&value, workspace),
        None => ProjectSandboxLayer::default(),
    }
}
