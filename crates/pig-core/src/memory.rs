//! Cross-session workspace memory (ZCode's model-managed memory, ported).
//!
//! Storage: a per-workspace directory `{data_dir}/memories/projects/{slug}-{hash8}/`
//! (slug = sanitized workspace dir name, hash = sha256 of the canonical path —
//! ZCode's project-root scheme) holding **one markdown file per fact** plus a
//! `MEMORY.md` **index** at its root (one line per memory:
//! `- [Title](file.md) — hook`).
//!
//! The model is the ONLY writer, with the plain tools: the injected
//! system-prompt section carries the usage instructions + the index (never
//! the memory content — the model Reads the files it needs on demand,
//! ZCode's default-index recall), and the memory directory is registered as
//! a read-write root (approval-free Write/Edit/Read, absolute-path only —
//! see task::SessionToolState::extra_rw_roots). The user shapes memory the
//! same way ("remember X" / "forget X") or edits the plain files on disk; there is no
//! dedicated manual path. Mid-session index changes ride the turn_reminder
//! change-triggered channel (the reminder carries the fresh index, same as
//! AGENTS.md changes).
//!
//! All model-facing text here is English (core convention); memory content is
//! user data and stays in whatever language it was written in.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use pig_protocol::{AppConfig, BotMemoryFile};
use sha2::{Digest, Sha256};

/// Index injection budget (ZCode's index cap): the index stays small enough
/// to live in every session's system prompt; a pathological oversized index
/// (the model is prompt-guided to keep it tight) truncates at injection with
/// a marker rather than breaking the prompt
pub(crate) const INDEX_MAX_BYTES: usize = 25 * 1024;

/// The workspace's memory directory: `{data_dir}/memories/projects/{slug}-{hash}`,
/// slug = sanitized workspace dir name, hash = first 12 hex of sha256 over the
/// canonical path. Distinct workspaces never collide; the same workspace
/// reached through different path spellings canonicalizes to the same
/// directory.
pub fn workspace_memory_dir(data_dir: &Path, cwd: &Path) -> PathBuf {
    let canonical = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    let hash = format!("{:x}", hasher.finalize());
    data_dir.join("memories").join("projects").join(format!(
        "{}-{}",
        workspace_slug(cwd),
        &hash[..12]
    ))
}

/// Sanitized workspace dir name for the memory directory: ascii lowercase
/// alphanumerics kept, everything else collapsed to '-', capped at 24 chars;
/// empty results (e.g. "/") fall back to "project"
fn workspace_slug(cwd: &Path) -> String {
    let name = cwd
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut slug = String::new();
    let mut last_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
        if slug.len() >= 24 {
            break;
        }
    }
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        "project".into()
    } else {
        slug
    }
}

/// Bot personal long-term memory directory: `{data_dir}/memories/bots/{bot_id}/`.
/// Follows `bot_id` across workspaces and syncs across same-account devices.
pub fn bot_memory_dir(data_dir: &Path, bot_id: &str) -> PathBuf {
    let safe = sanitize_memory_key(bot_id, "bot");
    data_dir.join("memories").join("bots").join(safe)
}

/// Shared/team memory directory: `{data_dir}/memories/shared/{scope}/`.
/// Reserved for multi-bot collaborative channels and team-wide conventions.
pub fn shared_memory_dir(data_dir: &Path, scope: &str) -> PathBuf {
    let safe = sanitize_memory_key(scope, "default");
    data_dir.join("memories").join("shared").join(safe)
}

fn sanitize_memory_key(raw: &str, fallback: &str) -> String {
    let mut slug = String::new();
    for ch in raw.trim().chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
            slug.push(ch.to_ascii_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
        if slug.len() >= 64 {
            break;
        }
    }
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        fallback.to_string()
    } else {
        slug
    }
}

fn ensure_memory_dir(root: &Path, heading: &str) {
    let index = root.join("MEMORY.md");
    if let Err(error) = std::fs::create_dir_all(root) {
        tracing::warn!("memory directory unavailable {}: {error:?}", root.display());
        return;
    }
    if !index.exists()
        && let Err(error) = std::fs::write(&index, format!("# {heading}\n"))
    {
        tracing::warn!("memory index seed failed {}: {error:?}", index.display());
    }
}

/// Create the workspace memory directory + fresh index when missing.
pub fn ensure_initialized(data_dir: &Path, cwd: &Path) {
    let root = workspace_memory_dir(data_dir, cwd);
    ensure_memory_dir(&root, "Memory");
}

/// Create a Bot's personal memory directory + fresh index when missing.
pub fn ensure_bot_initialized(data_dir: &Path, bot_id: &str) {
    let root = bot_memory_dir(data_dir, bot_id);
    ensure_memory_dir(&root, &format!("Bot Memory ({bot_id})"));
}

/// Create a shared/team memory directory + fresh index when missing.
pub fn ensure_shared_initialized(data_dir: &Path, scope: &str) {
    let root = shared_memory_dir(data_dir, scope);
    ensure_memory_dir(&root, &format!("Shared Memory ({scope})"));
}

/// Compute all active memory RW roots for a session (Bot personal + Workspace
/// project + optional Shared/team scope).
pub fn layered_memory_roots(
    data_dir: &Path,
    bot_id: Option<&str>,
    workspace_cwd: Option<&Path>,
    shared_scope: Option<&str>,
) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(bot_id) = bot_id {
        roots.push(bot_memory_dir(data_dir, bot_id));
    }
    if let Some(cwd) = workspace_cwd {
        roots.push(workspace_memory_dir(data_dir, cwd));
    }
    if let Some(scope) = shared_scope {
        roots.push(shared_memory_dir(data_dir, scope));
    }
    roots
}

/// Initialize all active memory tiers for a session before building its prompt.
pub fn ensure_layered_initialized(
    data_dir: &Path,
    bot_id: Option<&str>,
    workspace_cwd: Option<&Path>,
    shared_scope: Option<&str>,
) {
    if let Some(bot_id) = bot_id {
        ensure_bot_initialized(data_dir, bot_id);
    }
    if let Some(cwd) = workspace_cwd {
        ensure_initialized(data_dir, cwd);
    }
    if let Some(scope) = shared_scope {
        ensure_shared_initialized(data_dir, scope);
    }
}

/// Build the prefix-cache-friendly three-tier memory section for a session:
/// 1. Bot personal long-term memory (cross-workspace, cross-device)
/// 2. Workspace project memory (local to this repository/directory)
/// 3. Shared/team memory (optional collaborative scope)
pub fn layered_memory_section(
    config: &AppConfig,
    data_dir: &Path,
    bot_id: Option<&str>,
    workspace_cwd: Option<&Path>,
    shared_scope: Option<&str>,
) -> String {
    if !config.memory.enabled {
        return String::new();
    }
    layered_memory_section_raw(data_dir, bot_id, workspace_cwd, shared_scope)
}

pub(crate) fn layered_memory_section_raw(
    data_dir: &Path,
    bot_id: Option<&str>,
    workspace_cwd: Option<&Path>,
    shared_scope: Option<&str>,
) -> String {
    let mut sections = Vec::new();
    if let Some(bot_id) = bot_id {
        let root = bot_memory_dir(data_dir, bot_id);
        let index = read_index(&root.join("MEMORY.md"));
        sections.push(bot_memory_section_raw(bot_id, &root, &index));
    }
    if let Some(cwd) = workspace_cwd {
        let root = workspace_memory_dir(data_dir, cwd);
        let index = read_index(&root.join("MEMORY.md"));
        sections.push(memory_section_raw(&root, &index));
    }
    if let Some(scope) = shared_scope {
        let root = shared_memory_dir(data_dir, scope);
        let index = read_index(&root.join("MEMORY.md"));
        sections.push(shared_memory_section_raw(scope, &root, &index));
    }
    sections.join("\n")
}

fn format_index_body(index: &str) -> String {
    if index.trim().is_empty() {
        "(no memories saved yet)".to_string()
    } else if index.len() <= INDEX_MAX_BYTES {
        index.trim().to_string()
    } else {
        let mut end = INDEX_MAX_BYTES;
        while !index.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}\n[MEMORY.md too long; truncated]", &index[..end])
    }
}

pub(crate) fn bot_memory_section_raw(bot_id: &str, root: &Path, index: &str) -> String {
    let body = format_index_body(index);
    format!(
        "## Bot personal memory (`{bot_id}`)\n\
         You keep a persistent personal long-term memory across all workspaces and devices at \
         `{}/`. This directory already exists — read and write its markdown files directly with \
         Read/Write/Edit (no approval needed). Store durable role methodology, cross-project user \
         preferences, and lessons learned here; keep project-specific facts in Workspace memory. \
         Never store secrets or API keys. Update `MEMORY.md` with one-line pointers (`- [Title](file.md) — hook`).\n\
         \n\
         The current personal memory index follows:\n\
         Contents of {}:\n\
         {}\n",
        root.display(),
        root.join("MEMORY.md").display(),
        body
    )
}

pub(crate) fn shared_memory_section_raw(scope: &str, root: &Path, index: &str) -> String {
    let body = format_index_body(index);
    format!(
        "## Shared team memory (`{scope}`)\n\
         Shared multi-bot memory is located at `{}/`. Use Read/Write/Edit for durable cross-bot \
         decisions and shared conventions, indexed in `MEMORY.md`.\n\
         \n\
         Contents of {}:\n\
         {}\n",
        root.display(),
        root.join("MEMORY.md").display(),
        body
    )
}

/// Collect all markdown files in a Bot's personal memory directory for account
/// synchronization. Non-markdown entries, subdirectories, and symlinks are skipped.
pub fn collect_bot_memory_files(data_dir: &Path, bot_id: &str) -> Vec<BotMemoryFile> {
    let root = bot_memory_dir(data_dir, bot_id);
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if !is_safe_memory_filename(&name) {
            continue;
        }
        let path = entry.path();
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let updated_at_ms = std::fs::metadata(&path)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|mtime| mtime.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0);
        files.push(BotMemoryFile {
            path: name,
            content,
            updated_at_ms,
            deleted: false,
        });
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    files
}

/// Apply synced Bot personal memory files locally using last-write-wins by
/// `updated_at_ms`. Returns the number of files written or removed.
pub fn apply_bot_memory_files(
    data_dir: &Path,
    bot_id: &str,
    files: &[BotMemoryFile],
) -> Result<usize, String> {
    if files.is_empty() {
        return Ok(0);
    }
    let root = bot_memory_dir(data_dir, bot_id);
    std::fs::create_dir_all(&root).map_err(|error| error.to_string())?;
    let mut applied = 0;
    for file in files {
        if !is_safe_memory_filename(&file.path) {
            continue;
        }
        let target = root.join(&file.path);
        let local_mtime = std::fs::metadata(&target)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|mtime| mtime.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0);
        if target.exists() && local_mtime > file.updated_at_ms {
            continue;
        }
        if file.deleted {
            if target.exists() {
                let _ = std::fs::remove_file(&target);
                applied += 1;
            }
            continue;
        }
        if target.exists()
            && std::fs::read_to_string(&target).ok().as_deref() == Some(file.content.as_str())
        {
            continue;
        }
        std::fs::write(&target, &file.content).map_err(|error| error.to_string())?;
        applied += 1;
    }
    Ok(applied)
}

fn is_safe_memory_filename(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains("..")
        && name.ends_with(".md")
}

/// Raw index content ("" when missing or unreadable)
pub fn read_index(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// The frozen system-prompt section for a session: empty when memory is
/// disabled; otherwise ALWAYS present — ZCode-style usage instructions (the
/// directory exists, write with the plain tools, one file per fact +
/// frontmatter, update the index pointer, what to save) plus the current
/// INDEX (never the memory content: the model Reads facts on demand).
pub fn memory_section(config: &AppConfig, data_dir: &Path, cwd: &Path) -> String {
    if !config.memory.enabled {
        return String::new();
    }
    let root = workspace_memory_dir(data_dir, cwd);
    let index = read_index(&root.join("MEMORY.md"));
    memory_section_raw(&root, &index)
}

/// Config-independent section build (the turn_reminder fresh check reuses it:
/// a session that started with memory off passes an empty fresh section and
/// never reminds). Non-empty for ANY index — the empty case renders the
/// "(no memories saved yet)" placeholder. A pathological oversized index is
/// truncated at injection with a marker (the write side trims manual
/// appends; a model-written index trusting the prompt usually stays small).
pub(crate) fn memory_section_raw(root: &Path, index: &str) -> String {
    let body = if index.trim().is_empty() {
        "(no memories saved yet)".to_string()
    } else if index.len() <= INDEX_MAX_BYTES {
        index.trim().to_string()
    } else {
        let mut end = INDEX_MAX_BYTES;
        while !index.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}\n[MEMORY.md too long; truncated]", &index[..end])
    };
    format!(
        "## Workspace memory\n\
         You keep a persistent file-based memory for this workspace at `{}/`. This directory \
         already exists — read and write its files directly with the Read/Write/Edit tools (it \
         sits outside the workspace but is always allowed for your memory files; no approval \
         needed). Each memory is one file holding one fact, with frontmatter:\n\
         \n\
         ```markdown\n\
         ---\n\
         name: <short-kebab-case-slug>\n\
         description: <one-line summary — used to decide relevance>\n\
         metadata:\n\
           type: user | feedback | project | reference\n\
         ---\n\
         \n\
         <the fact; for feedback/project include why it matters and how to apply it>\n\
         ```\n\
         \n\
         Types: `user` — who the user is (role, expertise, preferred language, stable \
         preferences about responses or workflow). `feedback` — corrections or guidance the \
         user gave about how to work, including the why. `project` — ongoing goals, \
         constraints, conventions not derivable from the code or git history (convert \
         relative dates to absolute). `reference` — external pointers (URLs, dashboards, \
         tickets).\n\
         \n\
         After writing a memory file, add or update its one-line pointer in `MEMORY.md` \
         (`- [Title](file.md) — hook`); `MEMORY.md` is the index loaded into your context \
         each session — never put memory content in it. Before saving, check for an existing \
         file that already covers it — update that file rather than creating a duplicate; \
         delete memories that turn out to be wrong. Don't save what the repo already records \
         (code structure, past fixes, git history, AGENTS.md), what only matters to this \
         conversation, or secrets and credentials. Use your memory when you learn something \
         durable, and immediately when the user asks you to remember or forget something.\n\
         \n\
         The current index follows:\n\
         Contents of {}:\n\
         {}\n",
        root.display(),
        root.join("MEMORY.md").display(),
        body
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use pig_protocol::MemoryConfig;

    /// Minimal temp-dir helper (same approach as prompt.rs's test module)
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("pig-memory-test-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn config(enabled: bool) -> AppConfig {
        AppConfig {
            memory: MemoryConfig { enabled },
            ..AppConfig::default()
        }
    }

    /// Distinct workspaces map to distinct memory dirs; the same workspace is
    /// stable across calls; the slug is sanitized and the hash suffixed
    #[test]
    fn memory_dir_is_per_workspace_and_stable() {
        let data = Path::new("/data");
        let a = workspace_memory_dir(data, Path::new("/home/jin/My Project"));
        let b = workspace_memory_dir(data, Path::new("/home/jin/other"));
        assert_ne!(a, b);
        assert_eq!(
            a,
            workspace_memory_dir(data, Path::new("/home/jin/My Project"))
        );
        let name = a.file_name().unwrap().to_string_lossy().to_string();
        assert!(
            name.starts_with("my-project-") && name.len() > "my-project-".len(),
            "sanitized slug + hash: {name}"
        );
        let root = workspace_memory_dir(data, Path::new("/"));
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("project-"),
            "root cwd falls back to the project slug"
        );
    }

    /// ensure_initialized creates the directory + a fresh index; a second run
    /// never clobbers an existing index
    #[test]
    fn ensure_initialized_seeds_once() {
        let tmp = TempDir::new("init");
        let data = tmp.path().join("data");
        let cwd = tmp.path().join("ws");
        std::fs::create_dir_all(&cwd).unwrap();
        ensure_initialized(&data, &cwd);
        let index = workspace_memory_dir(&data, &cwd).join("MEMORY.md");
        assert_eq!(read_index(&index), "# Memory\n");
        std::fs::write(&index, "# Memory\n\n- [a](a.md)\n").unwrap();
        ensure_initialized(&data, &cwd);
        assert!(
            read_index(&index).contains("- [a](a.md)"),
            "an existing index is never clobbered"
        );
    }

    /// The frozen section: empty ONLY when disabled — enabled renders the
    /// ZCode-style instructions + the index (placeholder when empty), never
    /// the memory content itself
    #[test]
    fn memory_section_carries_instructions_and_index() {
        let tmp = TempDir::new("section");
        let data = tmp.path().join("data");
        let cwd = tmp.path().join("ws");
        std::fs::create_dir_all(&cwd).unwrap();
        assert!(memory_section(&config(false), &data, &cwd).is_empty());
        // Enabled + empty index: instructions + placeholder
        let empty = memory_section(&config(true), &data, &cwd);
        assert!(empty.starts_with("## Workspace memory\n"));
        assert!(empty.contains("(no memories saved yet)"));
        assert!(empty.contains("This directory already exists"));
        assert!(empty.contains("type: user | feedback | project | reference"));
        // Enabled + index: the pointer lines are injected, root path included
        let root = workspace_memory_dir(&data, &cwd);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("MEMORY.md"),
            "# Memory\n\n- [Prefers concise replies](prefers-concise.md) — reply style\n",
        )
        .unwrap();
        let section = memory_section(&config(true), &data, &cwd);
        assert!(section.contains("- [Prefers concise replies](prefers-concise.md)"));
        assert!(section.contains(&format!("Contents of {}", root.join("MEMORY.md").display())));
    }
}
