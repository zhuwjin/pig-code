//! Entry-point normalization for workspace/session paths: `~` expansion + absolutization +
//! lexical cleanup (digesting `.`/`..`, duplicate and trailing separators). Deliberately no case
//! folding and no symlink resolution — consistent with kimi-code/ZCode: grouping relies on the
//! entry point producing identical strings; alias fields err on the strict side and never
//! accidentally merge on case-sensitive volumes.
//!
//! Also the shared data-directory and session-media layout conventions (written
//! by pig-core's rollout, read by pig-app's attachment rendering).

use std::path::{Component, Path, PathBuf};

/// Data directory: the PIG_DATA_DIR environment variable first (self-test isolation); default ~/.pigcode.
pub fn data_dir() -> PathBuf {
    std::env::var_os("PIG_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".pigcode")
        })
}

/// Session attachment media layout: `{sessions_dir}/{session_id}.media/` (the
/// rollout writes pasted/attached images there; the UI reads them back for
/// rendering)
pub fn media_dir(sessions_dir: &Path, session_id: &str) -> PathBuf {
    sessions_dir.join(format!("{session_id}.media"))
}

/// Normalize a workspace path. Different spellings of the same directory (trailing slash, relative path, `~`) map to the same key;
/// case variants and symlink aliases remain distinct paths.
pub fn normalize_workspace_path(path: &Path) -> PathBuf {
    let expanded = expand_tilde(path);
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&expanded))
            .unwrap_or(expanded)
    };
    // Path::components already ignores duplicate and trailing separators (except the root)
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn expand_tilde(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if text == "~" {
        if let Some(home) = home_dir() {
            return home;
        }
    } else if let Some(rest) = text.strip_prefix("~/")
        && let Some(home) = home_dir()
    {
        return home.join(rest);
    }
    path.to_path_buf()
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Absolute-path root: "/" on Unix; Windows needs a drive letter ("/x" has no drive on Windows and is not absolute)
    #[cfg(unix)]
    const ROOT: &str = "/";
    #[cfg(windows)]
    const ROOT: &str = "C:/";

    fn rooted(segments: &str) -> PathBuf {
        PathBuf::from(format!("{ROOT}{segments}"))
    }

    #[test]
    fn strips_trailing_and_duplicate_separators() {
        assert_eq!(
            normalize_workspace_path(&rooted("Users/x/a/")),
            rooted("Users/x/a")
        );
        assert_eq!(
            normalize_workspace_path(&rooted("Users/x//a//b/")),
            rooted("Users/x/a/b")
        );
        assert_eq!(
            normalize_workspace_path(Path::new(ROOT)),
            PathBuf::from(ROOT)
        );
    }

    #[test]
    fn resolves_dot_segments_lexically() {
        assert_eq!(
            normalize_workspace_path(&rooted("Users/x/a/./b/../c")),
            rooted("Users/x/a/c")
        );
        // .. above the root does not panic; clamped to the root
        assert_eq!(normalize_workspace_path(&rooted("a/../../b")), rooted("b"));
    }

    #[test]
    fn expands_tilde() {
        let Some(home) = home_dir() else { return };
        assert_eq!(normalize_workspace_path(Path::new("~")), home.clone());
        assert_eq!(
            normalize_workspace_path(Path::new("~/a/b")),
            home.join("a/b")
        );
    }

    #[test]
    fn absolutizes_relative_paths() {
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(normalize_workspace_path(Path::new("a/b")), cwd.join("a/b"));
    }

    #[test]
    fn keeps_case_and_spelling() {
        // No case folding, no realpath: lexically different paths stay different
        assert_ne!(
            normalize_workspace_path(&rooted("Users/x/a")),
            normalize_workspace_path(&rooted("users/x/a"))
        );
    }
}
