use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use nsh_core::prelude::*;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};

const MAX_READ: usize = 64 * 1024;

struct OutputBuffer {
    text: String,
    pending: Vec<u8>,
    filter_carry: String,
}

impl OutputBuffer {
    fn new() -> Self {
        Self {
            text: String::new(),
            pending: Vec::new(),
            filter_carry: String::new(),
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        let mut data = std::mem::take(&mut self.pending);
        data.extend_from_slice(bytes);
        let mut start = 0;
        while start < data.len() {
            match std::str::from_utf8(&data[start..]) {
                Ok(text) => {
                    self.text.push_str(text);
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    if valid > 0 {
                        if let Ok(text) = std::str::from_utf8(&data[start..start + valid]) {
                            self.text.push_str(text);
                        }
                        start += valid;
                    }
                    match error.error_len() {
                        Some(length) => {
                            self.text.push('\u{FFFD}');
                            start += length;
                        }
                        None => {
                            self.pending.extend_from_slice(&data[start..]);
                            break;
                        }
                    }
                }
            }
        }
    }

    fn take(&mut self, max_bytes: usize) -> String {
        if self.text.is_empty() {
            return String::new();
        }
        let mut end = self.text.len().min(max_bytes);
        while end > 0 && !self.text.is_char_boundary(end) {
            end -= 1;
        }
        if end == 0 {
            return String::new();
        }
        let head: String = self.text.drain(..end).collect();
        let combined = std::mem::take(&mut self.filter_carry) + &head;
        let (filtered, carry) = filter_kitty_keyboard(&combined);
        self.filter_carry = carry;
        filtered
    }
}

/// NSH 0.6.0's VT treats `CSI < u` (kitty keyboard query) as `CSI u` (restore cursor).
/// Drop the query until the emulator checks its intermediates, keeping a split
/// sequence for the next read.
fn filter_kitty_keyboard(text: &str) -> (String, String) {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != 0x1b {
            let character = text[index..].chars().next().expect("character");
            out.push(character);
            index += character.len_utf8();
            continue;
        }
        if index + 1 >= bytes.len() {
            return (out, text[index..].to_string());
        }
        if bytes[index + 1] != b'[' {
            out.push('\x1b');
            index += 1;
            continue;
        }
        let mut end = index + 2;
        while end < bytes.len() && !(0x40..=0x7e).contains(&bytes[end]) {
            end += 1;
        }
        if end >= bytes.len() {
            return (out, text[index..].to_string());
        }
        let intermediate = bytes[index + 2..end]
            .iter()
            .any(|byte| matches!(byte, b'<' | b'>' | b'='));
        if bytes[end] == b'u' && intermediate {
            index = end + 1;
        } else {
            out.push_str(&text[index..=end]);
            index = end + 1;
        }
    }
    (out, String::new())
}

struct SharedPty {
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    child: Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
    exited: AtomicBool,
    output: Mutex<OutputBuffer>,
}

impl SharedPty {
    fn update_exit_status(&self) {
        let exited = self
            .child
            .lock()
            .ok()
            .and_then(|mut child| child.try_wait().ok().flatten())
            .is_some();
        if exited {
            self.exited.store(true, Ordering::SeqCst);
        }
    }
}

impl Drop for SharedPty {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
        }
    }
}

/// A live shell process connected to a pseudo-terminal.
#[class]
pub struct Terminal {
    pty: Arc<SharedPty>,
    shell_name: String,
}

#[class]
impl Terminal {
    /// Starts a login shell in `cwd`; an empty `shell` uses the system default.
    pub fn new(cwd: String, shell: Option<String>) -> Result<Self, String> {
        let cwd = PathBuf::from(cwd);
        let mut cmd = match shell.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(program) => {
                #[allow(unused_mut)]
                let mut cmd = CommandBuilder::new(program);
                #[cfg(unix)]
                cmd.arg("-l");
                cmd
            }
            None => default_shell_command(),
        };
        Self::spawn_inner(&mut cmd, &cwd).map_err(|error| error.to_string())
    }

    /// The shell's name for the tab label.
    pub fn shell(&self) -> String {
        self.shell_name.clone()
    }

    /// The program's output bytes since the last read, as UTF-8 text.
    pub fn read(&self) -> String {
        match self.pty.output.lock() {
            Ok(mut output) => output.take(MAX_READ),
            Err(_) => String::new(),
        }
    }

    /// Writes keyboard input, pasted bytes or the emulator's replies to the shell.
    pub fn write(&self, text: String) -> Result<(), String> {
        if self.pty.exited.load(Ordering::SeqCst) {
            return Err("terminal has exited".into());
        }
        let mut writer = self.pty.writer.lock().map_err(|error| error.to_string())?;
        writer
            .write_all(text.as_bytes())
            .map_err(|error| error.to_string())?;
        writer.flush().map_err(|error| error.to_string())
    }

    /// Resizes the pseudo-terminal's window in cells.
    pub fn resize(&self, cols: u32, rows: u32) -> Result<(), String> {
        let size = pty_size(cols, rows);
        self.pty
            .master
            .lock()
            .map_err(|error| error.to_string())?
            .resize(size)
            .map_err(|error| error.to_string())
    }

    /// Whether the shell has exited.
    pub fn exited(&self) -> bool {
        self.pty.update_exit_status();
        self.pty.exited.load(Ordering::SeqCst)
    }

    /// Kills the shell and releases the pseudo-terminal.
    pub fn close(&self) {
        self.pty.exited.store(true, Ordering::SeqCst);
        if let Ok(mut child) = self.pty.child.lock() {
            let _ = child.kill();
        }
    }
}

impl Terminal {
    fn spawn_inner(cmd: &mut CommandBuilder, cwd: &Path) -> std::io::Result<Self> {
        let shell_name = shell_name_of(cmd);
        let pair = native_pty_system()
            .openpty(pty_size(80, 24))
            .map_err(std::io::Error::other)?;
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TERM_PROGRAM", "pig-code");
        #[cfg(unix)]
        cmd.env("PWD", cwd);
        let child = pair
            .slave
            .spawn_command(cmd.clone())
            .map_err(std::io::Error::other)?;
        drop(pair.slave);
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(std::io::Error::other)?;
        let writer = pair.master.take_writer().map_err(std::io::Error::other)?;
        let pty = Arc::new(SharedPty {
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            child: Mutex::new(child),
            exited: AtomicBool::new(false),
            output: Mutex::new(OutputBuffer::new()),
        });
        spawn_reader(pty.clone(), reader);
        Ok(Self { pty, shell_name })
    }

    #[cfg(all(test, unix))]
    fn spawn_program(program: &str, args: &[&str]) -> std::io::Result<Self> {
        let mut cmd = CommandBuilder::new(program);
        cmd.args(args);
        Self::spawn_inner(&mut cmd, &std::env::temp_dir())
    }
}

fn shell_name_of(cmd: &CommandBuilder) -> String {
    if let Some(prog) = cmd.get_argv().first() {
        let prog = prog.to_string_lossy().into_owned();
        let base = Path::new(&prog)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
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
        std::env::var("SHELL")
            .ok()
            .and_then(|shell| {
                Path::new(&shell)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "sh".into())
    }
    #[cfg(windows)]
    {
        "shell".into()
    }
}

fn default_shell_command() -> CommandBuilder {
    #[cfg(unix)]
    {
        CommandBuilder::new_default_prog()
    }
    #[cfg(windows)]
    {
        for candidate in ["pwsh.exe", "powershell.exe"] {
            if std::env::var_os("PATH")
                .map(|paths| {
                    std::env::split_paths(&paths)
                        .any(|directory| directory.join(candidate).is_file())
                })
                .unwrap_or(false)
            {
                return CommandBuilder::new(candidate);
            }
        }
        CommandBuilder::new("cmd.exe")
    }
}

fn pty_size(cols: u32, rows: u32) -> PtySize {
    PtySize {
        rows: rows.clamp(1, u16::MAX as u32) as u16,
        cols: cols.clamp(1, u16::MAX as u32) as u16,
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn spawn_reader(pty: Arc<SharedPty>, mut reader: Box<dyn Read + Send>) {
    std::thread::Builder::new()
        .name("pig-terminal-reader".into())
        .spawn(move || {
            let mut buffer = [0u8; 64 * 1024];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        if let Ok(mut output) = pty.output.lock() {
                            output.push(&buffer[..count]);
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            pty.exited.store(true, Ordering::SeqCst);
        })
        .expect("spawn terminal reader");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn read_until(terminal: &Terminal, needle: &str) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut text = String::new();
        while std::time::Instant::now() < deadline {
            text.push_str(&terminal.read());
            if text.contains(needle) {
                return text;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("terminal output did not contain {needle:?}: {text}");
    }

    #[cfg(unix)]
    #[test]
    fn real_pty_output_input_and_exit_are_visible() {
        let terminal = Terminal::spawn_program(
            "sh",
            &[
                "-c",
                "printf 'pig-terminal-ready开发\\n'; read line; printf 'got:%s\\n' \"$line\"",
            ],
        )
        .expect("spawn test shell");
        read_until(&terminal, "pig-terminal-ready开发");
        terminal.write("hello-pty\r".into()).expect("write input");
        read_until(&terminal, "got:hello-pty");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !terminal.exited() {
            assert!(
                std::time::Instant::now() < deadline,
                "terminal did not report its exit"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn utf8_split_across_reads_is_kept() {
        let mut output = OutputBuffer::new();
        let bytes = "开发".as_bytes();
        output.push(&bytes[..2]);
        assert!(output.take(MAX_READ).is_empty());
        output.push(&bytes[2..]);
        assert_eq!(output.take(MAX_READ), "开发");
    }

    #[test]
    fn read_limit_stops_on_a_character_boundary() {
        let mut output = OutputBuffer::new();
        output.push("ab开发cd".as_bytes());
        let first = output.take(4);
        assert_eq!(first, "ab");
        assert_eq!(output.take(MAX_READ), "开发cd");
    }

    #[test]
    fn kitty_keyboard_query_is_not_a_cursor_restore() {
        let mut output = OutputBuffer::new();
        output.push(b"ab\x1b[<ucd");
        assert_eq!(output.take(MAX_READ), "abcd");
    }

    #[test]
    fn kitty_keyboard_query_split_across_reads_is_removed() {
        let mut output = OutputBuffer::new();
        output.push(b"ab\x1b[<");
        assert_eq!(output.take(MAX_READ), "ab");
        output.push(b"ucd");
        assert_eq!(output.take(MAX_READ), "cd");
    }

    #[test]
    fn cursor_restore_without_intermediates_is_kept() {
        let mut output = OutputBuffer::new();
        output.push(b"ab\x1b[ucd");
        assert_eq!(output.take(MAX_READ), "ab\x1b[ucd");
    }
}
