use std::io::Write as _;
use std::path::Path;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use pig_protocol::{AppConfig, CoreError};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt as _;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;

/// Write a config/state file atomically and privately.
///
/// The replacement is a same-directory temp file plus rename, so readers see
/// either the old or the new complete document. On Unix the temp file is
/// created with mode 0600 before its contents are written; on Windows the
/// destination inherits the parent's ACLs (no mode change). The parent
/// directory is created when missing, but its permissions are **not** changed:
/// only [`ensure_private_data_dir`] hardens a directory, and only when the
/// caller identifies it as the application data directory.
pub fn write_private_file(path: &Path, contents: &[u8]) -> Result<(), CoreError> {
    atomic_write(path, contents).map_err(|error| CoreError::ConfigWrite {
        path: path.display().to_string(),
        detail: error.to_string(),
    })
}

/// Whether a directory entry is a symlink or (on Windows) any reparse point
/// (a junction is not reported by `FileType::is_symlink`).
fn is_link_like(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

/// Create `directory` when missing and require the **final** component to be a
/// real directory (no symlink, junction or other reparse point).
///
/// Only one level is created, so a planted dangling link cannot make the
/// caller create a tree at its target: the link itself is detected before the
/// creation, and the post-create check catches a racing swap. Parents belong to
/// the caller's controlled layout (workspace, data dir); for a project
/// `.pigcode`, validate it before `.pigcode/skills`.
pub fn ensure_real_directory(directory: &Path) -> std::io::Result<()> {
    let invalid = |metadata: &std::fs::Metadata| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            if is_link_like(metadata) {
                format!(
                    "{} is a symbolic link or reparse point; refusing to write through it",
                    directory.display()
                )
            } else {
                format!("{} is not a directory", directory.display())
            },
        )
    };
    match std::fs::symlink_metadata(directory) {
        Ok(metadata) => {
            if is_link_like(&metadata) || !metadata.is_dir() {
                return Err(invalid(&metadata));
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir(directory)?;
            let metadata = std::fs::symlink_metadata(directory)?;
            if is_link_like(&metadata) || !metadata.is_dir() {
                return Err(invalid(&metadata));
            }
            Ok(())
        }
        Err(error) => Err(error),
    }
}

/// Require an existing real directory (no symlink, junction or other reparse
/// point). Unlike [`ensure_real_directory`] this never creates the path, so
/// validation paths such as a skill deletion cannot recreate what they are
/// about to remove.
pub fn require_real_directory(directory: &Path) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(directory)?;
    if is_link_like(&metadata) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "{} is a symbolic link or reparse point; refusing to write through it",
                directory.display()
            ),
        ));
    }
    if !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} is not a directory", directory.display()),
        ));
    }
    Ok(())
}

/// Atomic settings-file write that refuses a symlinked/reparse final entry and
/// preserves an existing file's Unix mode (a new file stays 0600).
///
/// GUI settings write repository content (project `mcp.json`, `SKILL.md`) and
/// user state; following a planted link would overwrite an arbitrary user file.
/// Callers that write inside a workspace's `.pigcode` must also validate that
/// directory with [`ensure_real_directory`] before this call.
pub fn write_settings_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let write_error = |detail: String| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{}: {detail}", path.display()),
        )
    };
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if is_link_like(&metadata) => {
            return Err(write_error(
                "refusing to replace a symbolic link or reparse point".to_string(),
            ));
        }
        Ok(metadata) if !metadata.is_file() => {
            return Err(write_error(
                "the target exists and is not a regular file".to_string(),
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    atomic_write_inner(path, contents, true)
}

fn atomic_write(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    atomic_write_inner(path, contents, false)
}

#[cfg_attr(not(unix), allow(unused_variables))]
fn atomic_write_inner(path: &Path, contents: &[u8], preserve_mode: bool) -> std::io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    if let Some(parent) = parent {
        std::fs::create_dir_all(parent)?;
    }
    let directory = parent.unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config".to_string());
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let temp = directory.join(format!(".{name}.tmp-{}-{nanos}", std::process::id()));
    let result = (|| -> std::io::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&temp)?;
        file.write_all(contents)?;
        file.flush()?;
        drop(file);
        #[cfg(unix)]
        if preserve_mode && let Ok(metadata) = std::fs::metadata(path) {
            std::fs::set_permissions(&temp, metadata.permissions())?;
        }
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// Restrict a directory to the owner (0700 on Unix) without following a
/// symlink and without touching any other path.
///
/// A symlinked data directory is left as the user configured it: chmod would
/// silently change the link target, which may be outside the application's
/// data. A regular file at the path is an error. On Windows this only verifies
/// the directory exists (ACLs are inherited).
pub fn ensure_private_data_dir(directory: &Path) -> Result<(), CoreError> {
    ensure_private_directory(directory).map_err(|error| CoreError::DataDirCreate {
        path: directory.display().to_string(),
        detail: error.to_string(),
    })
}

fn ensure_private_directory(directory: &Path) -> std::io::Result<()> {
    if directory.as_os_str().is_empty() {
        return Ok(());
    }
    let missing = match std::fs::symlink_metadata(directory) {
        Ok(_) => false,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(error) => return Err(error),
    };
    if missing {
        std::fs::create_dir_all(directory)?;
    }
    let metadata = std::fs::symlink_metadata(directory)?;
    if metadata.file_type().is_symlink() {
        tracing::warn!(
            "not changing the permissions of the symlinked data directory {}",
            directory.display()
        );
        return Ok(());
    }
    if !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the data directory path is not a directory",
        ));
    }
    #[cfg(unix)]
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// Harden the standard data directory when `config_path` lives inside it, or
/// whenever `PIG_DATA_DIR` explicitly names the data directory (the selftest
/// and any isolated run). A custom config path outside an implicitly chosen
/// data directory is left alone, so this never chmods an arbitrary directory.
pub(super) fn harden_default_data_dir(config_path: &Path) {
    let data_dir = pig_utils::data_dir();
    let configured = std::env::var_os("PIG_DATA_DIR").is_some();
    let in_data_dir = config_path.parent() == Some(data_dir.as_path());
    if !configured && !in_data_dir {
        return;
    }
    if let Err(error) = ensure_private_data_dir(&data_dir) {
        tracing::warn!(
            "could not restrict the data directory {}: {error:?}",
            data_dir.display()
        );
    }
}

pub fn save(path: &std::path::Path, config: &AppConfig) -> Result<(), CoreError> {
    harden_default_data_dir(path);
    let raw = toml::to_string_pretty(config).map_err(|e| CoreError::ConfigSerialize {
        detail: e.to_string(),
    })?;
    write_private_file(path, raw.as_bytes())?;
    harden_default_data_dir(path);
    Ok(())
}
