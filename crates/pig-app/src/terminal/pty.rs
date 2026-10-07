//! PTY child process management: spawn / read / write / resize / exit detection.
//!
//! Modeled on the spawn snippet of tty7
//! `crates/tty7-core/src/daemon/pane.rs:1736-1819`
//! (openpty → spawn_command → drop slave → clone reader / take writer), dropping
//! the daemon protocol and reconnection logic; portable-pty is held directly
//! in-process.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use portable_pty::{CommandBuilder, ExitStatus, MasterPty, PtySize, native_pty_system};

/// One live PTY: master (resize), writer (keyboard input), child (exit detection
/// and kill).
///
/// All of it is shared via Mutexes with the UI thread (writing keys / resize)
/// and the waiter thread (try_wait). Drop kills the child process: closing a
/// tab kills the shell.
pub(crate) struct Pty {
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    child: Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
    /// Killed deliberately (Drop / closing a tab); the waiter thread exits silently upon seeing it and reports nothing further
    killed: AtomicBool,
    /// Shell name used for the tab label (e.g. "zsh")
    shell_name: String,
}

impl Pty {
    /// Open a PTY and start a shell. A None or blank `shell` takes the system
    /// default (unix login shell / Windows pwsh→powershell→cmd); a given path is
    /// started as a login shell (unix appends -l).
    /// Returns (Pty, reader); the reader is handed to the reader thread in term.rs.
    pub(crate) fn spawn(
        cwd: &Path,
        size: PtySize,
        shell: Option<&str>,
    ) -> std::io::Result<(std::sync::Arc<Self>, Box<dyn Read + Send>)> {
        let custom = shell.map(str::trim).filter(|s| !s.is_empty());
        let cmd = match custom {
            Some(prog) => custom_shell_command(prog),
            None => default_shell_command(),
        };
        Self::spawn_inner(cmd, cwd, size, None)
    }

    /// For tests: start the given program instead of a login shell
    #[cfg(test)]
    pub(crate) fn spawn_program(
        cwd: &Path,
        size: PtySize,
        program: &str,
        args: &[&str],
    ) -> std::io::Result<(std::sync::Arc<Self>, Box<dyn Read + Send>)> {
        let mut cmd = CommandBuilder::new(program);
        cmd.args(args);
        Self::spawn_inner(cmd, cwd, size, Some(program.to_string()))
    }

    fn spawn_inner(
        mut cmd: CommandBuilder,
        cwd: &Path,
        size: PtySize,
        shell_name: Option<String>,
    ) -> std::io::Result<(std::sync::Arc<Self>, Box<dyn Read + Send>)> {
        let shell_name = shell_name.unwrap_or_else(|| shell_name_of(&cmd));
        let pair = native_pty_system()
            .openpty(size)
            .map_err(std::io::Error::other)?;

        cmd.cwd(cwd);
        // Environment injection per apply_common_command_setup in tty7 pane.rs (trimmed)
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TERM_PROGRAM", "pig-code");
        #[cfg(unix)]
        cmd.env("PWD", cwd);

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(std::io::Error::other)?;
        // The slave end must be dropped immediately, otherwise the master cannot read EOF after the shell exits
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(std::io::Error::other)?;
        let writer = pair.master.take_writer().map_err(std::io::Error::other)?;

        Ok((
            std::sync::Arc::new(Self {
                master: Mutex::new(pair.master),
                writer: Mutex::new(writer),
                child: Mutex::new(child),
                killed: AtomicBool::new(false),
                shell_name,
            }),
            reader,
        ))
    }

    pub(crate) fn shell_name(&self) -> &str {
        &self.shell_name
    }

    /// Write keyboard input to the PTY. A failed write (child already gone) is silently ignored; the exit event arrives right away
    pub(crate) fn write(&self, bytes: &[u8]) {
        if let Ok(mut w) = self.writer.lock() {
            let _ = w.write_all(bytes);
            let _ = w.flush();
        }
    }

    pub(crate) fn resize(&self, size: PtySize) {
        if let Ok(master) = self.master.lock() {
            let _ = master.resize(size);
        }
    }

    /// Reap the child non-blockingly; returns its status once exited (portable-pty's own ExitStatus type)
    pub(crate) fn try_wait(&self) -> Option<ExitStatus> {
        self.child.lock().ok()?.try_wait().ok().flatten()
    }

    pub(crate) fn killed(&self) -> bool {
        self.killed.load(Ordering::SeqCst)
    }

    fn kill(&self) {
        self.killed.store(true, Ordering::SeqCst);
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
        }
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Shell name for the tab label: the basename of `resolve_login_shell()`.
///
/// The label is guaranteed to match what actually gets spawned: the default
/// command is resolved by portable-pty `new_default_prog`, and this function
/// follows exactly the same chain as its internals ($SHELL executable check →
/// passwd pw_shell executable check → /bin/sh), same process environment and
/// same validation criteria, so they cannot diverge
#[cfg(unix)]
fn login_shell_label() -> String {
    let shell = resolve_login_shell();
    std::path::Path::new(&shell)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "sh".into())
}

/// Unix default shell resolution: exactly the same chain as portable-pty
/// `new_default_prog`'s `get_shell()`: $SHELL (validated executable via
/// `access(X_OK)`) → the passwd database `getpwuid`'s `pw_shell` (same
/// validation; macOS goes through OpenDirectory user records) → `/bin/sh`.
/// tty7's `login_shell()` (shells.rs:242) uses this chain too
#[cfg(unix)]
fn resolve_login_shell() -> String {
    fn executable(path: &str) -> bool {
        let Ok(c) = std::ffi::CString::new(path) else {
            return false;
        };
        unsafe { libc::access(c.as_ptr(), libc::X_OK) == 0 }
    }
    if let Ok(shell) = std::env::var("SHELL")
        && !shell.is_empty()
        && executable(&shell)
    {
        return shell;
    }
    // passwd database; getpwuid returns a pointer into static storage, read it
    // immediately and never touch it again
    unsafe {
        let ent = libc::getpwuid(libc::getuid());
        if !ent.is_null()
            && let Ok(shell) = std::ffi::CStr::from_ptr((*ent).pw_shell).to_str()
            && !shell.is_empty()
            && executable(shell)
        {
            return shell.to_string();
        }
    }
    "/bin/sh".into()
}

/// Unix: portable-pty's default prog = login shell ($SHELL → passwd fallback);
/// at spawn, argv[0] automatically gets a `-` prefix so it starts as a login
/// shell (see the default_prog at tty7 pane.rs:31-43, exactly the same).
#[cfg(unix)]
fn default_shell_command() -> CommandBuilder {
    CommandBuilder::new_default_prog()
}

/// Windows: pwsh → powershell → cmd (see windows_default_shell in tty7 shells.rs:390)
#[cfg(windows)]
fn default_shell_command() -> CommandBuilder {
    for candidate in ["pwsh.exe", "powershell.exe"] {
        // Resolution relies purely on PATH (done by portable-pty search_path at
        // spawn); here we probe which-style: any PowerShell 7 under ProgramFiles
        // wins
        if which_on_path(candidate) {
            return CommandBuilder::new(candidate);
        }
    }
    CommandBuilder::new("cmd.exe")
}

/// Custom launch command for a specified shell. Unix appends `-l` to start a
/// login shell (accepted by zsh/bash/fish/sh/nu alike); the default branch's
/// new_default_prog argv[0] `-` prefix is internal portable-pty behavior that a
/// custom path cannot reach, hence the explicit argument
#[cfg(unix)]
fn custom_shell_command(prog: &str) -> CommandBuilder {
    let mut cmd = CommandBuilder::new(prog);
    cmd.arg("-l");
    cmd
}

/// Windows custom shells launch directly (terminal programs take different arguments; do not add blindly)
#[cfg(windows)]
fn custom_shell_command(prog: &str) -> CommandBuilder {
    CommandBuilder::new(prog)
}

#[cfg(windows)]
fn which_on_path(program: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| dir.join(program).is_file())
}

/// Shell name (tab label): for a custom command, the program basename (.exe
/// stripped on Windows); the default command has an empty argv (the program is
/// only resolved at spawn), so unix falls back to the $SHELL label
fn shell_name_of(cmd: &CommandBuilder) -> String {
    if let Some(prog) = cmd.get_argv().first() {
        let prog = prog.to_string_lossy().into_owned();
        let base = std::path::Path::new(&prog)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or(prog);
        #[cfg(windows)]
        let base = base
            .strip_suffix(".exe")
            .map(str::to_string)
            .unwrap_or(base);
        return base;
    }
    #[cfg(unix)]
    {
        login_shell_label()
    }
    #[cfg(windows)]
    {
        "shell".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_size() -> PtySize {
        PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        }
    }

    /// Custom shell: the label takes the program basename; blank config falls back to the system default shell
    #[test]
    fn 自定义shell标签与空白回退() {
        let dir = std::env::temp_dir();
        let (pty, _reader) = Pty::spawn(&dir, test_size(), Some("sh")).expect("spawn sh");
        assert_eq!(pty.shell_name(), "sh");

        let (pty2, _r2) = Pty::spawn(&dir, test_size(), Some("   "))
            .expect("blank value falls back to the default shell");
        assert!(!pty2.shell_name().is_empty());
        #[cfg(unix)]
        assert_eq!(pty2.shell_name(), login_shell_label());
    }

    /// Default shell resolution matches portable-pty's chain: must equal $SHELL
    /// when $SHELL is valid; the result is always an executable file path
    /// (passwd fallback or /bin/sh)
    #[cfg(unix)]
    #[test]
    fn 默认shell解析与环境一致() {
        let resolved = resolve_login_shell();
        assert!(!resolved.is_empty());
        let c = std::ffi::CString::new(resolved.as_str()).unwrap();
        assert!(
            unsafe { libc::access(c.as_ptr(), libc::X_OK) } == 0,
            "resolved shell should be executable: {resolved}"
        );
        if let Ok(shell) = std::env::var("SHELL") {
            let sc = std::ffi::CString::new(shell.clone()).unwrap();
            if unsafe { libc::access(sc.as_ptr(), libc::X_OK) } == 0 {
                assert_eq!(
                    resolved, shell,
                    "$SHELL should take precedence over the passwd fallback when valid"
                );
            }
        }
        // The label matches the basename of the resolved result
        let label = login_shell_label();
        assert_eq!(
            label,
            std::path::Path::new(&resolved)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap()
        );
    }

    /// Custom shell starts as a login shell (-l) and works: write echo, read the echo back
    #[cfg(unix)]
    #[test]
    fn 自定义shell可执行并回显() {
        let dir = std::env::temp_dir();
        let (pty, mut reader) = Pty::spawn(&dir, test_size(), Some("sh")).expect("spawn sh");
        pty.write(b"echo pig-custom-shell\n");
        let mut buf = [0u8; 4096];
        let mut out = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    out.extend_from_slice(&buf[..n]);
                    if String::from_utf8_lossy(&out).contains("pig-custom-shell") {
                        return;
                    }
                }
                Err(_) => break,
            }
        }
        panic!(
            "did not see echo from the custom shell: {}",
            String::from_utf8_lossy(&out)
        );
    }
}
