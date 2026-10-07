//! alacritty Term wrapper: the EventProxy event proxy plus the PTY reader and
//! exit waiter threads.
//!
//! Modeled on tty7 `src/terminal/remote.rs`'s EventProxy (:37-60), Term
//! construction (:1067-1068), and the reader loop (:1207 onward; batched lock
//! release in feed_grid :3665-3704), dropping the daemon protocol; the reader
//! thread blocks reading the PTY master directly.

use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use alacritty_terminal::event::{Event as AlacEvent, EventListener};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi;
use portable_pty::PtySize;

use super::pty::Pty;

/// Grid size (implements alacritty Dimensions). A verbatim port of tty7 `src/terminal/size.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TermSize {
    pub cols: usize,
    pub rows: usize,
}

impl TermSize {
    pub fn new(cols: usize, rows: usize) -> Self {
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

/// Bridge from alacritty events to the UI event pump (see EventProxy in tty7
/// remote.rs). try_send never blocks: when the UI stalls, dropping an event is
/// preferable to stalling the PTY reader thread. A dropped Wakeup is harmless
/// (the next batch of output sends another), the queue is unbounded, and
/// try_send does not actually fail.
#[derive(Clone)]
pub(crate) struct EventProxy {
    tx: async_channel::Sender<AlacEvent>,
}

impl EventListener for EventProxy {
    fn send_event(&self, event: AlacEvent) {
        let _ = self.tx.try_send(event);
    }
}

/// All non-UI state of one terminal session. term / pty are both Arc plus lock,
/// so locking and calling them directly in the render prepaint is safe (they are
/// not Entities).
pub(crate) struct Terminal {
    pub(crate) term: Arc<FairMutex<Term<EventProxy>>>,
    pub(crate) events: async_channel::Receiver<AlacEvent>,
    pty: Arc<Pty>,
    size: Mutex<TermSize>,
    /// Child process exited (set by the view after the waiter thread reports; quick check before writing keys)
    exited: AtomicBool,
}

impl Terminal {
    /// Start a shell: build the alacritty Term, open the PTY, and start the
    /// reader/waiter threads. Initially 80x24; resized as soon as the first
    /// prepaint measures the real cell size.
    pub(crate) fn spawn(cwd: &Path, shell: Option<&str>) -> std::io::Result<Self> {
        let size = TermSize::new(80, 24);
        let (pty, reader) = Pty::spawn(cwd, pty_size(size, 0, 0), shell)?;
        Ok(Self::assemble(size, pty, reader))
    }

    /// For tests: start the given program instead of a login shell
    #[cfg(test)]
    pub(crate) fn spawn_program(cwd: &Path, program: &str, args: &[&str]) -> std::io::Result<Self> {
        let size = TermSize::new(80, 24);
        let (pty, reader) = Pty::spawn_program(cwd, pty_size(size, 0, 0), program, args)?;
        Ok(Self::assemble(size, pty, reader))
    }

    fn assemble(size: TermSize, pty: Arc<Pty>, reader: Box<dyn std::io::Read + Send>) -> Self {
        let (tx, rx) = async_channel::unbounded::<AlacEvent>();
        let proxy = EventProxy { tx };

        // kitty_keyboard must be on to negotiate the kitty keyboard protocol
        // (input.rs implements the full encoding; the default false would leave
        // a modern TUI's CSI >u request silently ignored)
        let config = Config {
            scrolling_history: 10_000,
            kitty_keyboard: true,
            ..Default::default()
        };
        let term = Term::new(config, &size, proxy.clone());
        let term = Arc::new(FairMutex::new(term));

        spawn_reader(term.clone(), proxy.clone(), reader);
        spawn_waiter(Arc::downgrade(&pty), proxy);

        Self {
            term,
            events: rx,
            pty,
            size: Mutex::new(size),
            exited: AtomicBool::new(false),
        }
    }

    pub(crate) fn shell_name(&self) -> &str {
        self.pty.shell_name()
    }

    /// Write keyboard / paste / reply bytes to the PTY
    pub(crate) fn write(&self, bytes: &[u8]) {
        self.pty.write(bytes);
    }

    /// Sync the alacritty grid and PTY winsize when the cell pixel size changes
    /// (called every frame in prepaint; returns immediately when unchanged)
    pub(crate) fn resize(&self, size: TermSize, cell_width: u16, cell_height: u16) {
        {
            let mut cur = self.size.lock().unwrap();
            if *cur == size {
                return;
            }
            *cur = size;
        }
        self.term.lock().resize(size);
        self.pty.resize(pty_size(size, cell_width, cell_height));
    }

    pub(crate) fn size(&self) -> TermSize {
        *self.size.lock().unwrap()
    }

    pub(crate) fn exited(&self) -> bool {
        self.exited.load(Ordering::SeqCst)
    }

    pub(crate) fn mark_exited(&self) {
        self.exited.store(true, Ordering::SeqCst);
    }
}

fn pty_size(size: TermSize, cell_width: u16, cell_height: u16) -> PtySize {
    PtySize {
        rows: size.rows as u16,
        cols: size.cols as u16,
        pixel_width: cell_width,
        pixel_height: cell_height,
    }
}

/// Maximum bytes fed to the parser per lock hold. On large bursts (cat of a big
/// file) the lock is released between batches so the render thread can slip in,
/// take the lock, and draw a frame (see MAX_LOCKED_FEED in tty7 feed_grid).
const MAX_LOCKED_FEED: usize = 64 * 1024;

/// Reader thread: block reading the PTY, feed it into the Term via
/// ansi::Processor, and send a Wakeup per batch. EOF / read error means the
/// shell side is fully done, so send a final Wakeup plus Exit (see tty7's
/// teardown: the exit event follows the content so the UI consumes the tail
/// output before marking exited).
fn spawn_reader(
    term: Arc<FairMutex<Term<EventProxy>>>,
    proxy: EventProxy,
    mut reader: Box<dyn Read + Send>,
) {
    std::thread::Builder::new()
        .name("pig-term-reader".to_string())
        .spawn(move || {
            let mut processor: ansi::Processor = ansi::Processor::new();
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let mut at = 0;
                        while at < n {
                            let end = (at + MAX_LOCKED_FEED).min(n);
                            {
                                let mut t = term.lock();
                                processor.advance(&mut *t, &buf[at..end]);
                            }
                            at = end;
                        }
                        proxy.send_event(AlacEvent::Wakeup);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            proxy.send_event(AlacEvent::Wakeup);
            proxy.send_event(AlacEvent::Exit);
        })
        .expect("spawn pig-term-reader");
}

/// Exit detection thread: polls try_wait every 100ms (the cost of one waitpid).
/// Holds the Pty via a Weak; after a tab closes, Terminal/Pty get dropped (Pty's
/// Drop kills the child), and a failed upgrade or a seen killed flag means a
/// silent exit with nothing further reported. The exit event is Exit rather than
/// ChildExit: portable-pty's ExitStatus is its own type and does not fit into
/// alacritty's ChildExit(std::process::ExitStatus), while the view handles both
/// the same way (mark exited); the exit code is not shown for now.
fn spawn_waiter(pty: Weak<Pty>, proxy: EventProxy) {
    std::thread::Builder::new()
        .name("pig-term-waiter".to_string())
        .spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_millis(100));
                let Some(pty) = pty.upgrade() else {
                    return;
                };
                if pty.killed() {
                    return;
                }
                if pty.try_wait().is_some() {
                    proxy.send_event(AlacEvent::Exit);
                    return;
                }
            }
        })
        .expect("spawn pig-term-waiter");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End-to-end smoke test (no GUI): start sh on a real PTY, verifying both
    /// chains: output → reader thread → parsed into the grid, and child exit →
    /// waiter → Exit event
    #[test]
    fn 端到端跑通输出与退出() {
        let dir = std::env::temp_dir();
        let term = Terminal::spawn_program(&dir, "sh", &["-c", "echo pig-term-ok"]).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut saw_output = false;
        let mut saw_exit = false;
        while std::time::Instant::now() < deadline && !(saw_output && saw_exit) {
            while let Ok(ev) = term.events.try_recv() {
                match ev {
                    AlacEvent::Wakeup => {
                        let t = term.term.lock();
                        let text: String = t
                            .renderable_content()
                            .display_iter
                            .map(|cell| cell.c)
                            .collect();
                        saw_output |= text.contains("pig-term-ok");
                    }
                    AlacEvent::ChildExit(_) | AlacEvent::Exit => saw_exit = true,
                    _ => {}
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(saw_output, "did not see the echo output from sh");
        assert!(saw_exit, "did not see the child exit event");
    }

    /// Keys written to the PTY must reach the child's stdin as-is (echoed by sh -c read)
    #[test]
    fn 写入到达子进程() {
        let dir = std::env::temp_dir();
        let term =
            Terminal::spawn_program(&dir, "sh", &["-c", "read line; echo got:$line"]).unwrap();
        term.write(b"hello-pty\n");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut saw = false;
        while std::time::Instant::now() < deadline && !saw {
            while term.events.try_recv().is_ok() {}
            let t = term.term.lock();
            let text: String = t
                .renderable_content()
                .display_iter
                .map(|cell| cell.c)
                .collect();
            saw = text.contains("got:hello-pty");
            drop(t);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(saw, "sh did not receive the written keystrokes");
    }
}
