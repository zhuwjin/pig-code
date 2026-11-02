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

use pig_protocol::AppConfig;
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

/// Create the memory directory + a fresh empty index when missing (run at
/// session create/load when memory is on, so the directory genuinely exists
/// before the prompt tells the model "this directory already exists — write
/// to it directly"). Best-effort: a read-only data_dir logs and continues
/// (memory writes then fail individually with tool errors).
pub fn ensure_initialized(data_dir: &Path, cwd: &Path) {
    let root = workspace_memory_dir(data_dir, cwd);
    let index = root.join("MEMORY.md");
    if let Err(error) = std::fs::create_dir_all(&root) {
        tracing::warn!("workspace memory directory unavailable: {error:?}");
        return;
    }
    if !index.exists()
        && let Err(error) = std::fs::write(&index, "# Memory\n")
    {
        tracing::warn!("workspace memory index seed failed: {error:?}");
    }
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
