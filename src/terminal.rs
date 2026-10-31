use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event as AlacEvent, EventListener};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi;
use nsh_core::prelude::*;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TermSize {
    cols: usize,
    rows: usize,
}

impl TermSize {
    fn new(cols: usize, rows: usize) -> Self {
        Self {
            cols: cols.max(1),
            rows: rows.max(1),
        }
    }
}

impl alacritty_terminal::grid::Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

#[derive(Clone)]
struct EventProxy {
    tx: std::sync::mpsc::Sender<AlacEvent>,
}

impl EventListener for EventProxy {
    fn send_event(&self, event: AlacEvent) {
        let _ = self.tx.send(event);
    }
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

/// A live shell parsed into an Alacritty terminal grid.
#[class]
pub struct Terminal {
    pty: Arc<SharedPty>,
    term: Arc<FairMutex<Term<EventProxy>>>,
    size: Mutex<TermSize>,
    _events: std::sync::mpsc::Receiver<AlacEvent>,
}

#[class]
impl Terminal {
    /// Starts a login shell in `cwd`; an empty `shell` uses the system default.
    pub fn new(cwd: String, shell: Option<String>) -> Result<Self, String> {
        let cwd = PathBuf::from(cwd);
        let mut cmd = match shell.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(program) => {
                let mut cmd = CommandBuilder::new(program);
                #[cfg(unix)]
                cmd.arg("-l");
                cmd
            }
            None => default_shell_command(),
        };
        Self::spawn_inner(&mut cmd, &cwd).map_err(|error| error.to_string())
    }

    /// Writes keyboard input or pasted bytes to the shell.
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

    /// Resizes both the terminal grid and the PTY.
    pub fn resize(&self, cols: u32, rows: u32) -> Result<(), String> {
        let size = TermSize::new(cols as usize, rows as usize);
        {
            let mut current = self.size.lock().map_err(|error| error.to_string())?;
            if *current == size {
                return Ok(());
            }
            *current = size;
        }
        self.term.lock().resize(size);
        self.pty
            .master
            .lock()
            .map_err(|error| error.to_string())?
            .resize(pty_size(size))
            .map_err(|error| error.to_string())
    }

    /// Returns the current text grid and process state as JSON.
    pub fn snapshot(&self) -> String {
        self.pty.update_exit_status();
        serde_json::json!({
            "text": self.snapshot_text(),
            "exited": self.pty.exited.load(Ordering::SeqCst),
        })
        .to_string()
    }
}

impl Terminal {
    fn spawn_inner(cmd: &mut CommandBuilder, cwd: &Path) -> std::io::Result<Self> {
        let size = TermSize::new(80, 24);
        let pair = native_pty_system()
            .openpty(pty_size(size))
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
        });
        let (tx, rx) = std::sync::mpsc::channel();
        let proxy = EventProxy { tx };
        let config = Config {
            scrolling_history: 10_000,
            kitty_keyboard: true,
            ..Default::default()
        };
        let term = Arc::new(FairMutex::new(Term::new(config, &size, proxy.clone())));
        spawn_reader(term.clone(), proxy, pty.clone(), reader);
        Ok(Self {
            pty,
            term,
            size: Mutex::new(size),
            _events: rx,
        })
    }

    fn snapshot_text(&self) -> String {
        let size = *self.size.lock().unwrap();
        let term = self.term.lock();
        let content = term.renderable_content();
        let mut grid = vec![vec![' '; size.cols]; size.rows];
        let offset = content.display_offset as i32;
        for cell in content.display_iter {
            let row = cell.point.line.0 + offset;
            let col = cell.point.column.0;
            if row >= 0 && (row as usize) < size.rows && col < size.cols {
                grid[row as usize][col] = cell.c;
            }
        }
        let mut lines: Vec<String> = grid
            .into_iter()
            .map(|row| row.into_iter().collect::<String>().trim_end().to_owned())
            .collect();
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        lines.join("\n")
    }

    #[cfg(all(test, unix))]
    fn spawn_program(program: &str, args: &[&str]) -> std::io::Result<Self> {
        let mut cmd = CommandBuilder::new(program);
        cmd.args(args);
        Self::spawn_inner(&mut cmd, &std::env::temp_dir())
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

fn pty_size(size: TermSize) -> PtySize {
    PtySize {
        rows: size.rows as u16,
        cols: size.cols as u16,
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn spawn_reader(
    term: Arc<FairMutex<Term<EventProxy>>>,
    proxy: EventProxy,
    pty: Arc<SharedPty>,
    mut reader: Box<dyn Read + Send>,
) {
    std::thread::Builder::new()
        .name("pig-terminal-reader".into())
        .spawn(move || {
            let mut processor: ansi::Processor = ansi::Processor::new();
            let mut buffer = [0u8; 64 * 1024];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        processor.advance(&mut *term.lock(), &buffer[..count]);
                        proxy.send_event(AlacEvent::Wakeup);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            pty.exited.store(true, Ordering::SeqCst);
            proxy.send_event(AlacEvent::Exit);
        })
        .expect("spawn terminal reader");
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn real_pty_output_input_and_exit_are_visible() {
        let terminal = Terminal::spawn_program(
            "sh",
            &[
                "-c",
                "printf 'pig-terminal-ready\\n'; read line; printf 'got:%s\\n' \"$line\"",
            ],
        )
        .expect("spawn test shell");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if terminal.snapshot_text().contains("pig-terminal-ready") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(terminal.snapshot_text().contains("pig-terminal-ready"));
        terminal.write("hello-pty\r".into()).expect("write input");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            terminal.pty.update_exit_status();
            let text = terminal.snapshot_text();
            if text.contains("got:hello-pty") && terminal.pty.exited.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!(
            "terminal did not process input and exit: {}",
            terminal.snapshot_text()
        );
    }
}
