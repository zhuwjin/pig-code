//! Read-deny rules: one platform-neutral table, shared by both backends.
//!
//! Codex expresses denials as `Deny` entries in the filesystem policy, and each
//! backend enforces them in its own way: Seatbelt compiles the globs into
//! access-time regexes, the Windows MXC path materializes them into concrete
//! paths by walking. We keep that split — one rule table, two enforcement
//! strategies — with one deliberate difference: **a rule set is made of scoped
//! groups with their own exceptions** (`.env.example`, `*.pub`), where Codex has
//! no exception concept at all. Flattening the groups into one deny list would
//! make an exception lift *every* rule that matches the same path (for example
//! a user rule `secrets/**` would stop denying `secrets/config.env.example`,
//! which an earlier audit fixed), so a rule carries its group's exceptions with
//! it.
//!
//! There is no built-in group any more (2026-10-20): the sensitive-file table is
//! seeded into the user's `[[sandbox.deny]]` configuration, so every rule here
//! arrives as data — either a structured group or a `deny_read` string.
//!
//! The rule set is built once from configuration, in the parent process, and
//! then travels to the backend as data: the Windows helper materializes it, the
//! macOS builder compiles it to regexes. Nothing here is Windows-specific.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use globset::GlobSet;
use globset::GlobSetBuilder;
use serde::Deserialize;
use serde::Serialize;

/// One deny rule group: the globs that deny, plus the globs that lift this
/// group's denials (and only this group's).
///
/// Exceptions carry the access they restore, because on Seatbelt an exception
/// is an allow that has to re-grant exactly what the denial removed:
/// `exceptions` are readable only (a `.env.example`, a configured read entry),
/// `write_exceptions` regain write access as well (a configured write entry
/// inside a denied subtree). The Windows backend only needs to know that the
/// path is not denied — the ambient grants decide the rest.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DenyRuleGroup {
    pub globs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exceptions: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub write_exceptions: Vec<String>,
}

impl From<&pig_protocol::SandboxDenyGroup> for DenyRuleGroup {
    fn from(group: &pig_protocol::SandboxDenyGroup) -> Self {
        Self {
            globs: group.globs.clone(),
            exceptions: group.exceptions.clone(),
            write_exceptions: group.write_exceptions.clone(),
        }
    }
}

/// How a configured rule is interpreted: a plain path needs no walk, a glob is
/// expanded at launch. The settings page shows this per row.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DenyRuleKind {
    Path,
    Glob,
}

/// Classify a rule without touching the filesystem (the settings page calls
/// this for every row of every frame).
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn classify_deny_rule(rule: &str) -> DenyRuleKind {
    if rule.trim().contains(['*', '?', '[', ']', '{', '}']) {
        DenyRuleKind::Glob
    } else {
        DenyRuleKind::Path
    }
}

/// Validate one configured rule through the same path the launcher uses, so a
/// rule the editor accepts can never fail a launch.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn validate_deny_rule(rule: &str) -> Result<DenyRuleKind, String> {
    let trimmed = rule.trim();
    if trimmed.is_empty() {
        return Err("the rule is empty".to_string());
    }
    let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let normalized = normalize_pattern(trimmed, &workspace);
    if normalized.contains(['*', '?', '[', ']', '{', '}']) {
        if !has_literal_text(trimmed) {
            return Err(format!(
                "the rule `{trimmed}` matches everything; name a file or directory"
            ));
        }
        // Compile exactly as the matcher will.
        let mut builder = GlobSetBuilder::new();
        add_glob(&mut builder, &normalized).map_err(|error| error.to_string())?;
        builder
            .build()
            .map_err(|error| format!("invalid sandbox deny rule `{trimmed}`: {error}"))?;
        // A glob anchored at a drive root would walk an entire volume on every
        // command; reject it. A glob with no literal directory (`**/*.pem`)
        // needs no extra scan root — the policy's writable roots are scanned
        // anyway, under the same entry bound.
        match literal_prefix(&normalized) {
            Some(prefix) if is_drive_root(&prefix) || prefix.parent().is_none() => Err(format!(
                "the rule `{trimmed}` would scan an entire drive; start it with a directory"
            )),
            _ => Ok(DenyRuleKind::Glob),
        }
    } else {
        let path = PathBuf::from(&normalized);
        if !path.is_absolute() && !normalized.starts_with('/') {
            return Err(format!(
                "the rule `{trimmed}` could not be resolved to an absolute path"
            ));
        }
        if is_drive_root(&path) || path.parent().is_none() {
            return Err(format!(
                "the rule `{trimmed}` is a drive root; deny a directory inside it instead"
            ));
        }
        Ok(DenyRuleKind::Path)
    }
}

/// `C:\` / `C:/` — syntactic, so it means the same thing on every platform.
pub(crate) fn is_drive_root(path: &Path) -> bool {
    let text = path.to_string_lossy();
    let mut chars = text.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_alphabetic())
        && matches!(chars.next(), Some(':'))
        && matches!(chars.next(), Some('\\' | '/'))
        && chars.next().is_none()
}

/// A compiled rule set: what a backend enforces.
#[derive(Clone, Debug, Default)]
pub(crate) struct DenyRules {
    pub(crate) groups: Vec<RuleMatcher>,
    /// Extra directories a user glob asked for, beyond the policy roots.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) extra_scan_roots: Vec<PathBuf>,
}

#[derive(Clone, Debug)]
pub(crate) struct RuleMatcher {
    pub(crate) deny: Arc<GlobSet>,
    /// Paths the group does not deny: read-only exceptions and write
    /// exceptions both lift the denial, so the walker treats them alike.
    pub(crate) exceptions: Arc<GlobSet>,
}

impl DenyRules {
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) fn extra_scan_roots(&self) -> &[PathBuf] {
        &self.extra_scan_roots
    }

    /// Whether `path` is denied: a group's globs match and that group's own
    /// exceptions do not. Exceptions from another group never apply.
    pub(crate) fn denies_path(&self, path: &Path) -> bool {
        let mut candidate = String::new();
        self.denies_normalized(normalize_candidate_into(path, &mut candidate))
    }

    /// Whether any deny rule exists (Codex's `has_denied_read_restrictions`).
    pub(crate) fn has_rules(&self) -> bool {
        !self.groups.is_empty()
    }

    /// Hot-path variant for walkers that keep one scratch buffer per worker:
    /// avoids allocating a normalized string for every visited entry.
    pub(crate) fn denies_normalized(&self, candidate: &str) -> bool {
        self.groups
            .iter()
            .any(|rule| rule.deny.is_match(candidate) && !rule.exceptions.is_match(candidate))
    }
}

/// One compiled rule set plus the literal paths that need no walk.
#[derive(Debug)]
pub(crate) struct CompiledRules {
    /// The compiled matcher; the product path rebuilds it from 197121 in the
    /// helper, so only the Windows unit tests read it directly.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) rules: DenyRules,
    /// The source groups, in the wire form: the backend recompiles from these.
    pub(crate) groups: Vec<DenyRuleGroup>,
    pub(crate) literal_denials: Vec<PathBuf>,
}

/// Compile configuration into a rule set, once per command.
///
/// `configured` are the structured `[[sandbox.deny]]` groups (each carries its
/// own exceptions); `user_patterns` are the simple `[sandbox] deny_read` rules,
/// each a literal path or a glob. The result is platform-neutral: the same set
/// goes to both backends.
pub(crate) fn compile(
    configured: &[DenyRuleGroup],
    user_patterns: &[String],
    workspace: &Path,
) -> Result<CompiledRules, String> {
    let mut rules: Vec<RuleMatcher> = Vec::new();
    let mut groups: Vec<DenyRuleGroup> = Vec::new();
    let mut literal_denials: Vec<PathBuf> = Vec::new();
    let mut extra_scan_roots: Vec<PathBuf> = Vec::new();

    for group in configured {
        if group.globs.is_empty() {
            // A group with no globs denies nothing; keeping it would only add
            // an empty matcher (and an empty exception scope) to the walk.
            continue;
        }
        for raw in &group.globs {
            if !has_literal_text(raw) {
                return Err(format!(
                    "the sandbox deny rule `{raw}` matches everything; name a file or directory"
                ));
            }
        }
        // Configured patterns go through the same normalization as `deny_read`
        // strings, so `secrets/**` in a group anchors at the workspace and
        // `~/.ssh/x` expands, exactly like a simple rule. A bare path means its
        // whole subtree, exactly like a `deny_read` literal.
        let deny = expand_group_patterns(&group.globs, workspace);
        let exceptions = normalize_all(&group.exceptions, workspace);
        let write_exceptions = normalize_all(&group.write_exceptions, workspace);
        let mut exception_patterns = exceptions.clone();
        exception_patterns.extend(write_exceptions.iter().cloned());
        rules.push(RuleMatcher {
            deny: Arc::new(compile_globs_owned(&deny, "rule")?),
            exceptions: Arc::new(compile_globs_owned(&exception_patterns, "exception")?),
        });
        collect_scan_roots(&deny, &mut extra_scan_roots)?;
        groups.push(DenyRuleGroup {
            globs: deny,
            exceptions,
            write_exceptions,
        });
    }

    // Every `deny_read` rule shares one matcher and has no exceptions: a simple
    // rule can only add denials.
    let mut user_builder = GlobSetBuilder::new();
    let mut user_globs: Vec<String> = Vec::new();
    for raw in user_patterns {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let normalized = normalize_pattern(raw, workspace);
        if normalized.contains(['*', '?', '[', '{']) {
            if !has_literal_text(raw) {
                return Err(format!(
                    "the sandbox deny rule `{raw}` matches everything; name a file or directory"
                ));
            }
            add_glob(&mut user_builder, &normalized)?;
            if !user_globs.contains(&normalized) {
                user_globs.push(normalized.clone());
            }
            collect_scan_roots(&[normalized], &mut extra_scan_roots)?;
        } else {
            let path = PathBuf::from(&normalized);
            if is_drive_root(&path) || path.parent().is_none() {
                return Err(format!(
                    "the sandbox deny rule `{raw}` is a drive root; deny a directory inside it \
                     instead"
                ));
            }
            if !literal_denials.contains(&path) {
                literal_denials.push(path);
            }
        }
    }
    if !user_globs.is_empty() {
        groups.push(DenyRuleGroup {
            globs: user_globs,
            exceptions: Vec::new(),
            write_exceptions: Vec::new(),
        });
        // Only a non-empty rule set contributes a matcher: an always-present
        // empty one made `has_rules()` true even with no configured rule, which
        // forced every `require_escalated` approval into the restricted form.
        rules.push(RuleMatcher {
            deny: Arc::new(
                user_builder
                    .build()
                    .map_err(|error| format!("invalid sandbox deny rule: {error}"))?,
            ),
            exceptions: Arc::new(
                GlobSetBuilder::new()
                    .build()
                    .map_err(|error| format!("invalid sandbox deny rule: {error}"))?,
            ),
        });
    }

    Ok(CompiledRules {
        rules: DenyRules {
            groups: rules,
            extra_scan_roots,
        },
        groups,
        literal_denials,
    })
}

/// Normalize a group's patterns into the form both backends match against.
fn normalize_all(patterns: &[String], workspace: &Path) -> Vec<String> {
    patterns
        .iter()
        .map(|pattern| pattern.trim())
        .filter(|pattern| !pattern.is_empty())
        .map(|pattern| normalize_pattern(pattern, workspace))
        .collect()
}

/// A group's deny patterns: a glob is matched as written, a bare path denies its
/// whole subtree — the same reading a `deny_read` literal gets (`secrets` means
/// the directory and everything in it, not only the directory's own path).
fn expand_group_patterns(patterns: &[String], workspace: &Path) -> Vec<String> {
    let mut expanded: Vec<String> = Vec::new();
    for pattern in normalize_all(patterns, workspace) {
        let has_wildcard = pattern.contains(['*', '?', '[', ']', '{', '}']);
        if !expanded.contains(&pattern) {
            expanded.push(pattern.clone());
        }
        if !has_wildcard {
            let subtree = format!("{}/**", pattern.trim_end_matches('/'));
            if !expanded.contains(&subtree) {
                expanded.push(subtree);
            }
        }
    }
    expanded
}

/// A glob's literal prefix becomes an extra scan root, so a rule that points
/// outside the policy's own roots is still walked (Windows) or known to need no
/// walk at all (`**/...`).
fn collect_scan_roots(
    patterns: &[String],
    extra_scan_roots: &mut Vec<PathBuf>,
) -> Result<(), String> {
    for pattern in patterns {
        match literal_prefix(pattern) {
            Some(prefix) if is_drive_root(&prefix) || prefix.parent().is_none() => {
                return Err(format!(
                    "the sandbox deny rule `{pattern}` would scan an entire drive; start it \
                     with a directory"
                ));
            }
            Some(prefix) => {
                // Canonicalize so deduplication against the policy's own
                // (canonicalized) roots works across separator spellings.
                let prefix = canonical(&prefix);
                if prefix.is_dir() && !extra_scan_roots.contains(&prefix) {
                    extra_scan_roots.push(prefix);
                }
            }
            // `**/*.pem`: matches under every writable root, which the policy
            // scans anyway. Nothing extra to add.
            None => {}
        }
    }
    Ok(())
}

/// Rebuild matchers for a rule set that already crossed the transport (the
/// Windows helper). No normalization happens here: the parent compiled it.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn from_groups(groups: &[DenyRuleGroup]) -> Result<DenyRules, String> {
    let mut rules: Vec<RuleMatcher> = Vec::with_capacity(groups.len());
    let mut extra_scan_roots: Vec<PathBuf> = Vec::new();
    for group in groups {
        let deny = compile_globs_owned(&group.globs, "rule")?;
        let mut exception_patterns = group.exceptions.clone();
        exception_patterns.extend(group.write_exceptions.iter().cloned());
        let exceptions = compile_globs_owned(&exception_patterns, "exception")?;
        // A glob's literal prefix becomes an extra scan root, so a rule that
        // points outside the workspace is still walked.
        for pattern in &group.globs {
            if let Some(prefix) = literal_prefix(pattern) {
                let prefix = canonical(&prefix);
                if prefix.is_dir() && !is_drive_root(&prefix) && !extra_scan_roots.contains(&prefix)
                {
                    extra_scan_roots.push(prefix);
                }
            }
        }
        rules.push(RuleMatcher {
            deny: Arc::new(deny),
            exceptions: Arc::new(exceptions),
        });
    }
    Ok(DenyRules {
        groups: rules,
        extra_scan_roots,
    })
}

fn compile_globs(patterns: &[&str], what: &str) -> Result<GlobSet, String> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        add_glob(&mut builder, pattern)?;
    }
    builder
        .build()
        .map_err(|error| format!("invalid sandbox deny {what}: {error}"))
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn compile_globs_owned(patterns: &[String], what: &str) -> Result<GlobSet, String> {
    let patterns: Vec<&str> = patterns.iter().map(String::as_str).collect();
    compile_globs(&patterns, what)
}

/// Compile one glob the way both backends interpret it.
pub(crate) fn add_glob(builder: &mut GlobSetBuilder, pattern: &str) -> Result<(), String> {
    // Case-insensitive on purpose: Windows paths are, and the previous
    // hand-rolled predicate lowercased every name. `literal_separator` keeps a
    // single `*` from crossing a directory boundary, so `**/*.env` needs `**`
    // to reach nested files — the semantics users expect from gitignore.
    let glob = globset::GlobBuilder::new(pattern)
        .case_insensitive(true)
        .literal_separator(true)
        .build()
        .map_err(|error| format!("invalid sandbox deny rule `{pattern}`: {error}"))?;
    builder.add(glob);
    Ok(())
}

/// Normalize a candidate path to the same spelling the patterns use.
///
/// The Windows verbatim prefix is stripped: `std::fs::canonicalize` (what the
/// file tools resolve to) returns `\\?\C:\...`, while every pattern and policy
/// path is built from `dunce`-canonicalized spellings without it. Mixing the
/// two makes prefix comparisons silently fail, which is how a credential
/// directory could be read without an approval.
pub(crate) fn normalize_candidate(path: &Path) -> String {
    let mut buffer = String::new();
    normalize_candidate_into(path, &mut buffer).to_string()
}

/// Allocation-free variant for scan loops: the caller reuses `buffer` for every
/// visited entry (one buffer per walker thread).
pub(crate) fn normalize_candidate_into<'a>(path: &Path, buffer: &'a mut String) -> &'a str {
    buffer.clear();
    // `to_string_lossy` borrows for valid Unicode; the push loop reuses the
    // buffer's capacity instead of allocating a new String per entry.
    for character in path.to_string_lossy().chars() {
        buffer.push(if character == '\\' { '/' } else { character });
    }
    if buffer.starts_with("//?/") {
        buffer.drain(..4);
    }
    buffer.as_str()
}

/// Normalize one configured rule into the canonical pattern form:
/// `~` expanded, forward slashes, bare globs matched at any depth, relative
/// patterns anchored at the workspace.
pub(crate) fn normalize_pattern(pattern: &str, workspace: &Path) -> String {
    // `dunce` (via `canonical_root`) strips the Windows `\?\` prefix; here the
    // prefix may also arrive already slash-normalized as `//?/`, whose `?`
    // would otherwise be read as a glob wildcard.
    let mut pattern = strip_verbatim(&pattern.trim().replace('\\', "/"));
    if let Some(home) = home_dir() {
        let home = strip_verbatim(&home.to_string_lossy().replace('\\', "/"));
        if pattern == "~" {
            pattern = home;
        } else if let Some(rest) = pattern.strip_prefix("~/") {
            pattern = format!("{home}/{rest}");
        }
    }
    let is_absolute =
        pattern.starts_with('/') || Path::new(&pattern).is_absolute() || drive_absolute(&pattern);
    // A slash-less **glob** (`.env.*`, `*.pem`) matches at any depth, which is
    // gitignore's rule for slash-less patterns. This has to happen before the
    // workspace anchor: after anchoring, every pattern contains the workspace's
    // separators and the check can no longer tell. A slash-less *literal*
    // (`secrets`) stays a workspace-relative path instead — users read it as
    // "the secrets directory here".
    let any_depth = !pattern.contains('/') && pattern.contains(['*', '?', '[', '{']);
    if any_depth {
        pattern = format!("**/{pattern}");
    }
    // A pattern that already starts with `**` is root-independent and must not
    // be anchored either: `**/secrets` means "a secrets directory at any
    // depth".
    if !is_absolute && !any_depth && !pattern.starts_with("**") {
        let workspace = super::canonical_root(workspace.to_path_buf());
        let workspace = strip_verbatim(&workspace.to_string_lossy().replace('\\', "/"));
        pattern = format!("{}/{}", workspace.trim_end_matches('/'), pattern);
    }
    pattern
}

/// Drop a Windows verbatim prefix after separator normalization
/// (`//?/C:/...` becomes `C:/...`).
fn strip_verbatim(pattern: &str) -> String {
    pattern.strip_prefix("//?/").unwrap_or(pattern).to_string()
}

/// `C:/...` (and `C:\...` before separator normalization). `Path::is_absolute`
/// is not enough on Windows: it also accepts a bare `C:` prefix without a root.
fn drive_absolute(pattern: &str) -> bool {
    let mut chars = pattern.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_alphabetic())
        && matches!(chars.next(), Some(':'))
        && matches!(chars.next(), Some('\\' | '/'))
}

fn home_dir() -> Option<PathBuf> {
    ["HOME", "USERPROFILE"]
        .iter()
        .find_map(|key| std::env::var_os(key).map(PathBuf::from))
}

/// The literal directory part of a glob: the path up to the first component
/// that contains a wildcard. `None` when the pattern starts with a wildcard
/// (nothing to anchor on).
pub(crate) fn literal_prefix(pattern: &str) -> Option<PathBuf> {
    let first = pattern
        .char_indices()
        .find_map(|(index, ch)| matches!(ch, '*' | '?' | '[' | ']' | '{' | '}').then_some(index))?;
    let head = &pattern[..first];
    // Cut at the last separator so the prefix is a real directory.
    let end = head.rfind('/').filter(|index| *index > 0)?;
    let prefix = &pattern[..end];
    if prefix.is_empty() {
        return None;
    }
    // Keep a Windows drive root intact (`C:` → `C:/`).
    let prefix = if drive_absolute(&format!("{prefix}/")) {
        PathBuf::from(format!("{prefix}/"))
    } else {
        PathBuf::from(prefix)
    };
    Some(prefix)
}

/// Whether a glob has at least one literal character besides wildcards, so it
/// cannot match the entire filesystem.
pub(crate) fn has_literal_text(pattern: &str) -> bool {
    pattern
        .chars()
        .any(|ch| !matches!(ch, '*' | '?' | '[' | ']' | '{' | '}' | '/' | '\\'))
}

fn canonical(path: &Path) -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    }
    #[cfg(not(target_os = "windows"))]
    {
        path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> PathBuf {
        PathBuf::from("/work/repo")
    }

    /// The groups a fresh install is seeded with, in the compiled shape.
    fn seeded() -> Vec<DenyRuleGroup> {
        crate::config::default_deny_groups()
            .iter()
            .map(DenyRuleGroup::from)
            .collect()
    }

    /// The seeded table denies the names the file tools refuse, and each
    /// group's own exceptions stay readable.
    #[test]
    fn the_seeded_groups_deny_their_names_and_keep_their_exceptions() {
        let workspace = workspace();
        let compiled = compile(&seeded(), &[], &workspace).expect("compile");
        for denied in [
            workspace.join("app").join(".env"),
            workspace.join("app").join(".env.local"),
            workspace.join("keys").join("id_rsa"),
            workspace.join("keys").join("id_ed25519.pub.bak"),
            workspace.join(".aws").join("credentials"),
            workspace.join(".gcp").join("credentials"),
        ] {
            assert!(compiled.rules.denies_path(&denied), "{denied:?}");
        }
        for allowed in [
            workspace.join("app").join(".env.example"),
            workspace.join("app").join(".env.sample"),
            workspace.join("keys").join("id_rsa.pub"),
            // `.gcp` only denies the credentials file itself.
            workspace.join(".gcp").join("config"),
        ] {
            assert!(!compiled.rules.denies_path(&allowed), "{allowed:?}");
        }
    }

    #[test]
    fn a_group_exception_lifts_only_that_group() {
        let workspace = workspace();
        let mut groups = seeded();
        // Keep the `.env` group (index 0) and add a user rule that matches the
        // same family.
        groups.truncate(1);
        let compiled = compile(&groups, &["secrets/**".to_string()], &workspace).expect("compile");
        // The env group's exception does not lift the user's rule.
        let path = workspace.join("secrets").join("config.env.example");
        assert!(
            compiled.rules.denies_path(&path),
            "a simple rule has no exceptions"
        );
        // And `.env.example` itself stays readable.
        let example = workspace.join("app").join(".env.example");
        assert!(!compiled.rules.denies_path(&example));
        // While `.env` is denied.
        assert!(
            compiled
                .rules
                .denies_path(&workspace.join("app").join(".env"))
        );
    }

    /// A group is removed by deleting its entry from the configuration: leaving
    /// it out of the table leaves its paths alone, and every other group keeps
    /// applying.
    #[test]
    fn a_group_that_is_not_configured_denies_nothing() {
        let workspace = workspace();
        let without_env: Vec<DenyRuleGroup> = seeded().into_iter().skip(1).collect();
        let compiled = compile(&without_env, &[], &workspace).expect("compile");
        assert!(
            !compiled
                .rules
                .denies_path(&workspace.join("app").join(".env"))
        );
        // Other groups are unaffected.
        assert!(
            compiled
                .rules
                .denies_path(&workspace.join("app").join("id_rsa"))
        );

        // An empty table is exactly "no rules at all".
        let compiled = compile(&[], &[], &workspace).expect("compile");
        assert!(
            !compiled
                .rules
                .denies_path(&workspace.join("app").join(".env"))
        );
        assert!(
            !compiled
                .rules
                .denies_path(&workspace.join("app").join("id_rsa"))
        );
        assert!(compiled.groups.is_empty(), "only empty matchers are built");
    }

    /// A configured group is normalized like a `deny_read` rule: relative
    /// patterns anchor at the workspace, `*`-less extras stay literal.
    #[test]
    fn configured_groups_are_normalized_like_simple_rules() {
        let workspace = workspace();
        let groups = vec![DenyRuleGroup {
            globs: vec!["secrets/**".to_string()],
            exceptions: vec!["secrets/public/**".to_string()],
            write_exceptions: Vec::new(),
        }];
        let compiled = compile(&groups, &[], &workspace).expect("compile");
        assert!(
            compiled
                .rules
                .denies_path(&workspace.join("secrets").join("token.txt"))
        );
        assert!(
            !compiled
                .rules
                .denies_path(&workspace.join("secrets").join("public").join("keep.txt"))
        );
        // The wire form carries the normalized patterns, so the backend
        // recompiles exactly what was compiled here.
        assert_eq!(compiled.groups[0].globs, vec!["/work/repo/secrets/**"]);
        assert_eq!(
            compiled.groups[0].exceptions,
            vec!["/work/repo/secrets/public/**"]
        );
    }

    /// A write exception lifts the denial like a read exception does; the
    /// difference between the two lives in the Seatbelt profile, not in the
    /// matcher.
    #[test]
    fn a_write_exception_lifts_its_group() {
        let workspace = workspace();
        let groups = vec![DenyRuleGroup {
            globs: vec!["**/.env".to_string()],
            exceptions: Vec::new(),
            write_exceptions: vec!["**/.env.dist".to_string()],
        }];
        let compiled = compile(&groups, &[], &workspace).expect("compile");
        assert!(
            compiled
                .rules
                .denies_path(&workspace.join("app").join(".env"))
        );
        assert!(
            !compiled
                .rules
                .denies_path(&workspace.join("app").join(".env.dist"))
        );
    }

    /// A bare path in a group denies its whole subtree, exactly like a
    /// `deny_read` literal (`secrets` means the directory and what is in it).
    #[test]
    fn a_bare_path_in_a_group_denies_its_subtree() {
        let workspace = workspace();
        let groups = vec![DenyRuleGroup {
            globs: vec!["vault".to_string()],
            exceptions: vec!["vault/notes.md".to_string()],
            write_exceptions: Vec::new(),
        }];
        let compiled = compile(&groups, &[], &workspace).expect("compile");
        assert!(
            compiled
                .rules
                .denies_path(&workspace.join("vault").join("token.txt"))
        );
        assert!(
            !compiled
                .rules
                .denies_path(&workspace.join("vault").join("notes.md"))
        );
    }

    #[test]
    fn user_rules_anchor_at_the_workspace() {
        let workspace = workspace();
        let compiled = compile(&[], &["secrets".to_string()], &workspace).expect("compile");
        assert_eq!(
            compiled.literal_denials,
            vec![PathBuf::from("/work/repo/secrets")]
        );
        assert_eq!(
            normalize_pattern("*.pem", &workspace),
            "**/*.pem".to_string()
        );
        assert_eq!(
            normalize_pattern("secrets", &workspace),
            "/work/repo/secrets".to_string()
        );
    }

    #[test]
    fn drive_root_rules_are_rejected() {
        let workspace = workspace();
        let error = compile(&[], &["C:/".to_string()], &workspace).expect_err("drive root");
        assert!(error.contains("drive"), "{error}");
        let error = compile(&[], &["**/*".to_string()], &workspace).expect_err("matches all");
        assert!(error.contains("matches everything"), "{error}");

        // A configured group is held to the same bounds.
        let groups = vec![DenyRuleGroup {
            globs: vec!["C:/**".to_string()],
            exceptions: Vec::new(),
            write_exceptions: Vec::new(),
        }];
        let error = compile(&groups, &[], &workspace).expect_err("drive root");
        assert!(error.contains("drive"), "{error}");

        let groups = vec![DenyRuleGroup {
            globs: vec!["**/*".to_string()],
            exceptions: Vec::new(),
            write_exceptions: Vec::new(),
        }];
        let error = compile(&groups, &[], &workspace).expect_err("matches all");
        assert!(error.contains("matches everything"), "{error}");
    }

    /// "Deny this directory, except one file inside it": a group's exception
    /// glob lifts exactly that path, and the directory itself stays traversable
    /// (a denied directory would make the walker skip the whole subtree before
    /// reaching the exception).
    #[test]
    fn a_directory_deny_can_except_one_file() {
        let rules = from_groups(&[DenyRuleGroup {
            globs: vec!["/w/secrets/**".to_string()],
            exceptions: vec!["/w/secrets/public.txt".to_string()],
            write_exceptions: Vec::new(),
        }])
        .expect("compile");

        assert!(!rules.denies_path(Path::new("/w/secrets")));
        assert!(!rules.denies_path(Path::new("/w/secrets/public.txt")));
        assert!(rules.denies_path(Path::new("/w/secrets/other.txt")));
        assert!(rules.denies_path(Path::new("/w/secrets/nested/token.txt")));
    }

    /// A *subdirectory* exception must also name the directory itself: the
    /// deny glob matches the directory, the exception glob `public/**` does
    /// not, so the walker records the directory and skips its subtree before
    /// the exception is ever reached.
    #[test]
    fn a_subdirectory_exception_must_name_the_directory_itself() {
        let rules = from_groups(&[DenyRuleGroup {
            globs: vec!["/w/secrets/**".to_string()],
            exceptions: vec!["/w/secrets/public/**".to_string()],
            write_exceptions: Vec::new(),
        }])
        .expect("compile");

        assert!(
            rules.denies_path(Path::new("/w/secrets/public")),
            "the directory itself is denied, so the walker would prune it"
        );
        assert!(!rules.denies_path(Path::new("/w/secrets/public/keep.txt")));
    }

    #[test]
    fn the_wire_form_round_trips() {
        let groups = vec![DenyRuleGroup {
            globs: vec!["**/.env".to_string()],
            exceptions: vec!["**/.env.example".to_string()],
            write_exceptions: Vec::new(),
        }];
        let rules = from_groups(&groups).expect("compile");
        assert!(rules.denies_path(Path::new("/a/.env")));
        assert!(!rules.denies_path(Path::new("/a/.env.example")));
    }
}
