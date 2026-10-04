//! PTY 子进程管理：spawn / 读写 / resize / 退出检测。
//!
//! 参考 tty7 `crates/tty7-core/src/daemon/pane.rs:1736-1819` 的 spawn 片段
//! （openpty → spawn_command → drop slave → clone reader / take writer），
//! 砍掉 daemon 协议与重连逻辑，进程内直接持有 portable-pty。

use std::io::{Read, Write};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use portable_pty::{CommandBuilder, ExitStatus, MasterPty, PtySize, native_pty_system};

/// 一个活 PTY：master（resize）、writer（键盘输入）、child（退出检测与 kill）。
///
/// 全部经 Mutex 共享给 UI 线程（写键/resize）与 waiter 线程（try_wait）。
/// Drop 时 kill 子进程——关 tab 即杀 shell。
pub(crate) struct Pty {
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    child: Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
    /// 已被主动 kill（Drop / 关 tab）；waiter 线程见到后静默退出，不再上报
    killed: AtomicBool,
    /// tab 标签用的 shell 名（如 "zsh"）
    shell_name: String,
}

impl Pty {
    /// 开 PTY 并起 shell。`shell` 为 None 或空白时取系统默认（unix 登录 shell /
    /// Windows pwsh→powershell→cmd）；指定路径时以 login shell 方式启动（unix 追加 -l）。
    /// 返回 (Pty, reader)；reader 交给 term.rs 的读线程。
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

    /// 测试用：起指定程序而非登录 shell
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
        // 参考 tty7 pane.rs apply_common_command_setup 的环境注入（精简版）
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TERM_PROGRAM", "pig-code");
        #[cfg(unix)]
        cmd.env("PWD", cwd);

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(std::io::Error::other)?;
        // slave 端必须立刻丢掉，否则 shell 退出后 master 读不到 EOF
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

    /// 键盘输入写向 PTY。写失败（子进程已走）静默忽略——退出事件马上会到
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

    /// 非阻塞收割子进程；已退出返回其状态（portable-pty 自有 ExitStatus 类型）
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

/// tab 标签用的 shell 名：取 `resolve_login_shell()` 的 basename。
///
/// 标签与真实起进程结果保证一致：默认命令由 portable-pty `new_default_prog`
/// 解析，本函数与其内部完全同链（$SHELL 可执行校验 → passwd pw_shell 可执行
/// 校验 → /bin/sh），同一进程环境、同一校验口径，不会分叉
#[cfg(unix)]
fn login_shell_label() -> String {
    let shell = resolve_login_shell();
    std::path::Path::new(&shell)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "sh".into())
}

/// unix 默认 shell 解析：与 portable-pty `new_default_prog` 的 `get_shell()`
/// 完全同链——$SHELL（`access(X_OK)` 校验可执行）→ passwd 数据库 `getpwuid`
/// 的 `pw_shell`（同校验，macOS 走 OpenDirectory 用户记录）→ `/bin/sh`。
/// tty7 的 `login_shell()`（shells.rs:242）也是这条链
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
    // passwd 数据库；getpwuid 返回静态区指针，立即读取后不再触碰
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

/// unix：portable-pty 的 default prog = 登录 shell（$SHELL → passwd 回退），
/// spawn 时 argv[0] 自动加 `-` 前缀以 login shell 方式启动（参考 tty7
/// pane.rs:31-43 的 default_prog，完全同款）。
#[cfg(unix)]
fn default_shell_command() -> CommandBuilder {
    CommandBuilder::new_default_prog()
}

/// Windows：pwsh → powershell → cmd（参考 tty7 shells.rs:390 windows_default_shell）
#[cfg(windows)]
fn default_shell_command() -> CommandBuilder {
    for candidate in ["pwsh.exe", "powershell.exe"] {
        // 仅靠 PATH 解析（spawn 时由 portable-pty search_path 完成）；
        // 这里用 which 式探测：任一 ProgramFiles 下的 PowerShell 7 优先
        if which_on_path(candidate) {
            return CommandBuilder::new(candidate);
        }
    }
    CommandBuilder::new("cmd.exe")
}

/// 指定 shell 的自定义启动命令。unix 追加 `-l` 以 login shell 启动（zsh/bash/
/// fish/sh/nu 均接受；默认分支 new_default_prog 的 argv[0] `-` 前缀是
/// portable-pty 内部行为，自定义路径够不到，故用显式参数）
#[cfg(unix)]
fn custom_shell_command(prog: &str) -> CommandBuilder {
    let mut cmd = CommandBuilder::new(prog);
    cmd.arg("-l");
    cmd
}

/// Windows 自定义 shell 直接启动（不同终端程序参数各异，不盲加）
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

/// shell 名（tab 标签）：自定义命令取程序 basename（Windows 去 .exe）；
/// 默认命令 argv 为空（程序在 spawn 时才解析），unix 回退 $SHELL 标签
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

    /// 自定义 shell：标签取程序 basename；空白配置回退系统默认 shell
    #[test]
    fn 自定义shell标签与空白回退() {
        let dir = std::env::temp_dir();
        let (pty, _reader) = Pty::spawn(&dir, test_size(), Some("sh")).expect("spawn sh");
        assert_eq!(pty.shell_name(), "sh");

        let (pty2, _r2) = Pty::spawn(&dir, test_size(), Some("   ")).expect("空白回退默认 shell");
        assert!(!pty2.shell_name().is_empty());
        #[cfg(unix)]
        assert_eq!(pty2.shell_name(), login_shell_label());
    }

    /// 默认 shell 解析与 portable-pty 同链：$SHELL 有效时必须等于 $SHELL；
    /// 结果始终是可执行文件路径（passwd 回退或 /bin/sh）
    #[cfg(unix)]
    #[test]
    fn 默认shell解析与环境一致() {
        let resolved = resolve_login_shell();
        assert!(!resolved.is_empty());
        let c = std::ffi::CString::new(resolved.as_str()).unwrap();
        assert!(
            unsafe { libc::access(c.as_ptr(), libc::X_OK) } == 0,
            "解析结果应可执行：{resolved}"
        );
        if let Ok(shell) = std::env::var("SHELL") {
            let sc = std::ffi::CString::new(shell.clone()).unwrap();
            if unsafe { libc::access(sc.as_ptr(), libc::X_OK) } == 0 {
                assert_eq!(resolved, shell, "$SHELL 有效时应优先于 passwd 回退");
            }
        }
        // 标签与解析结果的 basename 一致
        let label = login_shell_label();
        assert_eq!(
            label,
            std::path::Path::new(&resolved)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap()
        );
    }

    /// 自定义 shell 以 login 方式（-l）启动且可用：写 echo 读回显
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
            "没等到自定义 shell 的回显：{}",
            String::from_utf8_lossy(&out)
        );
    }
}
