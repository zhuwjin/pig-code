use std::io::Read as _;
use std::path::{Component, Path};

use pig_protocol::{CoreError, PluginWorkspaceFileReadRequest};

/// Files larger than this are read only up to the cap and reported truncated.
pub const MAX_READ_BYTES: usize = 4 * 1024 * 1024;
/// Hard cap for one response's text.
pub const MAX_RETURN_BYTES: usize = 1024 * 1024;
pub const DEFAULT_RETURN_BYTES: usize = 256 * 1024;
pub const MAX_LINES: u64 = 5_000;

/// Read one workspace-relative text file for a plugin.
///
/// The path is confined to `root` both lexically and through canonicalization;
/// `.git` components and symlinks are refused so a read cannot escape through
/// repository metadata or a link. Output is bounded and line ranges are
/// inclusive. The returned `truncated` flag covers both the file-size cap and
/// the response-byte budget.
pub fn read_workspace_file(
    root: &Path,
    request: &PluginWorkspaceFileReadRequest,
) -> Result<serde_json::Value, CoreError> {
    let root = root.canonicalize().map_err(io)?;
    let relative = Path::new(request.path.trim());
    if request.path.trim().is_empty() || relative.is_absolute() {
        return Err(invalid(
            "the path must be a non-empty workspace-relative path",
        ));
    }
    if !relative
        .components()
        .all(|part| matches!(part, Component::Normal(_)))
    {
        return Err(invalid("the path must not contain `.` or `..` components"));
    }
    reject_git_component(relative)?;
    let target = root.join(relative);
    let metadata = std::fs::symlink_metadata(&target)
        .map_err(|error| invalid(format!("cannot read `{}`: {error}", request.path.trim())))?;
    if metadata.file_type().is_symlink() {
        return Err(invalid("workspace file reads do not follow symlinks"));
    }
    if !metadata.is_file() {
        return Err(invalid("the path is not a regular file"));
    }
    let canonical = target.canonicalize().map_err(io)?;
    if !canonical.starts_with(&root) {
        return Err(CoreError::PluginPermission {
            detail: "the file is outside the session workspace".into(),
        });
    }
    let relative_canonical =
        canonical
            .strip_prefix(&root)
            .map_err(|_| CoreError::PluginPermission {
                detail: "the file is outside the session workspace".into(),
            })?;
    reject_git_component(relative_canonical)?;

    let size = metadata.len();
    let read_limit = size.min(MAX_READ_BYTES as u64) as usize;
    let mut bytes = Vec::with_capacity(read_limit);
    std::fs::File::open(&canonical)
        .map_err(io)?
        .take(read_limit as u64)
        .read_to_end(&mut bytes)
        .map_err(io)?;
    if bytes.iter().any(|byte| *byte == 0) {
        return Err(invalid("the file appears to be binary"));
    }
    let size_truncated = size > MAX_READ_BYTES as u64;
    let mut content = match String::from_utf8(bytes) {
        Ok(content) => content,
        // A byte-capped read can cut a multi-byte character: keep the valid
        // prefix instead of rejecting an otherwise readable file.
        Err(error) if size_truncated && error.utf8_error().valid_up_to() > 0 => {
            let valid = error.utf8_error().valid_up_to();
            String::from_utf8_lossy(&error.as_bytes()[..valid]).into_owned()
        }
        Err(_) => return Err(invalid("the file is not valid UTF-8 text")),
    };

    let total_lines = content.lines().count() as u64;
    let start = request.start_line.unwrap_or(1).max(1);
    let explicit_end = request.end_line;
    let mut end = explicit_end.unwrap_or(total_lines.max(1));
    if end < start {
        return Err(invalid(
            "endLine must be greater than or equal to startLine",
        ));
    }
    if start > total_lines.max(1) {
        return Err(invalid("startLine is past the end of the file"));
    }
    let line_truncated = explicit_end.is_none() && end.saturating_sub(start) + 1 > MAX_LINES;
    if explicit_end.is_some() && end.saturating_sub(start) + 1 > MAX_LINES {
        return Err(invalid(format!(
            "a read may return at most {MAX_LINES} lines"
        )));
    }
    if line_truncated {
        end = start + MAX_LINES - 1;
    }
    let end = end.min(total_lines.max(1));
    let selected: Vec<&str> = content
        .lines()
        .skip((start - 1) as usize)
        .take((end - start + 1) as usize)
        .collect();
    let actual_end = start + selected.len().saturating_sub(1) as u64;
    content = selected.join("\n");
    let budget = request
        .max_bytes
        .unwrap_or(DEFAULT_RETURN_BYTES)
        .clamp(1, MAX_RETURN_BYTES);
    let budget_truncated = content.len() > budget;
    if budget_truncated {
        let mut cut = budget;
        while !content.is_char_boundary(cut) {
            cut -= 1;
        }
        content.truncate(cut);
    }
    Ok(serde_json::json!({
        "path": request.path.trim(),
        "content": content,
        "startLine": start,
        "endLine": actual_end,
        "totalLines": total_lines,
        "size": size,
        "truncated": size_truncated || budget_truncated || line_truncated,
    }))
}

fn reject_git_component(path: &Path) -> Result<(), CoreError> {
    if path.components().any(|part| match part {
        Component::Normal(name) => name
            .to_str()
            .is_some_and(|name| name.eq_ignore_ascii_case(".git")),
        _ => false,
    }) {
        return Err(invalid("the path must not target repository metadata"));
    }
    Ok(())
}

fn invalid(detail: impl Into<String>) -> CoreError {
    CoreError::PluginInvalidInput {
        detail: detail.into(),
    }
}

fn io(error: std::io::Error) -> CoreError {
    CoreError::Internal {
        detail: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "pig-workspace-read-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("src")).expect("create workspace");
        std::fs::write(
            root.join("src/lib.rs"),
            "fn one() {}\nfn two() {}\nfn three() {}\n",
        )
        .expect("write source");
        std::fs::write(root.join("binary.bin"), b"a\0b").expect("write binary");
        root
    }

    fn request(path: &str) -> PluginWorkspaceFileReadRequest {
        PluginWorkspaceFileReadRequest {
            path: path.into(),
            start_line: None,
            end_line: None,
            max_bytes: None,
        }
    }

    #[test]
    fn reads_a_line_range_inside_the_workspace() {
        let root = fixture();
        let value = read_workspace_file(
            &root,
            &PluginWorkspaceFileReadRequest {
                path: "src/lib.rs".into(),
                start_line: Some(2),
                end_line: Some(3),
                max_bytes: Some(4096),
            },
        )
        .expect("read range");
        assert_eq!(value["content"], "fn two() {}\nfn three() {}");
        assert_eq!(value["startLine"], 2);
        assert_eq!(value["endLine"], 3);
        assert_eq!(value["totalLines"], 3);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn refuses_traversal_absolute_paths_git_and_binary_files() {
        let root = fixture();
        for path in ["../outside.txt", "C:\\outside.txt", ".git/config"] {
            assert!(
                matches!(
                    read_workspace_file(&root, &request(path)),
                    Err(CoreError::PluginInvalidInput { .. })
                        | Err(CoreError::PluginPermission { .. })
                ),
                "{path}"
            );
        }
        assert!(matches!(
            read_workspace_file(&root, &request("binary.bin")),
            Err(CoreError::PluginInvalidInput { .. })
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn refuses_a_symlink_that_escapes_the_workspace() {
        use std::os::unix::fs::symlink;
        let root = fixture();
        let outside = root.with_extension("outside");
        std::fs::write(&outside, "secret").expect("outside file");
        symlink(&outside, root.join("link.txt")).expect("symlink");
        assert!(matches!(
            read_workspace_file(&root, &request("link.txt")),
            Err(CoreError::PluginInvalidInput { .. })
        ));
        let _ = std::fs::remove_file(outside);
        let _ = std::fs::remove_dir_all(root);
    }
}
