//! alacritty Term 封装：EventProxy 事件代理 + PTY reader / 退出 waiter 线程。
//!
//! 参考 tty7 `src/terminal/remote.rs` 的 EventProxy（:37-60）、Term 构造
//! （:1067-1068）与 reader 循环（:1207 起，分批放锁见 feed_grid :3665-3704），
//! 砍掉 daemon 协议——读线程直接阻塞读 PTY master。

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

/// grid 尺寸（实现 alacritty Dimensions）。全抄 tty7 `src/terminal/size.rs`。
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

/// alacritty 事件 → UI 事件泵的桥（参考 tty7 remote.rs 的 EventProxy）。
/// try_send 不阻塞：UI 卡顿时宁可丢事件也不能卡住 PTY 读线程——
/// 丢 Wakeup 无碍（下一批输出会再发），队列是无界的，try_send 实际不会失败。
#[derive(Clone)]
pub(crate) struct EventProxy {
    tx: async_channel::Sender<AlacEvent>,
}

impl EventListener for EventProxy {
    fn send_event(&self, event: AlacEvent) {
        let _ = self.tx.try_send(event);
    }
}

/// 一条终端会话的全部非 UI 状态。term / pty 都是 Arc + 锁，渲染 prepaint
/// 里直接 lock 调用是安全的（它们不是 Entity）。
pub(crate) struct Terminal {
    pub(crate) term: Arc<FairMutex<Term<EventProxy>>>,
    pub(crate) events: async_channel::Receiver<AlacEvent>,
    pty: Arc<Pty>,
    size: Mutex<TermSize>,
    /// 子进程已退出（waiter 线程上报后由 view 置位；写键前快速判断）
    exited: AtomicBool,
}

impl Terminal {
    /// 起 shell：建 alacritty Term + 开 PTY + 读/等线程。
    /// 初始 80x24，首帧 prepaint 测出真实 cell 尺寸后立即 resize。
    pub(crate) fn spawn(cwd: &Path, shell: Option<&str>) -> std::io::Result<Self> {
        let size = TermSize::new(80, 24);
        let (pty, reader) = Pty::spawn(cwd, pty_size(size, 0, 0), shell)?;
        Ok(Self::assemble(size, pty, reader))
    }

    /// 测试用：起指定程序而非登录 shell
    #[cfg(test)]
    pub(crate) fn spawn_program(cwd: &Path, program: &str, args: &[&str]) -> std::io::Result<Self> {
        let size = TermSize::new(80, 24);
        let (pty, reader) = Pty::spawn_program(cwd, pty_size(size, 0, 0), program, args)?;
        Ok(Self::assemble(size, pty, reader))
    }

    fn assemble(size: TermSize, pty: Arc<Pty>, reader: Box<dyn std::io::Read + Send>) -> Self {
        let (tx, rx) = async_channel::unbounded::<AlacEvent>();
        let proxy = EventProxy { tx };

        // kitty_keyboard 开才能协商 kitty 键盘协议（input.rs 已实现全套编码；
        // 默认 false 会让现代 TUI 的 CSI >u 请求被静默忽略）
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

    /// 键盘 / 粘贴 / 应答字节写向 PTY
    pub(crate) fn write(&self, bytes: &[u8]) {
        self.pty.write(bytes);
    }

    /// cell 像素尺寸变化时同步 alacritty grid 与 PTY winsize
    /// （prepaint 每帧调用，尺寸没变直接返回）
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

/// 一次持锁喂给解析器的最大字节数。大批输出（cat 大文件）时分批放锁，
/// 让渲染线程能插进来取锁画帧（参考 tty7 feed_grid 的 MAX_LOCKED_FEED）。
const MAX_LOCKED_FEED: usize = 64 * 1024;

/// 读线程：阻塞读 PTY → ansi::Processor 喂进 Term → 每批发 Wakeup。
/// EOF / 读错 = shell 侧全走完了，发最后一个 Wakeup + Exit（参考 tty7 的
/// teardown：退出事件跟在内容之后，UI 先吃到尾部输出再标退出）。
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

/// 退出检测线程：轮询 try_wait（100ms 一次，一个 waitpid 的开销）。
/// Weak 持有 Pty——关 tab 后 Terminal/Pty 被 drop（Pty Drop 会 kill 子进程），
/// upgrade 失败或见到 killed 标志即静默退出，不再上报。
/// 退出事件发 Exit 而非 ChildExit：portable-pty 的 ExitStatus 是自有类型，
/// 喂不进 alacritty 的 ChildExit(std::process::ExitStatus)，而 view 对两者
/// 的处理相同（标记退出），退出码暂不展示。
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

    /// 端到端冒烟（不开 GUI）：真 PTY 起 sh，验证 输出→读线程→解析进 grid
    /// 与 子进程退出→waiter→Exit 事件 两条链路
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
        assert!(saw_output, "没等到 sh 的 echo 输出");
        assert!(saw_exit, "没等到子进程退出事件");
    }

    /// 写往 PTY 的键入应原样到达子进程 stdin（sh -c read 回显）
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
        assert!(saw, "sh 没收到写入的键入");
    }
}
