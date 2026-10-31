use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event as AlacEvent, EventListener};
use alacritty_terminal::grid::{Dimensions as _, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
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
    shell_name: String,
    selecting: AtomicBool,
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
        {
            let mut term = self.term.lock();
            term.selection = None;
            term.scroll_display(Scroll::Bottom);
        }
        self.selecting.store(false, Ordering::SeqCst);
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

    /// Starts or extends an Alacritty selection and returns its visible range as JSON.
    pub fn select_start(&self, col: u32, row: u32, left: bool, clicks: u32, shift: bool) -> String {
        let mut term = self.term.lock();
        let point = selection_point(&term, col, row);
        let side = if left { Side::Left } else { Side::Right };
        if shift && clicks == 1 && term.selection.is_some() {
            if let Some(selection) = term.selection.as_mut() {
                selection.update(point, side);
            }
        } else {
            let kind = match clicks {
                2 => SelectionType::Semantic,
                n if n >= 3 => SelectionType::Lines,
                _ => SelectionType::Simple,
            };
            term.selection = Some(Selection::new(kind, point, side));
        }
        self.selecting.store(true, Ordering::SeqCst);
        selection_json(&term).to_string()
    }

    /// Updates the active Alacritty selection and returns its visible range as JSON.
    pub fn select_update(&self, col: u32, row: u32, left: bool) -> String {
        let mut term = self.term.lock();
        if self.selecting.load(Ordering::SeqCst) {
            let point = selection_point(&term, col, row);
            let side = if left { Side::Left } else { Side::Right };
            if let Some(selection) = term.selection.as_mut() {
                selection.update(point, side);
            }
        }
        selection_json(&term).to_string()
    }

    /// Finishes the active selection without discarding it.
    pub fn select_end(&self) {
        self.selecting.store(false, Ordering::SeqCst);
    }

    /// Clears the current selection.
    pub fn clear_selection(&self) {
        self.term.lock().selection = None;
        self.selecting.store(false, Ordering::SeqCst);
    }

    /// Selects the complete scrollback and visible grid.
    pub fn select_all(&self) -> String {
        let mut term = self.term.lock();
        let grid = term.grid();
        let start = Point::new(grid.topmost_line(), Column(0));
        let end = Point::new(grid.bottommost_line(), grid.last_column());
        let mut selection = Selection::new(SelectionType::Simple, start, Side::Left);
        selection.update(end, Side::Right);
        term.selection = Some(selection);
        selection_json(&term).to_string()
    }

    /// Returns the selected terminal text, or null when the selection is empty.
    pub fn copy_selection(&self) -> Option<String> {
        self.term.lock().selection_to_string()
    }

    /// Returns the text grid, cursor, shell, selection and process state as JSON.
    pub fn snapshot(&self) -> String {
        self.pty.update_exit_status();
        let (text, lines, cursor, hidden, selection) = self.snapshot_grid();
        serde_json::json!({
            "text": text,
            "lines": lines,
            "cursor": cursor.filter(|_| !hidden),
            "selection": selection,
            "exited": self.pty.exited.load(Ordering::SeqCst),
            "shell": self.shell_name,
        })
        .to_string()
    }
}

#[derive(serde::Serialize)]
struct CursorSnapshot {
    row: usize,
    col: usize,
    text: String,
    shape: &'static str,
}

#[derive(serde::Serialize)]
struct SnapshotSpan {
    text: String,
    col: usize,
    cells: usize,
    fg: Option<serde_json::Value>,
    bg: Option<serde_json::Value>,
    dim: bool,
    bold: bool,
    inverse: bool,
}

#[derive(serde::Serialize)]
struct SnapshotLine {
    spans: Vec<SnapshotSpan>,
    columns: String,
}

fn selection_point(term: &Term<EventProxy>, col: u32, row: u32) -> Point {
    let grid = term.grid();
    let col = (col as usize).min(grid.columns().saturating_sub(1));
    let row = (row as usize).min(grid.screen_lines().saturating_sub(1));
    Point::new(Line(grid.topmost_line().0 + row as i32), Column(col))
}

fn selection_json(term: &Term<EventProxy>) -> serde_json::Value {
    if !term
        .selection
        .as_ref()
        .is_some_and(|selection| !selection.is_empty())
    {
        return serde_json::Value::Null;
    }
    let Some(range) = term.renderable_content().selection else {
        return serde_json::Value::Null;
    };
    let top = term.grid().topmost_line().0;
    let start_row = range.start.line.0 - top;
    let end_row = range.end.line.0 - top;
    if start_row < 0 || end_row < 0 {
        return serde_json::Value::Null;
    }
    serde_json::json!({
        "start": { "row": start_row, "col": range.start.column.0 },
        "end": { "row": end_row, "col": range.end.column.0 + 1 },
    })
}

impl Terminal {
    fn spawn_inner(cmd: &mut CommandBuilder, cwd: &Path) -> std::io::Result<Self> {
        let shell_name = shell_name_of(cmd);
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
            shell_name,
            selecting: AtomicBool::new(false),
            _events: rx,
        })
    }

    fn ansi_value(color: ansi::Color) -> Option<serde_json::Value> {
        match color {
            ansi::Color::Spec(rgb) => {
                Some(format!("#{:02x}{:02x}{:02x}", rgb.r, rgb.g, rgb.b).into())
            }
            ansi::Color::Indexed(index) => Some(index.into()),
            ansi::Color::Named(named) => match named {
                ansi::NamedColor::Foreground | ansi::NamedColor::Background => None,
                other => Some((other as usize).into()),
            },
        }
    }

    fn snapshot_grid(
        &self,
    ) -> (
        String,
        Vec<SnapshotLine>,
        Option<CursorSnapshot>,
        bool,
        serde_json::Value,
    ) {
        let term = self.term.lock();
        let grid = term.grid();
        let content = term.renderable_content();
        let cursor = content.cursor;
        let hidden = matches!(cursor.shape, ansi::CursorShape::Hidden);
        let top = grid.topmost_line().0;
        let bottom = grid.bottommost_line().0;
        let mut lines = Vec::with_capacity((bottom - top + 1) as usize);
        let mut plain = Vec::with_capacity(lines.capacity());
        for line in top..=bottom {
            let row = &grid[Line(line)];
            let columns = row.into_iter().map(|cell| cell.c).collect::<String>();
            let mut spans: Vec<SnapshotSpan> = Vec::new();
            for (col, cell) in row.into_iter().enumerate() {
                if cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    continue;
                }
                let cells = usize::from(
                    col + 1 < grid.columns()
                        && row[Column(col + 1)]
                            .flags
                            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER),
                ) + 1;
                let mut text = cell.c.to_string();
                if let Some(marks) = cell.zerowidth() {
                    text.extend(marks);
                }
                let fg = Self::ansi_value(cell.fg);
                let bg = Self::ansi_value(cell.bg);
                let dim = cell.flags.contains(Flags::DIM);
                let bold = cell.flags.intersects(Flags::BOLD | Flags::BOLD_ITALIC);
                let inverse = cell.flags.contains(Flags::INVERSE);
                if let Some(last) = spans.last_mut()
                    && last.col + last.cells == col
                    && last.fg == fg
                    && last.bg == bg
                    && last.dim == dim
                    && last.bold == bold
                    && last.inverse == inverse
                {
                    last.text.push_str(&text);
                    last.cells += cells;
                    continue;
                }
                spans.push(SnapshotSpan {
                    text,
                    col,
                    cells,
                    fg,
                    bg,
                    dim,
                    bold,
                    inverse,
                });
            }
            plain.push(
                spans
                    .iter()
                    .map(|span| span.text.as_str())
                    .collect::<String>()
                    .trim_end()
                    .to_owned(),
            );
            lines.push(SnapshotLine { spans, columns });
        }
        while plain.last().is_some_and(String::is_empty)
            && lines.last().is_some_and(|line| {
                line.spans
                    .iter()
                    .all(|span| span.bg.is_none() && !span.inverse)
            })
        {
            plain.pop();
            lines.pop();
        }
        let row = cursor.point.line.0 - top;
        let mut col = cursor.point.column.0;
        let cursor = (row >= 0 && row <= bottom - top && col < grid.columns()).then(|| {
            let cell = &grid[cursor.point.line][Column(col)];
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                && col > 0
            {
                col -= 1;
            }
            let cell = &grid[cursor.point.line][Column(col)];
            let mut text = cell.c.to_string();
            if let Some(marks) = cell.zerowidth() {
                text.extend(marks);
            }
            CursorSnapshot {
                row: row as usize,
                col,
                text,
                shape: match cursor.shape {
                    ansi::CursorShape::Beam => "bar",
                    ansi::CursorShape::Underline => "underline",
                    _ => "block",
                },
            }
        });
        (
            plain.join("\n"),
            lines,
            cursor,
            hidden,
            selection_json(&term),
        )
    }

    #[cfg(all(test, unix))]
    fn snapshot_text(&self) -> String {
        self.snapshot_grid().0
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
                "printf 'pig-terminal-ready开发\\n'; read line; printf 'got:%s\\n' \"$line\"",
            ],
        )
        .expect("spawn test shell");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if terminal.snapshot_text().contains("pig-terminal-ready开发") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(terminal.snapshot_text().contains("pig-terminal-ready开发"));
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

    #[test]
    fn colored_space_lines_are_preserved() {
        let terminal = Terminal::spawn_program(
            "sh",
            &[
                "-c",
                "printf '\\033[41m    \\033[42m    \\033[0m\\n'; read line",
            ],
        )
        .expect("spawn test shell");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let lines = terminal.snapshot_grid().1;
            if lines
                .first()
                .is_some_and(|line| line.spans.iter().filter(|span| span.bg.is_some()).count() >= 2)
            {
                let spans = &lines[0].spans;
                assert_eq!((spans[0].col, spans[0].cells), (0, 4));
                assert_eq!(spans[0].bg, Some(1.into()));
                assert_eq!((spans[1].col, spans[1].cells), (4, 4));
                assert_eq!(spans[1].bg, Some(2.into()));
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "colored spaces were dropped from the snapshot"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn grid_spans_keep_ansi_colors_and_terminal_columns() {
        let terminal = Terminal::spawn_program(
            "sh",
            &["-c", "printf '\\033[1;33mAB\\033[0m开发\\n'; read line"],
        )
        .expect("spawn test shell");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let lines = terminal.snapshot_grid().1;
            if lines.first().is_some_and(|line| line.spans.len() >= 2) {
                let spans = &lines[0].spans;
                assert_eq!(spans[0].text, "AB");
                assert_eq!(spans[0].fg, Some(3.into()));
                assert!(spans[0].bold);
                assert_eq!((spans[1].col, spans[1].cells), (2, 78));
                assert!(spans[1].text.starts_with("开发"));
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "colored grid was not ready"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn selection_uses_the_alacritty_grid() {
        let terminal = Terminal::spawn_program(
            "sh",
            &["-c", "printf 'pig-terminal-ready开发\\n'; read line"],
        )
        .expect("spawn test shell");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !terminal.snapshot_text().contains("pig-terminal-ready开发") {
            assert!(std::time::Instant::now() < deadline, "output was not ready");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        terminal.select_start(0, 0, true, 1, false);
        let selected: serde_json::Value =
            serde_json::from_str(&terminal.select_update(8, 0, false)).unwrap();
        assert_eq!(selected["start"], serde_json::json!({"row": 0, "col": 0}));
        assert_eq!(selected["end"], serde_json::json!({"row": 0, "col": 9}));
        assert_eq!(terminal.copy_selection().as_deref(), Some("pig-termi"));
        terminal.select_end();
        terminal.select_update(15, 0, true);
        assert_eq!(terminal.copy_selection().as_deref(), Some("pig-termi"));
        assert_eq!(terminal.select_start(0, 0, true, 1, false), "null");
        terminal.select_end();
        assert_ne!(terminal.select_all(), "null");
        assert!(
            terminal
                .copy_selection()
                .unwrap()
                .contains("pig-terminal-ready开发")
        );
        terminal.clear_selection();
        assert_eq!(terminal.copy_selection(), None);
        terminal.select_start(0, 5, true, 1, false);
        let blank: serde_json::Value =
            serde_json::from_str(&terminal.select_update(70, 8, false)).unwrap();
        assert_eq!(blank["start"], serde_json::json!({"row": 5, "col": 0}));
        assert_eq!(blank["end"], serde_json::json!({"row": 8, "col": 71}));
        terminal.clear_selection();
    }
}
