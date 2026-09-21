//! 工作区/会话路径的入口归一：`~` 展开 + 绝对化 + 词法规整（消化 `.`/`..`、
//! 重复与结尾分隔符）。刻意不做大小写折叠、不解析符号链接——与 kimi-code/ZCode
//! 一致：归组靠入口产出一致字符串，别名字段偏严不匹配，也不在大小写敏感卷上误并。

use std::path::{Component, Path, PathBuf};

/// 归一工作区路径。同一目录的不同写法（尾斜杠、相对路径、`~`）归到同一 key；
/// 大小写变体与符号链接别名仍视为不同路径。
pub fn normalize_workspace_path(path: &Path) -> PathBuf {
    let expanded = expand_tilde(path);
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&expanded))
            .unwrap_or(expanded)
    };
    // Path::components 已忽略重复分隔符与结尾分隔符（根除外）
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
    } else if let Some(rest) = text.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
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

    #[test]
    fn strips_trailing_and_duplicate_separators() {
        assert_eq!(
            normalize_workspace_path(Path::new("/Users/x/a/")),
            PathBuf::from("/Users/x/a")
        );
        assert_eq!(
            normalize_workspace_path(Path::new("/Users/x//a//b/")),
            PathBuf::from("/Users/x/a/b")
        );
        assert_eq!(normalize_workspace_path(Path::new("/")), PathBuf::from("/"));
    }

    #[test]
    fn resolves_dot_segments_lexically() {
        assert_eq!(
            normalize_workspace_path(Path::new("/Users/x/a/./b/../c")),
            PathBuf::from("/Users/x/a/c")
        );
        // 越过根的 .. 不炸，钳在根上
        assert_eq!(
            normalize_workspace_path(Path::new("/a/../../b")),
            PathBuf::from("/b")
        );
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
        assert_eq!(
            normalize_workspace_path(Path::new("a/b")),
            cwd.join("a/b")
        );
    }

    #[test]
    fn keeps_case_and_spelling() {
        // 不折大小写、不做 realpath：词法不同的路径保持不同
        assert_ne!(
            normalize_workspace_path(Path::new("/Users/x/a")),
            normalize_workspace_path(Path::new("/users/x/a"))
        );
    }
}
