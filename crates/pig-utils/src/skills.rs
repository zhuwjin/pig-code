//! Skills: discovery of `<skill directory>/SKILL.md`, frontmatter parsing,
//! enable/disable state, prompt listing, and body loading. Semantics aligned
//! with ZCode: the system prompt injects only a "name + description" listing
//! (with a budget; over budget it falls back to names only), and the body is
//! loaded on demand by the Skill tool; a project-level skill with the same
//! name overrides the user-level one. The directory layout is the same
//! two-layer scheme as agent profiles: `{data_dir}/skills/` (user level) +
//! `{cwd}/.pigcode/skills/` (project level).

use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use pig_protocol::CoreError;

/// Skill file name (a directory must contain it to count as a skill)
pub const SKILL_FILE: &str = "SKILL.md";
/// Character budget for the prompt skill listing: beyond it, fall back to names+paths only (same order of magnitude as ZCode)
const SECTION_BUDGET: usize = 20 * 1024;
/// Truncation length for a single listing entry's description (after description + when_to_use are joined)
const LISTING_DESC_CHARS: usize = 250;
/// Byte cap for the Skill tool reading a body
pub const MAX_SKILL_BYTES: u64 = 100 * 1024;
/// Body replacement placeholders: the base directory for relative paths in the skill body (CLAUDE skill ecosystem compatibility)
const DIR_PLACEHOLDERS: [&str; 2] = ["${SKILL_DIR}", "${CLAUDE_SKILL_DIR}"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkillSource {
    /// User level: `{data_dir}/skills/`
    User,
    /// Project level: `{cwd}/.pigcode/skills/`
    Project,
}

/// Display and invocation info for one skill (the body is not held in memory; the Skill tool reads it on demand)
#[derive(Clone, Debug)]
pub struct Skill {
    /// Invocation name: frontmatter name (falls back to the directory name by default)
    pub name: String,
    pub description: String,
    /// Optional applicability note (frontmatter when_to_use, joined into the listing)
    pub when_to_use: Option<String>,
    /// Absolute path of SKILL.md (key for the enable/disable state)
    pub path: PathBuf,
    /// Skill directory (base for relative paths in the body)
    pub directory: PathBuf,
    pub source: SkillSource,
    /// Whether SKILL.md has frontmatter (without it name=directory name and description is empty)
    pub has_frontmatter: bool,
}

pub fn user_root(data_dir: &Path) -> PathBuf {
    data_dir.join("skills")
}

pub fn project_root(cwd: &Path) -> PathBuf {
    cwd.join(".pigcode").join("skills")
}

/// Scan one skill root: the root's own SKILL.md + SKILL.md in one level of
/// subdirectories (same depth as the ZCode agent side; deeper grouping
/// directories are unsupported). A missing directory = empty. Entries that
/// fail to read/parse are skipped.
pub fn discover_root(root: &Path, source: SkillSource) -> Vec<Skill> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if root.join(SKILL_FILE).is_file() {
        dirs.push(root.to_path_buf());
    }
    if let Ok(entries) = std::fs::read_dir(root) {
        let mut children: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        children.sort();
        dirs.extend(children);
    }
    let mut skills = Vec::new();
    for dir in dirs {
        let path = dir.join(SKILL_FILE);
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue; // Skip on read failure (encoding/permissions)
        };
        let parsed = parse_skill_markdown(&content);
        let name = parsed
            .frontmatter
            .as_ref()
            .and_then(|fm| fm.name.clone())
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| {
                dir.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "skill".to_string())
            });
        if name.trim().is_empty() {
            continue;
        }
        skills.push(Skill {
            name,
            description: parsed
                .frontmatter
                .as_ref()
                .and_then(|fm| fm.description.clone())
                .unwrap_or_default(),
            when_to_use: parsed
                .frontmatter
                .as_ref()
                .and_then(|fm| fm.when_to_use.clone()),
            path,
            directory: dir,
            source,
            has_frontmatter: parsed.frontmatter.is_some(),
        });
    }
    skills
}

/// Two-layer discovery: user level as the base, project-level same-name
/// entries override wholesale (same policy as agent profiles), sorted by name.
/// Does not apply the enable/disable state (the UI wants to show disabled
/// entries; the caller filters itself)
pub fn discover(cwd: &Path, data_dir: &Path) -> Vec<Skill> {
    let user = discover_root(&user_root(data_dir), SkillSource::User);
    let project = discover_root(&project_root(cwd), SkillSource::Project);
    let mut by_name: HashMap<String, Skill> = HashMap::new();
    for skill in user.into_iter().chain(project) {
        by_name.insert(skill.name.clone(), skill);
    }
    let mut skills: Vec<Skill> = by_name.into_values().collect();
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

/// Available listing after applying the enable/disable state (for prompt injection / Skill tool lookup)
pub fn discover_enabled(cwd: &Path, data_dir: &Path) -> Vec<Skill> {
    let disabled = load_disabled_map(data_dir);
    discover(cwd, data_dir)
        .into_iter()
        .filter(|skill| !disabled.contains(&normalize_key(&skill.path)))
        .collect()
}

// ---------- SKILL.md parsing / serialization ----------

/// Recognized frontmatter fields + the remaining key-value pairs (preserved faithfully when written back)
#[derive(Clone, Debug, Default)]
pub struct SkillFrontmatter {
    pub name: Option<String>,
    pub description: Option<String>,
    pub when_to_use: Option<String>,
    /// Unrecognized key-value pairs (license/metadata etc.), kept in original order
    pub extra: Vec<(String, String)>,
}

/// Parse result: frontmatter (None if absent) + body
#[derive(Clone, Debug)]
pub struct ParsedSkill {
    pub frontmatter: Option<SkillFrontmatter>,
    pub body: String,
}

/// Parse SKILL.md: frontmatter delimited by `---` + body. Hand-written
/// line-by-line parsing (no serde_yaml dependency, same approach as agent
/// profiles): `key: value` scalars, quote stripping, `>`/`|` block scalars,
/// `#` comments and blank lines skipped; missing or unclosed frontmatter is
/// leniently treated as "no frontmatter".
pub fn parse_skill_markdown(content: &str) -> ParsedSkill {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let lines: Vec<&str> = content.lines().collect();
    if lines.first().map(|line| line.trim()) != Some("---") {
        return ParsedSkill {
            frontmatter: None,
            body: content.trim().to_string(),
        };
    }
    let mut fm = SkillFrontmatter::default();
    let mut i = 1;
    let mut closed = false;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        if trimmed == "---" {
            closed = true;
            i += 1;
            break;
        }
        if trimmed.is_empty() || trimmed.starts_with('#') {
            i += 1;
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            i += 1; // Leniently skip unrecognized lines
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        // Block scalar: > folded / | literal (including >- |- variants);
        // subsequent indented lines all belong to the value
        if let Some(style) = block_style(value) {
            let (text, consumed) = read_block_scalar(&lines, i + 1, style);
            assign(&mut fm, key, &text, value.to_string());
            i += 1 + consumed;
            continue;
        }
        assign(&mut fm, key, strip_quotes(value), String::new());
        i += 1;
    }
    if !closed {
        return ParsedSkill {
            frontmatter: None,
            body: content.trim().to_string(),
        };
    }
    ParsedSkill {
        frontmatter: Some(fm),
        body: lines[i.min(lines.len())..].join("\n").trim().to_string(),
    }
}

/// If the value starts a block scalar, return its folding style
fn block_style(value: &str) -> Option<bool> {
    let fold = value.starts_with('>');
    if fold || value.starts_with('|') {
        // Variants like > | >- |- >2 are all treated as the basic style
        // (indentation indicator digits ignored)
        Some(fold)
    } else {
        None
    }
}

/// Read the block scalar body from start: indented lines (blank lines
/// included) until the first non-indented non-blank line. Returns (text, lines
/// consumed); the folded style joins with spaces, the literal style keeps
/// newlines.
fn read_block_scalar(lines: &[&str], start: usize, fold: bool) -> (String, usize) {
    let mut body: Vec<String> = Vec::new();
    let mut i = start;
    while i < lines.len() {
        let raw = lines[i];
        if raw.trim().is_empty() {
            body.push(String::new());
            i += 1;
            continue;
        }
        if !raw.starts_with(' ') && !raw.starts_with('\t') {
            break;
        }
        body.push(raw.trim_start().to_string());
        i += 1;
    }
    // Trailing blank lines belong to block-end newline control; trimmed from the text
    while body.last().is_some_and(|line| line.is_empty()) {
        body.pop();
        i -= 1;
    }
    let text = if fold {
        body.join(" ")
    } else {
        body.join("\n")
    };
    (text, i - start)
}

fn assign(fm: &mut SkillFrontmatter, key: &str, value: &str, _raw: String) {
    let value = value.trim();
    let owned = (!value.is_empty()).then(|| value.to_string());
    match key {
        "name" => fm.name = owned,
        "description" => fm.description = owned,
        "when_to_use" | "whenToUse" => fm.when_to_use = owned,
        _ => fm.extra.push((key.to_string(), value.trim().to_string())),
    }
}

/// Strip a matched pair of quotes around the value ("..." or '...'; same as agent profiles)
fn strip_quotes(value: &str) -> &str {
    let bytes = value.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

/// Render SKILL.md: the three known fields + faithfully preserved extra keys +
/// body. Values containing newlines/special leading characters are emitted as
/// `|-` block scalars (supported the same way by the parser), guaranteeing
/// round-trip.
pub fn render_skill_markdown(
    name: &str,
    description: &str,
    when_to_use: Option<&str>,
    extra: &[(String, String)],
    body: &str,
) -> String {
    let scalar = |value: &str| {
        if value.contains('\n') || value.starts_with(['"', '\'', '>', '|', '#', '&', '*']) {
            // Block scalar (literal): the parser collects by indentation
            let indented: String = value
                .lines()
                .map(|line| format!("  {}\n", line))
                .collect::<String>();
            format!("|-\n{indented}")
        } else {
            value.to_string()
        }
    };
    let mut out = String::from("---\n");
    out.push_str(&format!("name: {}\n", scalar(name)));
    out.push_str(&format!("description: {}\n", scalar(description)));
    if let Some(when) = when_to_use.filter(|w| !w.trim().is_empty()) {
        out.push_str(&format!("when_to_use: {}\n", scalar(when)));
    }
    for (key, value) in extra {
        if matches!(
            key.as_str(),
            "name" | "description" | "when_to_use" | "whenToUse"
        ) {
            continue;
        }
        out.push_str(&format!("{key}: {}\n", scalar(value)));
    }
    out.push_str("---\n\n");
    out.push_str(body.trim());
    out.push('\n');
    out
}

// ---------- Enable/disable state (skills-state.json) ----------

/// `{data_dir}/skills-state.json`: stores only false entries (enabled by
/// default); the key is the SKILL.md absolute path (backslashes normalized to
/// `/`, same as ZCode)
#[derive(Default, Deserialize)]
struct SkillsState {
    #[serde(default)]
    skills: HashMap<String, SkillEnable>,
}

#[derive(Default, Deserialize)]
struct SkillEnable {
    #[serde(default)]
    enable: bool,
}

pub fn state_path(data_dir: &Path) -> PathBuf {
    data_dir.join("skills-state.json")
}

fn normalize_key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Disabled set: key = normalized SKILL.md path. Missing/invalid file = empty (not fatal)
pub fn load_disabled_map(data_dir: &Path) -> std::collections::HashSet<String> {
    let Ok(raw) = std::fs::read_to_string(state_path(data_dir)) else {
        return Default::default();
    };
    serde_json::from_str::<SkillsState>(&raw)
        .unwrap_or_default()
        .skills
        .into_iter()
        .filter(|(_, entry)| !entry.enable)
        .map(|(key, _)| key)
        .collect()
}

/// Enable/disable a skill: enabled=true removes the entry (back to default);
/// false writes it. Read-modify-write of the whole file (same as
/// agents-state)
pub fn set_skill_enabled(
    data_dir: &Path,
    skill_md_path: &Path,
    enabled: bool,
) -> Result<(), CoreError> {
    let path = state_path(data_dir);
    let mut raw: serde_json::Map<String, serde_json::Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|v| v.get("skills").and_then(|s| s.as_object()).cloned())
        .unwrap_or_default();
    let key = normalize_key(skill_md_path);
    if enabled {
        raw.remove(&key);
    } else {
        raw.insert(key, serde_json::json!({ "enable": false }));
    }
    let file = serde_json::json!({ "skills": raw });
    std::fs::create_dir_all(data_dir).map_err(|e| CoreError::DataDirCreate {
        path: data_dir.display().to_string(),
        detail: e.to_string(),
    })?;
    let text =
        serde_json::to_string_pretty(&file).map_err(|e| CoreError::SkillsStateSerialize {
            detail: e.to_string(),
        })?;
    std::fs::write(&path, text).map_err(|e| CoreError::SkillsStateWrite {
        detail: e.to_string(),
    })
}

// ---------- Prompt listing and body loading ----------

/// Skill listing in the system prompt: name + description (when_to_use joined,
/// truncated to 250 chars) + file path; if the total exceeds the budget it
/// falls back to names+paths only. Returns an empty string when no skills are
/// available
pub fn skills_section(cwd: &Path, data_dir: &Path) -> String {
    let skills = discover_enabled(cwd, data_dir);
    if skills.is_empty() {
        return String::new();
    }
    let entry = |skill: &Skill| {
        let mut desc = skill.description.trim().to_string();
        if let Some(when) = skill
            .when_to_use
            .as_deref()
            .filter(|w| !w.trim().is_empty())
        {
            if !desc.is_empty() {
                desc.push_str(" - ");
            }
            desc.push_str(when.trim());
        }
        if desc.chars().count() > LISTING_DESC_CHARS {
            desc = format!(
                "{}...",
                desc.chars().take(LISTING_DESC_CHARS).collect::<String>()
            );
        }
        let path = skill.path.display().to_string();
        if desc.is_empty() {
            format!("- {} (file: {path})", skill.name)
        } else {
            format!("- {}: {desc} (file: {path})", skill.name)
        }
    };
    let full: Vec<String> = skills.iter().map(entry).collect();
    let header = "## Available skills\n\
                  The following skills can be loaded on demand via the Skill tool for their \
                  full instructions; when a task matches a skill, call Skill to load it before \
                  continuing — do not invent skill names or content from memory:\n";
    if header.len() + full.iter().map(|l| l.len() + 1).sum::<usize>() <= SECTION_BUDGET {
        return format!("{header}{}\n", full.join("\n"));
    }
    // Fallback: keep names + paths only
    let names: Vec<String> = skills
        .iter()
        .map(|skill| format!("- {} (file: {})", skill.name, skill.path.display()))
        .collect();
    format!("{header}{}\n", names.join("\n"))
}

/// Look up by name (discover already deduplicated, so match name directly; None when not found)
pub fn find_skill<'a>(skills: &'a [Skill], name: &str) -> Option<&'a Skill> {
    skills.iter().find(|skill| skill.name == name)
}

/// Body loading for the Skill tool: look up by name → read the file (size
/// guard) → strip frontmatter → replace directory placeholders → wrap in
/// `<skill_content>`. Not found/read failure returns Err (the error text goes
/// straight into the tool result)
pub fn load_skill_output(cwd: &Path, data_dir: &Path, name: &str) -> Result<String, String> {
    let skills = discover_enabled(cwd, data_dir);
    let skill = find_skill(&skills, name).ok_or_else(|| {
        let available: Vec<String> = skills.iter().map(|s| s.name.clone()).collect();
        if available.is_empty() {
            format!(
                "Skill \"{name}\" does not exist (no skills are currently available); do not \
                 invent skill names"
            )
        } else {
            format!(
                "Skill \"{name}\" does not exist. Available skills: {}; only invoke skills from \
                 the listing",
                available.join(", ")
            )
        }
    })?;
    let len = std::fs::metadata(&skill.path)
        .map_err(|e| format!("Failed to read {}: {e}", skill.path.display()))?
        .len();
    if len > MAX_SKILL_BYTES {
        return Err(format!(
            "Skill file too large ({} KB, over the {} KB limit)",
            len / 1024,
            MAX_SKILL_BYTES / 1024
        ));
    }
    let content = std::fs::read_to_string(&skill.path)
        .map_err(|e| format!("Failed to read {}: {e}", skill.path.display()))?;
    let parsed = parse_skill_markdown(&content);
    let dir = skill.directory.display().to_string();
    let mut body = parsed.body;
    for placeholder in DIR_PLACEHOLDERS {
        body = body.replace(placeholder, &dir);
    }
    Ok(format!(
        "<skill_content name=\"{}\">\n\n# Skill: {}\n\n{}\n\nSkill directory: {}\nRelative paths in the body resolve against this directory.\n</skill_content>",
        skill.name,
        skill.name,
        body.trim(),
        dir
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("pig-skills-test-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write_skill(root: &Path, dir_name: &str, markdown: &str) {
        let dir = root.join(dir_name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(SKILL_FILE), markdown).unwrap();
    }

    #[test]
    fn parse_frontmatter_scalars() {
        let parsed = parse_skill_markdown(
            "---\nname: pdf\ndescription: PDF toolkit\nwhen_to_use: when generating PDFs\n---\n\nBody text\n",
        );
        let fm = parsed.frontmatter.expect("frontmatter should parse");
        assert_eq!(fm.name.as_deref(), Some("pdf"));
        assert_eq!(fm.description.as_deref(), Some("PDF toolkit"));
        assert_eq!(fm.when_to_use.as_deref(), Some("when generating PDFs"));
        assert_eq!(parsed.body, "Body text");
    }

    #[test]
    fn parse_quoted_and_multiline_description() {
        let parsed =
            parse_skill_markdown("---\nname: x\ndescription: \"desc with: a colon\"\n---\nbody");
        let fm = parsed.frontmatter.unwrap();
        assert_eq!(fm.description.as_deref(), Some("desc with: a colon"));

        let folded = parse_skill_markdown(
            "---\nname: x\ndescription: >\n  first line\n  second line\n---\nbody",
        );
        assert_eq!(
            folded.frontmatter.unwrap().description.as_deref(),
            Some("first line second line")
        );

        let literal = parse_skill_markdown(
            "---\nname: x\ndescription: |-\n  first line\n  second line\n---\nbody",
        );
        assert_eq!(
            literal.frontmatter.unwrap().description.as_deref(),
            Some("first line\nsecond line")
        );
    }

    #[test]
    fn parse_without_frontmatter_falls_back() {
        let parsed = parse_skill_markdown("body only, no frontmatter");
        assert!(parsed.frontmatter.is_none());
        assert_eq!(parsed.body, "body only, no frontmatter");
    }

    #[test]
    fn render_round_trip_with_extras() {
        let extra = vec![("license".to_string(), "MIT".to_string())];
        let markdown = render_skill_markdown(
            "demo",
            "Demo skill\nsecond line note",
            Some("when doing the demo"),
            &extra,
            "body content",
        );
        assert!(markdown.starts_with("---\n"));
        let parsed = parse_skill_markdown(&markdown);
        let fm = parsed
            .frontmatter
            .expect("round-trip should have frontmatter");
        assert_eq!(fm.name.as_deref(), Some("demo"));
        assert_eq!(
            fm.description.as_deref(),
            Some("Demo skill\nsecond line note")
        );
        assert_eq!(fm.when_to_use.as_deref(), Some("when doing the demo"));
        assert_eq!(fm.extra, vec![("license".to_string(), "MIT".to_string())]);
        assert_eq!(parsed.body, "body content");
    }

    #[test]
    fn discover_scopes_and_name_fallback() {
        let tmp = TempDir::new("discover");
        let data = tmp.0.join("data");
        let ws = tmp.0.join("ws");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&ws).unwrap();
        write_skill(
            &user_root(&data),
            "pdf",
            "---\nname: pdf\ndescription: user-level PDF\n---\nuser-level body",
        );
        write_skill(&user_root(&data), "plain", "skill body without frontmatter");
        write_skill(
            &project_root(&ws),
            "pdf",
            "---\nname: pdf\ndescription: project-level PDF\n---\nproject-level body",
        );
        let skills = discover(&ws, &data);
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["pdf", "plain"]);
        let pdf = find_skill(&skills, "pdf").unwrap();
        assert_eq!(pdf.description, "project-level PDF");
        assert_eq!(pdf.source, SkillSource::Project);
        // No frontmatter: name falls back to the directory name and description is empty
        let plain = find_skill(&skills, "plain").unwrap();
        assert_eq!(plain.name, "plain");
        assert!(plain.description.is_empty());
        assert!(!plain.has_frontmatter);
    }

    #[test]
    fn disabled_state_filters_and_roundtrips() {
        let tmp = TempDir::new("state");
        let data = tmp.0.join("data");
        let ws = tmp.0.join("ws");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&ws).unwrap();
        write_skill(
            &user_root(&data),
            "a",
            "---\nname: a\ndescription: skill A\n---\nbody",
        );
        write_skill(
            &user_root(&data),
            "b",
            "---\nname: b\ndescription: skill B\n---\nbody",
        );
        let a_md = user_root(&data).join("a").join(SKILL_FILE);
        set_skill_enabled(&data, &a_md, false).unwrap();
        let enabled_list = discover_enabled(&ws, &data);
        let enabled: Vec<&str> = enabled_list.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(enabled, vec!["b"]);
        // Re-enabling = entry removal (only b's disablement would remain, and
        // there is none → skills becomes an empty object)
        set_skill_enabled(&data, &a_md, true).unwrap();
        assert_eq!(discover_enabled(&ws, &data).len(), 2);
    }

    #[test]
    fn skills_section_lists_and_hides_disabled() {
        let tmp = TempDir::new("section");
        let data = tmp.0.join("data");
        let ws = tmp.0.join("ws");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&ws).unwrap();
        assert!(
            skills_section(&ws, &data).is_empty(),
            "empty section when there are no skills"
        );
        write_skill(
            &user_root(&data),
            "pdf",
            "---\nname: pdf\ndescription: PDF toolkit\nwhen_to_use: when generating PDFs\n---\nbody",
        );
        let section = skills_section(&ws, &data);
        assert!(section.contains("## Available skills"));
        assert!(section.contains("- pdf: PDF toolkit - when generating PDFs (file: "));
        assert!(section.contains("call Skill to load it"));
    }

    #[test]
    fn load_skill_output_wraps_body_and_dir() {
        let tmp = TempDir::new("load");
        let data = tmp.0.join("data");
        let ws = tmp.0.join("ws");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&ws).unwrap();
        write_skill(
            &user_root(&data),
            "demo",
            "---\nname: demo\ndescription: demo\n---\nSee ${SKILL_DIR}/README.md",
        );
        let out = load_skill_output(&ws, &data, "demo").unwrap();
        assert!(out.starts_with("<skill_content name=\"demo\">"));
        assert!(out.contains("# Skill: demo"));
        assert!(
            !out.contains("${SKILL_DIR}"),
            "the placeholder should be replaced with the real directory"
        );
        assert!(out.contains("Skill directory: "));
        assert!(out.ends_with("</skill_content>"));
        let err = load_skill_output(&ws, &data, "nonexistent").unwrap_err();
        assert!(
            err.contains("nonexistent"),
            "an unknown skill should error: {err}"
        );
        assert!(
            err.contains("demo"),
            "the error should list available skills: {err}"
        );
    }
}
