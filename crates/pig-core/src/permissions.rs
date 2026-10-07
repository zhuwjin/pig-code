//! Project-level allow/deny rules: workspace root `.pigcode/permissions.toml`.
//!
//! ```toml
//! allow = ["Bash(cargo *)", "Edit(src/**)"]
//! deny  = ["Bash(git push *)"]
//! ```
//!
//! Rule syntax `Tool(pattern)`: tool names are case-insensitive; pattern is a glob matched against
//! the tool's subject (Bash = the full command string, Write/Edit = path; `\` is normalized to `/`).
//! Evaluation semantics live in the session tool loop: deny takes top priority (hard-rejected in all
//! modes including Yolo), allow skips approval but does not exempt the dangerous-command dialog.

use std::path::Path;

use pig_protocol::CoreError;

/// Fixed location of the rules file at the workspace root
pub const PERMISSIONS_FILE: &str = ".pigcode/permissions.toml";

#[derive(Debug, Clone)]
struct Rule {
    /// Tool name (lowercase-normalized; matched case-insensitively)
    tool: String,
    pattern: glob::Pattern,
    /// Raw text ("Bash(cargo *)"), for errors/hints
    raw: String,
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
        })
    }

    fn matches(&self, tool: &str, subject: &str) -> bool {
        self.tool == tool.to_ascii_lowercase() && self.pattern.matches(subject)
    }
}

/// Rule set loaded for one workspace (missing file = empty rules)
#[derive(Debug, Default, Clone)]
pub struct PermissionRules {
    allow: Vec<Rule>,
    deny: Vec<Rule>,
    /// Number of rules skipped due to syntax errors (for load-time hints)
    pub skipped: usize,
}

impl PermissionRules {
    /// Load from the workspace root: missing file -> empty rules; invalid TOML -> Err (caller hints, non-fatal)
    pub fn load(cwd: &Path) -> Result<Self, CoreError> {
        let path = cwd.join(PERMISSIONS_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => {
                return Err(CoreError::PermissionsParse {
                    detail: format!("Failed to read {}: {e}", path.display()),
                });
            }
        };
        Self::parse(&text)
    }

    /// Parse TOML text (invalid rules are skipped and counted)
    pub fn parse(text: &str) -> Result<Self, CoreError> {
        #[derive(serde::Deserialize)]
        struct File {
            #[serde(default)]
            allow: Vec<String>,
            #[serde(default)]
            deny: Vec<String>,
        }
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

    /// Subject normalization: `\` -> `/` (Windows paths/commands unified into glob-friendly forward slashes)
    fn normalize_subject(subject: &str) -> String {
        subject.replace('\\', "/")
    }

    /// deny hit -> returns the rule's raw text (for the hard-reject message)
    pub fn deny_hit(&self, tool: &str, subject: &str) -> Option<&str> {
        let subject = Self::normalize_subject(subject);
        self.deny
            .iter()
            .find(|rule| rule.matches(tool, &subject))
            .map(|rule| rule.raw.as_str())
    }

    pub fn allow_hit(&self, tool: &str, subject: &str) -> bool {
        let subject = Self::normalize_subject(subject);
        self.allow.iter().any(|rule| rule.matches(tool, &subject))
    }

    pub fn is_empty(&self) -> bool {
        self.allow.is_empty() && self.deny.is_empty()
    }
}
