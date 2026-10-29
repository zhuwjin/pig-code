//! Configuration to policy entries.
//!
//! `[sandbox.filesystem]` is Codex's `permissions.<profile>.filesystem` table:
//! a path, a glob, a `dir/**` subtree or a special path maps to the access
//! granted there. The table is platform-neutral and compiles to the same
//! `SandboxEntry` list both backends resolve, so a rule means the same thing on
//! every host.
//!
//! What the compiler accepts mirrors Codex's, including what it refuses:
//!
//! * a **glob** key is deny-only — a pattern that grants read or write would
//!   have to be expanded into a set of paths the policy does not describe;
//! * a **trailing `/**`** is not a glob: for read/write it names the subtree
//!   root, for deny it stays a pattern (the whole subtree is refused);
//! * a **nested table** scopes further paths under the key and is deny-only;
//! * a **drive root** or a pattern without any literal text is refused, because
//!   it would deny or grant a whole volume by accident.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use pig_protocol::SandboxFsAccess;
use pig_protocol::SandboxFsPermission;

use super::SandboxAccess;
use super::SandboxEntry;
use super::SandboxPath;
use super::SandboxSpecialPath;
use super::canonical_root;
use super::deny_rules;

/// Compile the configured table into policy entries.
///
/// Relative paths anchor at the workspace and `~` expands to the home
/// directory, exactly like the `deny_read` shorthand.
pub(crate) fn compile(
    filesystem: &BTreeMap<String, SandboxFsPermission>,
    workspace: &Path,
) -> Result<Vec<SandboxEntry>, String> {
    let mut entries = Vec::new();
    for (key, permission) in filesystem {
        match permission {
            SandboxFsPermission::Access(access) => {
                entries.push(entry_for(key, *access, workspace)?);
            }
            SandboxFsPermission::Scoped(children) => {
                for (subpath, access) in children {
                    if *access != SandboxFsAccess::Deny {
                        return Err(format!(
                            "`{key}` scopes `{subpath}` with `{}` access; a nested filesystem entry \
                             is deny-only",
                            access_name(*access)
                        ));
                    }
                    let base = base_path(key, workspace)?;
                    let child = join_relative(&base, subpath, key)?;
                    entries.push(SandboxEntry::deny(child));
                }
            }
        }
    }
    Ok(entries)
}

/// Validate one project-supplied entry with the same compiler the policy uses.
///
/// Project config is repository-authored data: an entry that the compiler
/// would reject must be dropped before it reaches the table, because a single
/// invalid entry fails the whole compiled table and silently disables the file
/// tools' veto view. Compiling one entry in isolation lets the config layer
/// keep the enforceable rules and warn about the rest.
pub(crate) fn validate_entry(
    key: &str,
    permission: &SandboxFsPermission,
    workspace: &Path,
) -> Result<(), String> {
    let table = BTreeMap::from([(key.to_string(), permission.clone())]);
    compile(&table, workspace).map(|_| ())
}

/// One entry: a special path, a literal path, or a deny-only pattern.
fn entry_for(key: &str, access: SandboxFsAccess, workspace: &Path) -> Result<SandboxEntry, String> {
    if let Some(special) = special_path(key, workspace) {
        return Ok(SandboxEntry::new(special, sandbox_access(access)));
    }
    let normalized = deny_rules::normalize_pattern(key, workspace);
    if is_pattern(&normalized) {
        if access != SandboxFsAccess::Deny {
            return Err(format!(
                "the filesystem glob `{key}` only supports `deny` access; use an exact path or a \
                 trailing `/**` for `{}` subtree access",
                access_name(access)
            ));
        }
        if !deny_rules::has_literal_text(key) {
            return Err(format!(
                "the filesystem rule `{key}` matches everything; name a file or directory"
            ));
        }
        return Ok(SandboxEntry::deny_glob(normalized));
    }
    let path = literal_path(key, workspace)?;
    Ok(SandboxEntry::new(
        SandboxPath::Path { path },
        sandbox_access(access),
    ))
}

/// Whether the normalized key still carries a glob wildcard. A trailing `/**`
/// is a subtree marker rather than a pattern, so it is stripped first — the
/// same distinction Codex draws (`compile_read_write_glob_path`).
fn is_pattern(normalized: &str) -> bool {
    let without_subtree = normalized.strip_suffix("/**").unwrap_or(normalized);
    without_subtree.contains(['*', '?', '[', ']', '{', '}'])
}

/// The special paths this build materializes. Anything else is refused rather
/// than silently dropped: a policy entry the host cannot enforce must fail the
/// command, not disappear.
fn special_path(key: &str, workspace: &Path) -> Option<SandboxPath> {
    match key {
        ":root" => Some(SandboxPath::Special {
            value: SandboxSpecialPath::Root,
        }),
        // Codex's canonical name, with its pre-rename alias.
        ":workspace_roots" | ":project_roots" => Some(SandboxPath::Path {
            path: canonical_root(workspace.to_path_buf()),
        }),
        ":tmpdir" => Some(SandboxPath::Path {
            path: canonical_root(std::env::temp_dir()),
        }),
        _ => None,
    }
}

/// The path a nested table scopes its children under.
fn base_path(key: &str, workspace: &Path) -> Result<PathBuf, String> {
    if let Some(SandboxPath::Path { path }) = special_path(key, workspace) {
        return Ok(path);
    }
    literal_path(key, workspace)
}

/// Resolve a literal key to an absolute path.
fn literal_path(key: &str, workspace: &Path) -> Result<PathBuf, String> {
    if key.starts_with(':') {
        return Err(format!(
            "the filesystem path `{key}` is a special path this build does not support; \
             `:root`, `:workspace_roots` and `:tmpdir` are available"
        ));
    }
    let normalized = deny_rules::normalize_pattern(key, workspace);
    let path = PathBuf::from(normalized.trim_end_matches("/**"));
    // Syntactic, so it means the same thing on every platform: refuse before
    // the absolute-path check, whose message would send the user elsewhere.
    if deny_rules::is_drive_root(&path) {
        return Err(format!(
            "the filesystem path `{key}` names a whole drive; name a directory inside it"
        ));
    }
    if !path.is_absolute() {
        return Err(format!(
            "the filesystem path `{key}` could not be resolved to an absolute path"
        ));
    }
    let path = canonical_root(path);
    if deny_rules::is_drive_root(&path) {
        return Err(format!(
            "the filesystem path `{key}` names a whole drive; name a directory inside it"
        ));
    }
    Ok(path)
}

/// A nested entry's subpath: relative to the key's path, or absolute on its
/// own.
fn join_relative(base: &Path, subpath: &str, key: &str) -> Result<PathBuf, String> {
    let subpath = subpath.trim_matches('/');
    if subpath.is_empty() {
        return Err(format!(
            "the nested filesystem entry under `{key}` is empty; name a file or directory"
        ));
    }
    let candidate = Path::new(subpath);
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        base.join(candidate)
    };
    Ok(canonical_root(joined))
}

fn sandbox_access(access: SandboxFsAccess) -> SandboxAccess {
    match access {
        SandboxFsAccess::Read => SandboxAccess::Read,
        SandboxFsAccess::Write => SandboxAccess::Write,
        SandboxFsAccess::Deny => SandboxAccess::Deny,
    }
}

fn access_name(access: SandboxFsAccess) -> &'static str {
    match access {
        SandboxFsAccess::Read => "read",
        SandboxFsAccess::Write => "write",
        SandboxFsAccess::Deny => "deny",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An absolute path on the host running the test: `C:\...` on Windows,
    /// `/...` elsewhere. The compiler is platform-neutral, so the fixtures
    /// have to be too.
    fn host_path(relative: &str) -> PathBuf {
        let root = if cfg!(windows) {
            PathBuf::from("C:\\")
        } else {
            PathBuf::from("/")
        };
        root.join(relative)
    }

    fn workspace() -> PathBuf {
        host_path("work/repo")
    }

    fn table(entries: &[(&str, SandboxFsPermission)]) -> BTreeMap<String, SandboxFsPermission> {
        entries
            .iter()
            .map(|(key, value)| (key.to_string(), value.clone()))
            .collect()
    }

    fn access(value: SandboxFsAccess) -> SandboxFsPermission {
        SandboxFsPermission::Access(value)
    }

    fn path_of(entry: &SandboxEntry) -> PathBuf {
        match &entry.path {
            SandboxPath::Path { path } => path.clone(),
            other => panic!("expected a path entry, got {other:?}"),
        }
    }

    #[test]
    fn a_literal_path_compiles_to_its_access() {
        let vault = host_path("data/vault");
        let notes = host_path("data/notes");
        let build = host_path("data/build");
        let entries = compile(
            &table(&[
                (vault.to_str().unwrap(), access(SandboxFsAccess::Deny)),
                (notes.to_str().unwrap(), access(SandboxFsAccess::Read)),
                (build.to_str().unwrap(), access(SandboxFsAccess::Write)),
            ]),
            &workspace(),
        )
        .expect("compile");
        assert_eq!(entries.len(), 3);
        let by_path = |path: &PathBuf| {
            entries
                .iter()
                .find(|entry| path_of(entry) == *path)
                .unwrap_or_else(|| panic!("no entry for {}", path.display()))
        };
        assert_eq!(by_path(&vault).access, SandboxAccess::Deny);
        assert_eq!(by_path(&notes).access, SandboxAccess::Read);
        assert_eq!(by_path(&build).access, SandboxAccess::Write);
    }

    #[test]
    fn a_relative_path_anchors_at_the_workspace() {
        let entries = compile(
            &table(&[("secrets", access(SandboxFsAccess::Deny))]),
            &workspace(),
        )
        .expect("compile");
        assert_eq!(path_of(&entries[0]), workspace().join("secrets"));
    }

    #[test]
    fn a_glob_is_deny_only() {
        let entries = compile(
            &table(&[("**/*.pem", access(SandboxFsAccess::Deny))]),
            &workspace(),
        )
        .expect("compile");
        assert!(matches!(entries[0].path, SandboxPath::GlobPattern { .. }));

        let error = compile(
            &table(&[("**/*.pem", access(SandboxFsAccess::Read))]),
            &workspace(),
        )
        .expect_err("a read glob cannot be enforced");
        assert!(error.contains("only supports `deny`"), "{error}");
    }

    #[test]
    fn a_trailing_globstar_is_a_subtree_path_not_a_pattern() {
        let tree = host_path("data/tree");
        let key = format!(
            "{}/**",
            tree.display()
                .to_string()
                .replace(std::path::MAIN_SEPARATOR, "/")
        );
        // Read/write: the subtree root.
        let entries = compile(
            &table(&[(&key, access(SandboxFsAccess::Write))]),
            &workspace(),
        )
        .expect("compile");
        assert_eq!(path_of(&entries[0]), tree);
        assert_eq!(entries[0].access, SandboxAccess::Write);
        // Deny: the same literal path. A denied directory covers its subtree on
        // both backends, so the pattern would only add a walk — and would stop
        // denying the directory itself, which the glob does not match.
        let entries = compile(
            &table(&[(&key, access(SandboxFsAccess::Deny))]),
            &workspace(),
        )
        .expect("compile");
        assert_eq!(path_of(&entries[0]), tree);
        assert_eq!(entries[0].access, SandboxAccess::Deny);
    }

    #[test]
    fn a_nested_table_scopes_deny_entries() {
        let repo = host_path("data/repo");
        let key = repo.display().to_string();
        let entries = compile(
            &table(&[(
                &key,
                SandboxFsPermission::Scoped(BTreeMap::from([(
                    "sub".to_string(),
                    SandboxFsAccess::Deny,
                )])),
            )]),
            &workspace(),
        )
        .expect("compile");
        assert_eq!(path_of(&entries[0]), repo.join("sub"));
        assert_eq!(entries[0].access, SandboxAccess::Deny);

        let error = compile(
            &table(&[(
                &key,
                SandboxFsPermission::Scoped(BTreeMap::from([(
                    "sub".to_string(),
                    SandboxFsAccess::Read,
                )])),
            )]),
            &workspace(),
        )
        .expect_err("a nested read entry cannot be enforced");
        assert!(error.contains("deny-only"), "{error}");
    }

    #[test]
    fn special_paths_materialize() {
        let entries = compile(
            &table(&[
                (":root", access(SandboxFsAccess::Read)),
                (":workspace_roots", access(SandboxFsAccess::Write)),
                (":tmpdir", access(SandboxFsAccess::Read)),
            ]),
            &workspace(),
        )
        .expect("compile");
        assert!(entries.iter().any(|entry| matches!(
            entry.path,
            SandboxPath::Special {
                value: SandboxSpecialPath::Root
            }
        )));
        let workspace_entry = entries
            .iter()
            .find(|entry| entry.access == SandboxAccess::Write)
            .expect("workspace entry");
        assert_eq!(path_of(workspace_entry), workspace());
        let tmp = entries
            .iter()
            .find(|entry| {
                entry.access == SandboxAccess::Read
                    && matches!(&entry.path, SandboxPath::Path { path } if *path != workspace())
            })
            .expect("tmp entry");
        assert_eq!(path_of(tmp), canonical_root(std::env::temp_dir()));
    }

    #[test]
    fn an_unsupported_special_path_is_refused() {
        let error = compile(
            &table(&[(":minimal", access(SandboxFsAccess::Read))]),
            &workspace(),
        )
        .expect_err("an unsupported special path must not be dropped");
        assert!(error.contains("does not support"), "{error}");
    }

    /// A whole drive is refused on every platform: `C:\` is syntactically a
    /// drive root even where it is not an absolute path.
    #[test]
    fn a_whole_drive_is_refused() {
        let error = compile(
            &table(&[("C:\\", access(SandboxFsAccess::Deny))]),
            &workspace(),
        )
        .expect_err("a drive root is not a usable rule");
        assert!(error.contains("whole drive"), "{error}");
    }
}
