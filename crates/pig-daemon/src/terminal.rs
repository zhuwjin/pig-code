use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::Serialize;

const MAX_READ: usize = 64 * 1024;

#[derive(Clone, Debug, Serialize)]
pub struct TerminalInfo {
    pub terminal_id: String,
    pub shell: String,
    pub exited: bool,
}

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

/// Drops `CSI < u` (kitty keyboard query) while preserving split sequences and
/// ordinary `CSI u` cursor restore.
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

struct LiveTerminal {
    pty: Arc<SharedPty>,
    shell_name: String,
}

impl LiveTerminal {
    fn write(&self, text: &str) -> Result<(), String> {
        if self.pty.exited.load(Ordering::SeqCst) {
            return Err("terminal has exited".into());
        }
        let mut writer = self.pty.writer.lock().map_err(|error| error.to_string())?;
        writer
            .write_all(text.as_bytes())
            .map_err(|error| error.to_string())?;
        writer.flush().map_err(|error| error.to_string())
    }

    fn resize(&self, cols: u32, rows: u32) -> Result<(), String> {
        let size = pty_size(cols, rows);
        self.pty
            .master
            .lock()
            .map_err(|error| error.to_string())?
            .resize(size)
            .map_err(|error| error.to_string())
    }

    fn exited(&self) -> bool {
        self.pty.update_exit_status();
        self.pty.exited.load(Ordering::SeqCst)
    }

    fn close(&self) {
        self.pty.exited.store(true, Ordering::SeqCst);
        if let Ok(mut child) = self.pty.child.lock() {
            let _ = child.kill();
        }
    }
}

#[derive(Clone, Default)]
pub struct TerminalManager {
    terminals: Arc<Mutex<HashMap<String, Arc<LiveTerminal>>>>,
    next_id: Arc<AtomicU64>,
}

impl TerminalManager {
    pub fn open<FOut, FExit>(
        &self,
        cwd: &Path,
        shell: Option<&str>,
        on_output: FOut,
        on_exit: FExit,
    ) -> Result<TerminalInfo, String>
    where
        FOut: Fn(String, String) + Send + 'static,
        FExit: Fn(String) + Send + 'static,
    {
        let mut cmd = match shell.map(str::trim).filter(|s| !s.is_empty()) {
            Some(program) => {
                #[allow(unused_mut)]
                let mut cmd = CommandBuilder::new(program);
                #[cfg(unix)]
                cmd.arg("-l");
                cmd
            }
            None => default_shell_command(),
        };
        let shell_name = shell_name_of(&cmd);
        let pair = native_pty_system()
            .openpty(pty_size(80, 24))
            .map_err(|error| error.to_string())?;
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TERM_PROGRAM", "pig-code");
        #[cfg(unix)]
        cmd.env("PWD", cwd);
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|error| error.to_string())?;
        drop(pair.slave);
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| error.to_string())?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|error| error.to_string())?;
        let pty = Arc::new(SharedPty {
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            child: Mutex::new(child),
            exited: AtomicBool::new(false),
        });
        let id_num = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let terminal_id = format!("term-{id_num}");
        let live = Arc::new(LiveTerminal {
            pty: pty.clone(),
            shell_name: shell_name.clone(),
        });
        self.terminals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(terminal_id.clone(), live);
        spawn_reader(terminal_id.clone(), pty, reader, on_output, on_exit);
        Ok(TerminalInfo {
            terminal_id,
            shell: shell_name,
            exited: false,
        })
    }

    pub fn write(&self, terminal_id: &str, data: &str) -> Result<(), String> {
        let live = self
            .terminals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(terminal_id)
            .cloned()
            .ok_or_else(|| format!("terminal {terminal_id} not found"))?;
        live.write(data)
    }

    pub fn resize(&self, terminal_id: &str, cols: u32, rows: u32) -> Result<(), String> {
        let live = self
            .terminals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(terminal_id)
            .cloned()
            .ok_or_else(|| format!("terminal {terminal_id} not found"))?;
        live.resize(cols, rows)
    }

    pub fn close(&self, terminal_id: &str) -> bool {
        if let Some(live) = self
            .terminals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(terminal_id)
        {
            live.close();
            true
        } else {
            false
        }
    }

    pub fn list(&self) -> Vec<TerminalInfo> {
        let guard = self
            .terminals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut items: Vec<_> = guard
            .iter()
            .map(|(id, live)| TerminalInfo {
                terminal_id: id.clone(),
                shell: live.shell_name.clone(),
                exited: live.exited(),
            })
            .collect();
        items.sort_by(|left, right| left.terminal_id.cmp(&right.terminal_id));
        items
    }
}

fn spawn_reader<FOut, FExit>(
    terminal_id: String,
    pty: Arc<SharedPty>,
    mut reader: Box<dyn Read + Send>,
    on_output: FOut,
    on_exit: FExit,
) where
    FOut: Fn(String, String) + Send + 'static,
    FExit: Fn(String) + Send + 'static,
{
    let _ = std::thread::Builder::new()
        .name(format!("pig-daemon-{terminal_id}"))
        .spawn(move || {
            let mut buffer = [0u8; MAX_READ];
            let mut output = OutputBuffer::new();
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        output.push(&buffer[..count]);
                        loop {
                            let chunk = output.take(MAX_READ);
                            if chunk.is_empty() {
                                break;
                            }
                            on_output(terminal_id.clone(), chunk);
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            pty.exited.store(true, Ordering::SeqCst);
            on_exit(terminal_id);
        });
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

#[cfg(test)]
mod tests {
    use super::*;

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
