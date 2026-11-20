//! Execution environment abstraction (`ExecutorFileSystem` + `ProcessService`).
//!
//! Decouples core tools (`Read`, `Write`, `Edit`, `ExecCommand`, etc.) from
//! direct local OS calls (`std::fs`, `tokio::process::Command`) so local
//! workspaces, test doubles, and future remote executors (SSH / Docker /
//! exec-server) share one uniform contract.

use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;

use super::process::{
    ForegroundOutcome, run_foreground_with_profile, spawn_background_with_profile,
};
use super::registry::SessionToolState;
use super::sandbox::{AdditionalPermissions, SandboxOverride, SandboxProfile};
use super::shell::ModelShell;

/// Platform-neutral file metadata returned by [`ExecutorFileSystem`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FsMetadata {
    pub len: u64,
    pub is_file: bool,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub modified: Option<std::time::SystemTime>,
}

impl From<std::fs::Metadata> for FsMetadata {
    fn from(meta: std::fs::Metadata) -> Self {
        let ft = meta.file_type();
        Self {
            len: meta.len(),
            is_file: meta.is_file(),
            is_dir: meta.is_dir(),
            is_symlink: ft.is_symlink(),
            modified: meta.modified().ok(),
        }
    }
}

/// One directory entry returned by [`ExecutorFileSystem::read_dir`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsDirEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub is_file: bool,
    pub is_symlink: bool,
}

/// Abstract filesystem boundary for tool execution (modeled on Codex
/// `ExecutorFileSystem` / Kimi `IHostFileSystem` / DeepSeek `ctx.fs`).
pub trait ExecutorFileSystem: Send + Sync {
    /// Bounded read: reads at most `max_bytes + 1` bytes so callers can enforce
    /// size caps even if a file grows between `metadata` and read.
    fn read_bytes_bounded(&self, path: &Path, max_bytes: u64) -> io::Result<Vec<u8>>;

    /// Read the full file contents.
    fn read_all(&self, path: &Path) -> io::Result<Vec<u8>>;

    /// Atomic file write (temp file in same directory + rename on local FS,
    /// preserving existing Unix file mode).
    fn write_atomic(&self, path: &Path, bytes: &[u8]) -> io::Result<()>;

    /// Stat following symlinks.
    fn metadata(&self, path: &Path) -> io::Result<FsMetadata>;

    /// Stat without following a final symlink.
    fn symlink_metadata(&self, path: &Path) -> io::Result<FsMetadata>;

    /// Whether `path` currently exists.
    fn exists(&self, path: &Path) -> bool {
        self.metadata(path).is_ok()
    }

    /// Resolve a canonical absolute path (stripping `\\?\` verbatim prefixes on Windows).
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf>;

    /// Recursively create directories.
    fn create_dir_all(&self, path: &Path) -> io::Result<()>;

    /// Remove a file.
    fn remove_file(&self, path: &Path) -> io::Result<()>;

    /// List entries of a directory.
    fn read_dir(&self, path: &Path) -> io::Result<Vec<FsDirEntry>>;
}

/// Owned foreground command execution request passed to [`ProcessService`].
#[derive(Clone, Debug)]
pub struct ProcessRunRequest {
    pub workspace: PathBuf,
    pub workdir: PathBuf,
    pub command: String,
    pub shell: ModelShell,
    pub timeout: Duration,
    pub sandbox_override: SandboxOverride,
    pub sandbox_profile: SandboxProfile,
    pub additional_permissions: AdditionalPermissions,
}

/// Owned background command execution request passed to [`ProcessService`].
#[derive(Clone, Debug)]
pub struct ProcessBackgroundRequest {
    pub workspace: PathBuf,
    pub workdir: PathBuf,
    pub command: String,
    pub shell: ModelShell,
    pub sandbox_override: SandboxOverride,
    pub sandbox_profile: SandboxProfile,
    pub additional_permissions: AdditionalPermissions,
}

/// Abstract process execution boundary (modeled on Kimi `IHostProcessService`
/// / DeepSeek `ctx.subprocess` / Codex `codex-exec-server`).
pub trait ProcessService: Send + Sync {
    fn run_foreground(
        &self,
        state: SessionToolState,
        request: ProcessRunRequest,
    ) -> BoxFuture<'static, ForegroundOutcome>;

    fn spawn_background(
        &self,
        state: SessionToolState,
        request: ProcessBackgroundRequest,
    ) -> String;

    fn supports_pty(&self) -> bool {
        false
    }
}

/// Target environment classification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnvironmentKind {
    Local,
    Remote { label: String },
}

/// Unified execution environment attached to a [`SessionToolState`].
#[derive(Clone)]
pub struct ExecutionEnvironment {
    pub kind: EnvironmentKind,
    pub fs: Arc<dyn ExecutorFileSystem>,
    pub process: Arc<dyn ProcessService>,
}

impl ExecutionEnvironment {
    pub fn local() -> Self {
        Self {
            kind: EnvironmentKind::Local,
            fs: Arc::new(LocalFileSystem),
            process: Arc::new(LocalProcessService),
        }
    }

    pub fn custom(
        kind: EnvironmentKind,
        fs: Arc<dyn ExecutorFileSystem>,
        process: Arc<dyn ProcessService>,
    ) -> Self {
        Self { kind, fs, process }
    }

    pub fn is_local(&self) -> bool {
        matches!(self.kind, EnvironmentKind::Local)
    }
}

impl Default for ExecutionEnvironment {
    fn default() -> Self {
        Self::local()
    }
}

/// Host local filesystem implementation of [`ExecutorFileSystem`].
#[derive(Clone, Copy, Debug, Default)]
pub struct LocalFileSystem;

impl ExecutorFileSystem for LocalFileSystem {
    fn read_bytes_bounded(&self, path: &Path, max_bytes: u64) -> io::Result<Vec<u8>> {
        let file = std::fs::File::open(path)?;
        let mut bytes = Vec::new();
        file.take(max_bytes.saturating_add(1))
            .read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    fn read_all(&self, path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    fn write_atomic(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let tmp = dir.join(format!(".{name}.{}.{}.tmp", std::process::id(), nanos));
        let result = (|| -> io::Result<()> {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            let mut file = options.open(&tmp)?;
            file.write_all(bytes)?;
            file.flush()?;
            drop(file);
            #[cfg(unix)]
            if let Ok(metadata) = std::fs::metadata(path) {
                let permissions = metadata.permissions();
                if let Err(error) = std::fs::set_permissions(&tmp, permissions) {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(error);
                }
            }
            std::fs::rename(&tmp, path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    fn metadata(&self, path: &Path) -> io::Result<FsMetadata> {
        std::fs::metadata(path).map(FsMetadata::from)
    }

    fn symlink_metadata(&self, path: &Path) -> io::Result<FsMetadata> {
        std::fs::symlink_metadata(path).map(FsMetadata::from)
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        std::fs::canonicalize(path)
    }

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<FsDirEntry>> {
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let ft = entry.file_type()?;
            entries.push(FsDirEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                path: entry.path(),
                is_dir: ft.is_dir(),
                is_file: ft.is_file(),
                is_symlink: ft.is_symlink(),
            });
        }
        Ok(entries)
    }
}

/// Host local process implementation of [`ProcessService`].
#[derive(Clone, Copy, Debug, Default)]
pub struct LocalProcessService;

impl ProcessService for LocalProcessService {
    fn run_foreground(
        &self,
        state: SessionToolState,
        request: ProcessRunRequest,
    ) -> BoxFuture<'static, ForegroundOutcome> {
        Box::pin(async move {
            run_foreground_with_profile(
                &state,
                &request.workspace,
                &request.workdir,
                &request.command,
                &request.shell,
                request.timeout,
                request.sandbox_override,
                request.sandbox_profile,
                &request.additional_permissions,
            )
            .await
        })
    }

    fn spawn_background(
        &self,
        state: SessionToolState,
        request: ProcessBackgroundRequest,
    ) -> String {
        spawn_background_with_profile(
            &state,
            &request.workspace,
            &request.workdir,
            &request.command,
            &request.shell,
            request.sandbox_override,
            request.sandbox_profile,
            &request.additional_permissions,
        )
    }

    fn supports_pty(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Pure in-memory remote environment proving core file & shell tools
    /// operate cleanly against a non-local `ExecutionEnvironment`.
    struct MemoryRemoteFs {
        files: Mutex<HashMap<PathBuf, Vec<u8>>>,
        dirs: Mutex<std::collections::HashSet<PathBuf>>,
    }

    impl MemoryRemoteFs {
        fn new(root: &Path) -> Self {
            let mut dirs = std::collections::HashSet::new();
            dirs.insert(root.to_path_buf());
            Self {
                files: Mutex::new(HashMap::new()),
                dirs: Mutex::new(dirs),
            }
        }
    }

    impl ExecutorFileSystem for MemoryRemoteFs {
        fn read_bytes_bounded(&self, path: &Path, max_bytes: u64) -> io::Result<Vec<u8>> {
            let files = self.files.lock().unwrap();
            let data = files
                .get(path)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "not found"))?;
            let cap = (max_bytes as usize).saturating_add(1).min(data.len());
            Ok(data[..cap].to_vec())
        }

        fn read_all(&self, path: &Path) -> io::Result<Vec<u8>> {
            self.files
                .lock()
                .unwrap()
                .get(path)
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "not found"))
        }

        fn write_atomic(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
            self.files
                .lock()
                .unwrap()
                .insert(path.to_path_buf(), bytes.to_vec());
            Ok(())
        }

        fn metadata(&self, path: &Path) -> io::Result<FsMetadata> {
            if let Some(data) = self.files.lock().unwrap().get(path) {
                return Ok(FsMetadata {
                    len: data.len() as u64,
                    is_file: true,
                    is_dir: false,
                    is_symlink: false,
                    modified: None,
                });
            }
            if self.dirs.lock().unwrap().contains(path) {
                return Ok(FsMetadata {
                    len: 0,
                    is_file: false,
                    is_dir: true,
                    is_symlink: false,
                    modified: None,
                });
            }
            Err(io::Error::new(io::ErrorKind::NotFound, "not found"))
        }

        fn symlink_metadata(&self, path: &Path) -> io::Result<FsMetadata> {
            self.metadata(path)
        }

        fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
            self.metadata(path)?;
            Ok(path.to_path_buf())
        }

        fn create_dir_all(&self, path: &Path) -> io::Result<()> {
            self.dirs.lock().unwrap().insert(path.to_path_buf());
            Ok(())
        }

        fn remove_file(&self, path: &Path) -> io::Result<()> {
            self.files
                .lock()
                .unwrap()
                .remove(path)
                .map(|_| ())
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "not found"))
        }

        fn read_dir(&self, path: &Path) -> io::Result<Vec<FsDirEntry>> {
            let files = self.files.lock().unwrap();
            Ok(files
                .keys()
                .filter(|p| p.parent() == Some(path))
                .map(|p| FsDirEntry {
                    name: p.file_name().unwrap_or_default().to_string_lossy().into_owned(),
                    path: p.clone(),
                    is_dir: false,
                    is_file: true,
                    is_symlink: false,
                })
                .collect())
        }
    }

    struct MockRemoteProcess {
        commands: Mutex<Vec<(PathBuf, String)>>,
    }

    impl ProcessService for MockRemoteProcess {
        fn run_foreground(
            &self,
            _state: SessionToolState,
            request: ProcessRunRequest,
        ) -> BoxFuture<'static, ForegroundOutcome> {
            self.commands
                .lock()
                .unwrap()
                .push((request.workdir.clone(), request.command.clone()));
            let out = format!("remote:{}", request.command);
            Box::pin(async move {
                ForegroundOutcome::Completed {
                    output: out,
                    code: 0,
                    spill_path: None,
                }
            })
        }

        fn spawn_background(
            &self,
            _state: SessionToolState,
            request: ProcessBackgroundRequest,
        ) -> String {
            self.commands
                .lock()
                .unwrap()
                .push((request.workdir, request.command));
            "remote-bg-1".to_string()
        }
    }

    #[tokio::test]
    async fn custom_remote_environment_drives_read_write_edit_and_exec_without_local_fs() {
        let root = PathBuf::from(if cfg!(windows) {
            r"R:\remote-workspace"
        } else {
            "/remote-workspace"
        });
        let fs = Arc::new(MemoryRemoteFs::new(&root));
        let proc = Arc::new(MockRemoteProcess {
            commands: Mutex::new(Vec::new()),
        });
        let env = ExecutionEnvironment::custom(
            EnvironmentKind::Remote {
                label: "mock-ssh".into(),
            },
            fs.clone(),
            proc.clone(),
        );
        assert!(!env.is_local());

        let state = SessionToolState::for_test().with_environment(env);
        let mut tracker = crate::tool::ChangeTracker::default();

        // Write -> Read -> Edit through the remote ExecutorFileSystem
        let (out, is_err, ..) = crate::tool::execute(
            &pig_provider::ToolCall {
                id: "1".into(),
                name: "Write".into(),
                arguments: serde_json::json!({
                    "path": "hello.txt",
                    "content": "alpha\nbeta\n"
                })
                .to_string(),
            },
            crate::tool::ToolContext {
                cwd: &root,
                tracker: &mut tracker,
                state: &state,
                fs_grant: None,
                sandbox_override: Default::default(),
                sandbox_profile: SandboxProfile::WorkspaceWrite,
                additional_permissions: Default::default(),
            },
        )
        .await;
        assert!(!is_err, "remote Write failed: {out}");

        let (out, is_err, ..) = crate::tool::execute(
            &pig_provider::ToolCall {
                id: "2".into(),
                name: "Read".into(),
                arguments: serde_json::json!({ "path": "hello.txt" }).to_string(),
            },
            crate::tool::ToolContext {
                cwd: &root,
                tracker: &mut tracker,
                state: &state,
                fs_grant: None,
                sandbox_override: Default::default(),
                sandbox_profile: SandboxProfile::WorkspaceWrite,
                additional_permissions: Default::default(),
            },
        )
        .await;
        assert!(!is_err, "remote Read failed: {out}");
        assert!(out.contains("alpha"), "{out}");

        let (out, is_err, ..) = crate::tool::execute(
            &pig_provider::ToolCall {
                id: "3".into(),
                name: "Edit".into(),
                arguments: serde_json::json!({
                    "path": "hello.txt",
                    "old_string": "beta",
                    "new_string": "gamma"
                })
                .to_string(),
            },
            crate::tool::ToolContext {
                cwd: &root,
                tracker: &mut tracker,
                state: &state,
                fs_grant: None,
                sandbox_override: Default::default(),
                sandbox_profile: SandboxProfile::WorkspaceWrite,
                additional_permissions: Default::default(),
            },
        )
        .await;
        assert!(!is_err, "remote Edit failed: {out}");
        assert_eq!(
            String::from_utf8(fs.read_all(&root.join("hello.txt")).unwrap()).unwrap(),
            "alpha\ngamma\n"
        );

        // ExecCommand routes through the remote ProcessService
        let (out, is_err, ..) = crate::tool::execute(
            &pig_provider::ToolCall {
                id: "4".into(),
                name: crate::tool::EXEC_COMMAND_TOOL.into(),
                arguments: serde_json::json!({ "command": "uname -a" }).to_string(),
            },
            crate::tool::ToolContext {
                cwd: &root,
                tracker: &mut tracker,
                state: &state,
                fs_grant: None,
                sandbox_override: Default::default(),
                sandbox_profile: SandboxProfile::WorkspaceWrite,
                additional_permissions: Default::default(),
            },
        )
        .await;
        assert!(!is_err, "remote ExecCommand failed: {out}");
        assert!(out.contains("remote:uname -a"), "{out}");
        assert_eq!(proc.commands.lock().unwrap().len(), 1);
    }
}
