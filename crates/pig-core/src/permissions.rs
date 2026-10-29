//! Allow/deny rules, loaded from two levels and unioned per session:
//! the user-level file under the data directory and the workspace root's
//! `.pigcode/permissions.toml`.
//!
//! The project level is repository-authored content. Its `allow` rules are
//! admitted only when the workspace is trusted (`[hooks] trusted_projects`, the
//! same decision that admits hooks and the project config layer); an untrusted
//! workspace contributes its `deny` rules only. That keeps a cloned repository
//! from skipping approval prompts on its own.
//!
//! ```toml
//! allow = ["Bash(cargo *)", "Edit(src/**)"]
//! deny  = ["Bash(git push *)"]
//! ```
//!
//! Rule syntax `Tool(pattern)`: tool names are case-insensitive; pattern is a glob matched against
//! the tool's subject (Bash = the full command string, Write/Edit = path; `\` is normalized to `/`).
//! Evaluation semantics live in the session tool loop: deny takes top priority across both levels
//! (hard-rejected in all modes including Full), allow skips approval but does not exempt the
//! dangerous-command dialog.

use std::path::{Path, PathBuf};

use pig_protocol::CoreError;

use crate::config::WorkspaceTrust;

/// Fixed location of the project rules file at the workspace root
pub const PERMISSIONS_FILE: &str = ".pigcode/permissions.toml";

/// File name of the user-level rules file directly under the data directory
pub const USER_PERMISSIONS_FILE: &str = "permissions.toml";

/// Which file level a rule came from (deny notes name the level)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleScope {
    Project,
    User,
}

impl RuleScope {
    /// Wording for the hard-reject note (the model reads it)
    fn label(self) -> &'static str {
        match self {
            Self::Project => "project rule",
            Self::User => "user rule",
        }
    }
}

#[derive(Debug, Clone)]
struct Rule {
    /// Tool name (lowercase-normalized; matched case-insensitively)
    tool: String,
    pattern: glob::Pattern,
    /// Raw text ("Bash(cargo *)"), for errors/hints
    raw: String,
    scope: RuleScope,
    /// Human-readable source file (project: the relative path; user: the
    /// absolute path, so the model can open it)
    source: String,
}

impl Rule {
    /// Parse "Tool(pattern)"; unbalanced parentheses/empty tool name/invalid pattern -> None
    fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        let open = raw.find('(')?;
        if !raw.ends_with(')') {
            return None;
        }
        let tool = raw[..open].trim().to_ascii_lowercase();
        if tool.is_empty() {
            return None;
        }
        let pattern = glob::Pattern::new(raw[open + 1..raw.len() - 1].trim()).ok()?;
        Some(Self {
            tool,
            pattern,
            raw: raw.to_string(),
            scope: RuleScope::Project,
            source: PERMISSIONS_FILE.to_string(),
        })
    }

    fn matches(&self, tool: &str, subject: &str) -> bool {
        self.tool == tool.to_ascii_lowercase() && self.pattern.matches(subject)
    }
}

/// A deny hit: what blocked the call, and which file level wrote the rule
#[derive(Debug, Clone, Copy)]
pub struct DenyHit<'a> {
    /// Raw rule text ("Bash(git push *)")
    pub rule: &'a str,
    /// Project or user level (the note wording differs)
    pub scope: RuleScope,
    /// Human-readable source file
    pub source: &'a str,
}

impl DenyHit<'_> {
    /// Model-facing one-line explanation
    pub fn note(&self) -> String {
        format!(
            "Blocked by a {}: {} ({})",
            self.scope.label(),
            self.rule,
            self.source
        )
    }
}

/// TOML shape of one rules file
#[derive(Debug, Default, serde::Deserialize)]
struct File {
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    deny: Vec<String>,
}

/// Rule set loaded for one workspace: the user level unioned with the project
/// level (missing files = empty rules)
#[derive(Debug, Default, Clone)]
pub struct PermissionRules {
    allow: Vec<Rule>,
    deny: Vec<Rule>,
    /// Number of rules skipped due to syntax errors (for load-time hints)
    pub skipped: usize,
    /// Whether this workspace is trusted, i.e. whether a project `allow` rule
    /// written now will be honored by the next session. An untrusted
    /// `ApprovedForProject` must not promise a durable rule that the next open
    /// silently ignores.
    project_trusted: bool,
}

impl PermissionRules {
    /// Load the project level from the workspace root: missing file -> empty
    /// rules; invalid TOML -> Err (caller hints, non-fatal).
    ///
    /// Fail-closed by default: project `allow` rules are repository-authored
    /// and only admitted by an explicit workspace-trust decision, so this
    /// convenience loader keeps the project's **deny** rules and drops its
    /// allows. Runtime sessions use [`Self::load_layered_with_trust`] (or the
    /// safe-by-default [`Self::load_layered`]) so trust is always an explicit
    /// input.
    pub fn load(cwd: &Path) -> Result<Self, CoreError> {
        Self::load_project(cwd, /*trusted*/ false)
    }

    fn load_project(cwd: &Path, trusted: bool) -> Result<Self, CoreError> {
        let path = cwd.join(PERMISSIONS_FILE);
        let text = match read_text(&path)? {
            Some(text) => text,
            // A trusted workspace with no project file yet can still save a
            // rule later (ApprovedForProject), so the admission flag follows
            // the trust decision, not the file's current presence.
            None => {
                return Ok(Self {
                    project_trusted: trusted,
                    ..Self::default()
                });
            }
        };
        let mut rules = Self::parse(&text)?;
        rules.project_trusted = trusted;
        if !trusted {
            // A repository-authored allow rule must never skip an approval.
            // Denials stay: they only restrict, so loading them from an
            // untrusted workspace is the safe direction.
            rules.allow.clear();
        }
        Ok(rules)
    }

    /// Load the user level from `{data_dir}/permissions.toml` (same syntax,
    /// same tolerance); rules are tagged so a deny note can name the file
    pub fn load_user(data_dir: &Path) -> Result<Self, CoreError> {
        let path = user_file(data_dir);
        let text = match read_text(&path)? {
            Some(text) => text,
            None => return Ok(Self::default()),
        };
        let mut rules = Self::parse(&text)?;
        for rule in rules.allow.iter_mut().chain(rules.deny.iter_mut()) {
            rule.scope = RuleScope::User;
            rule.source = path.display().to_string();
        }
        Ok(rules)
    }

    /// Load both levels with no workspace trust: the user level in full, plus
    /// the project level's **deny** rules only. Existing call sites get the
    /// fail-closed behavior; a trust-aware caller passes
    /// [`WorkspaceTrust`] through [`Self::load_layered_with_trust`].
    pub fn load_layered(cwd: &Path, data_dir: &Path) -> (Self, Vec<CoreError>) {
        Self::load_layered_with_trust(cwd, data_dir, &WorkspaceTrust::untrusted(cwd))
    }

    /// Load both levels under one workspace-trust decision, user first then
    /// project: the effective set is the union, and a deny from either level
    /// hard-rejects. A trusted workspace also honors project `allow` rules;
    /// an untrusted one loads only its denials. A level whose file fails to
    /// parse is reported to the caller (hint, non-fatal) while the other level
    /// stays in effect.
    pub fn load_layered_with_trust(
        cwd: &Path,
        data_dir: &Path,
        trust: &WorkspaceTrust,
    ) -> (Self, Vec<CoreError>) {
        let trusted = trust.admits(cwd);
        let mut rules = Self::default();
        let mut errors = Vec::new();
        for loaded in [Self::load_user(data_dir), Self::load_project(cwd, trusted)] {
            match loaded {
                Ok(level) => rules.extend(level),
                Err(error) => errors.push(error),
            }
        }
        rules.project_trusted = trusted;
        (rules, errors)
    }

    /// Parse TOML text (invalid rules are skipped and counted)
    pub fn parse(text: &str) -> Result<Self, CoreError> {
        let parsed: File = toml::from_str(text).map_err(|e| CoreError::PermissionsParse {
            detail: e.to_string(),
        })?;
        let mut rules = Self::default();
        let mut load_list = |list: Vec<String>, into_deny: bool| {
            for raw in list {
                match Rule::parse(&raw) {
                    Some(rule) => {
                        if into_deny {
                            rules.deny.push(rule);
                        } else {
                            rules.allow.push(rule);
                        }
                    }
                    None => rules.skipped += 1,
                }
            }
        };
        load_list(parsed.allow, false);
        load_list(parsed.deny, true);
        Ok(rules)
    }

    /// Union another level's rules into this set (deny rules accumulate, so a
    /// deny from either level wins)
    pub fn extend(&mut self, other: PermissionRules) {
        self.allow.extend(other.allow);
        self.deny.extend(other.deny);
        self.skipped += other.skipped;
        self.project_trusted |= other.project_trusted;
    }

    /// Whether project `allow` rules are admitted for this workspace. Only a
    /// trusted workspace honors them on the next open, so only a trusted
    /// workspace may offer/persist the durable `ApprovedForProject` rule.
    pub fn project_allows_are_trusted(&self) -> bool {
        self.project_trusted
    }

    /// Add one allow rule to the live set (an approved "remember to project
    /// rules" decision, without a reload). Invalid text is ignored: callers
    /// validate through `approval_rule_texts` first.
    pub fn add_allow(&mut self, raw: &str) {
        if let Some(rule) = Rule::parse(raw) {
            self.allow.push(rule);
        }
    }

    /// Subject normalization: `\` -> `/` (Windows paths/commands unified into glob-friendly forward slashes)
    fn normalize_subject(subject: &str) -> String {
        subject.replace('\\', "/")
    }

    /// deny hit (either level) -> the rule, its level and its file (for the hard-reject note)
    pub fn deny_hit(&self, tool: &str, subject: &str) -> Option<DenyHit<'_>> {
        let subject = Self::normalize_subject(subject);
        self.deny
            .iter()
            .find(|rule| rule.matches(tool, &subject))
            .map(|rule| DenyHit {
                rule: rule.raw.as_str(),
                scope: rule.scope,
                source: rule.source.as_str(),
            })
    }

    pub fn allow_hit(&self, tool: &str, subject: &str) -> bool {
        let subject = Self::normalize_subject(subject);
        self.allow.iter().any(|rule| rule.matches(tool, &subject))
    }

    pub fn is_empty(&self) -> bool {
        self.allow.is_empty() && self.deny.is_empty()
    }
}

/// User-level rules file for a data directory
pub fn user_file(data_dir: &Path) -> PathBuf {
    data_dir.join(USER_PERMISSIONS_FILE)
}

/// Command prefixes that must never become a standing allow rule: each one can
/// run an arbitrary command with arguments the rule says nothing about. This
/// mirrors Codex's banned-prefix guardrail.
const BANNED_PREFIXES: &[&str] = &[
    "sh",
    "bash",
    "zsh",
    "pwsh",
    "powershell",
    "cmd",
    "env",
    "sudo",
    "su",
    "doas",
    "xargs",
    "eval",
    "exec",
    "source",
    "rm",
    "del",
    "shred",
];

/// Conservative rule text(s) for an ordinary approval ("remember to project
/// rules"). Returns `Err(reason)` when no rule would be tight enough — the
/// caller then treats the decision as a plain one-shot Allow and tells the
/// model why.
///
/// Bash: the executable basename, extended to `<exe> <subcommand>` when the
/// second token is pure-alphabetic (`git status`, `cargo build`, …). Two rules
/// are emitted — the bare form and the arguments form — so a later plain
/// `git status` hits while token gluing (`git statusx`) cannot: the glob is
/// never `<prefix>*` on its own.
/// Write/Edit: the exact subject path (one file, never its directory).
pub fn approval_rule_texts(tool: &str, subject: &str) -> Result<Vec<String>, String> {
    let subject = PermissionRules::normalize_subject(subject);
    let texts = match tool {
        crate::tool::EXEC_COMMAND_TOOL => bash_rule_texts(&subject)?,
        "Write" | "Edit" => vec![format!("{tool}({subject})")],
        other => return Err(format!("`{other}` has no conservative rule form")),
    };
    for text in &texts {
        if Rule::parse(text).is_none() {
            return Err(format!(
                "the extracted pattern `{text}` is not a valid rule"
            ));
        }
    }
    Ok(texts)
}

fn bash_rule_texts(command: &str) -> Result<Vec<String>, String> {
    let mut tokens = command.split_whitespace();
    let Some(first) = tokens.next() else {
        return Err("the command is empty".to_string());
    };
    // `FOO=1 make` hides the real executable behind an assignment; do not guess.
    if first.contains('=') {
        return Err("the command starts with an environment assignment".to_string());
    }
    let exe = first
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(first)
        .trim_matches(['"', '\'']);
    if exe.is_empty()
        || !exe
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
    {
        return Err(format!("`{exe}` is not a plain executable name"));
    }
    if BANNED_PREFIXES
        .iter()
        .any(|banned| banned.eq_ignore_ascii_case(exe))
    {
        return Err(format!(
            "`{exe}` can run arbitrary commands, so it cannot be allowed as a prefix"
        ));
    }
    let subcommand = tokens
        .next()
        .filter(|token| !token.is_empty() && token.chars().all(|c| c.is_ascii_alphabetic()));
    let prefix = match subcommand {
        Some(sub) => format!("{exe} {sub}"),
        None => exe.to_string(),
    };
    Ok(vec![
        format!("{}({prefix})", crate::tool::EXEC_COMMAND_TOOL),
        format!("{}({prefix} *)", crate::tool::EXEC_COMMAND_TOOL),
    ])
}

/// Append allow rules to the workspace's `.pigcode/permissions.toml`, creating
/// the directory and file when missing. Everything else in the file is kept
/// (other keys, comments, formatting); rules already present are not
/// duplicated.
///
/// Err carries a model-facing explanation and means **nothing was written** —
/// the caller reports it and keeps the approval one-shot. A malformed file is
/// never overwritten, because rewriting it would destroy the user's intent
/// (and comments plus formatting along with it).
pub fn append_project_allow(cwd: &Path, rules: &[String]) -> Result<(), String> {
    if rules.is_empty() {
        return Ok(());
    }
    let path = cwd.join(PERMISSIONS_FILE);
    // The rules file is repository content, so a cloned `.pigcode` directory or
    // `permissions.toml` may itself be a symlink authored by whoever wrote the
    // repository. Reading and replacing through it would operate on the link
    // target (an arbitrary file the user owns) instead of a workspace file.
    // Refuse both shapes; the atomic write below also replaces a final entry
    // rather than writing through it.
    let project_dir = cwd.join(".pigcode");
    match std::fs::symlink_metadata(&project_dir) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(format!(
                "{} is a symbolic link; refusing to write project permission rules outside \
                 the workspace",
                project_dir.display()
            ));
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(format!(
                "{} is not a directory; the project permission rules cannot be saved",
                project_dir.display()
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(&project_dir)
                .map_err(|error| format!("failed to create {}: {error}", project_dir.display()))?;
        }
        Err(error) => {
            return Err(format!(
                "failed to inspect {}: {error}",
                project_dir.display()
            ));
        }
    }
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(format!(
                "{} is a symbolic link; refusing to replace it with project permission rules",
                path.display()
            ));
        }
        Ok(metadata) if !metadata.is_file() => {
            return Err(format!(
                "{} is not a regular file; the project permission rules cannot be saved",
                path.display()
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!("failed to inspect {}: {error}", path.display()));
        }
    }
    let text = match read_text(&path) {
        Ok(Some(text)) => text,
        Ok(None) => String::new(),
        Err(error) => return Err(format!("{error:?}")),
    };
    let parsed: toml::Value = if text.trim().is_empty() {
        toml::Value::Table(toml::Table::new())
    } else {
        match toml::from_str(&text) {
            Ok(value) => value,
            Err(error) => {
                return Err(format!(
                    "{} is not valid TOML ({error}); fix or remove it first",
                    path.display()
                ));
            }
        }
    };
    let existing = allow_list(&parsed)
        .map_err(|detail| format!("{}: {detail}; fix or remove the file first", path.display()))?;
    let missing: Vec<&str> = rules
        .iter()
        .map(String::as_str)
        .filter(|rule| !existing.iter().any(|have| have == rule))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    let inserted = missing
        .iter()
        .map(|rule| toml_string(rule))
        .collect::<Vec<_>>()
        .join(", ");

    // Splice into the existing `allow` array when its opening bracket can be
    // located; otherwise add a fresh top-level key after the leading comment
    // block. Both candidates are verified by re-parsing before anything is
    // written, so a mis-located splice can never corrupt the file.
    let mut updated = None;
    if let Some(position) = allow_bracket_position(&text) {
        let mut candidate = text.clone();
        candidate.insert_str(position, &format!("{inserted}, "));
        if verify_update(&parsed, &candidate, &existing, &missing) {
            updated = Some(candidate);
        }
    }
    if updated.is_none() {
        let candidate = insert_allow_line(&text, &inserted);
        if verify_update(&parsed, &candidate, &existing, &missing) {
            updated = Some(candidate);
        }
    }
    let Some(updated) = updated else {
        return Err(format!(
            "the allow array in {} could not be updated without rewriting the file",
            path.display()
        ));
    };
    // Atomic same-directory replacement: a crash cannot truncate the rules
    // file, and the rename replaces the validated entry instead of reopening it.
    crate::config::write_private_file(&path, updated.as_bytes())
        .map_err(|error| format!("failed to write {}: {error:?}", path.display()))
}

fn read_text(path: &Path) -> Result<Option<String>, CoreError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(CoreError::PermissionsParse {
            detail: format!("Failed to read {}: {e}", path.display()),
        }),
    }
}

/// The `allow` array of a parsed file; Err when the key is present but is not
/// an array of strings (the writer must not guess at its shape)
fn allow_list(value: &toml::Value) -> Result<Vec<String>, String> {
    let Some(allow) = value.get("allow") else {
        return Ok(Vec::new());
    };
    let Some(items) = allow.as_array() else {
        return Err("`allow` is not an array of strings".to_string());
    };
    items
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_string)
                .ok_or_else(|| "`allow` contains a non-string entry".to_string())
        })
        .collect()
}

/// Byte offset just past the `[` that opens a top-level `allow = [` value
/// (top-level keys precede every table header in TOML, so the first matching
/// assignment is the top-level one)
fn allow_bracket_position(text: &str) -> Option<usize> {
    let mut offset = 0usize;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            // First table header: no top-level key can follow it
            return None;
        }
        if let Some(rest) = trimmed.strip_prefix("allow") {
            let after = rest.trim_start();
            if let Some(after_eq) = after.strip_prefix('=') {
                let value_start = offset + (line.len() - after_eq.len());
                // The value may open on a later line; only whitespace and
                // comments may sit in between.
                let mut chars = text[value_start..].char_indices().peekable();
                while let Some((ix, ch)) = chars.next() {
                    if ch.is_whitespace() {
                        continue;
                    }
                    if ch == '#' {
                        for (_, c) in chars.by_ref() {
                            if c == '\n' {
                                break;
                            }
                        }
                        continue;
                    }
                    return (ch == '[').then_some(value_start + ix + 1);
                }
                return None;
            }
        }
        offset += line.len();
    }
    None
}

/// Insert a fresh `allow = [...]` line after the leading comment/blank block
/// (top-level by construction: comments and blank lines carry no table scope)
fn insert_allow_line(text: &str, inserted: &str) -> String {
    let line = format!("allow = [{inserted}]\n");
    let mut offset = 0usize;
    for line_text in text.split_inclusive('\n') {
        let trimmed = line_text.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            offset += line_text.len();
        } else {
            break;
        }
    }
    let mut candidate = text.to_string();
    candidate.insert_str(offset, &line);
    candidate
}

/// Re-parse a candidate: every key except `allow` must be unchanged and
/// `allow` must be exactly the old list plus the new rules
fn verify_update(
    before: &toml::Value,
    candidate: &str,
    existing: &[String],
    missing: &[&str],
) -> bool {
    let Ok(after) = toml::from_str::<toml::Value>(candidate) else {
        return false;
    };
    let Ok(updated) = allow_list(&after) else {
        return false;
    };
    // A splice inserts at the array head, a fallback appends the key, so the
    // order is not fixed; the rule *set* is what must match (old rules plus the
    // new ones, nothing lost)
    let mut expected: Vec<String> = existing
        .iter()
        .cloned()
        .chain(missing.iter().map(|rule| (*rule).to_string()))
        .collect();
    let mut updated = updated;
    expected.sort();
    updated.sort();
    if updated != expected {
        return false;
    }
    let strip = |value: &toml::Value| {
        let mut value = value.clone();
        if let Some(table) = value.as_table_mut() {
            table.remove("allow");
        }
        value
    };
    strip(before) == strip(&after)
}

/// TOML basic-string literal (paths may carry backslashes on Windows)
fn toml_string(raw: &str) -> String {
    let mut escaped = String::with_capacity(raw.len() + 2);
    escaped.push('"');
    for ch in raw.chars() {
        match ch {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\t' => escaped.push_str("\\t"),
            '\r' => escaped.push_str("\\r"),
            other => escaped.push(other),
        }
    }
    escaped.push('"');
    escaped
}
