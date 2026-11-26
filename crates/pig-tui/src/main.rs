use std::io::{Write as _, stdout};
use std::path::PathBuf;

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{
    Event as TerminalEvent, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use crossterm::style::Print;
use crossterm::terminal::{
    Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
    enable_raw_mode, size,
};
use crossterm::{execute, queue};
use futures_util::StreamExt as _;
use pig_daemon::{DaemonClient, DaemonConnectConfig, DaemonMessage, snapshot_events};
use pig_protocol::{
    ApprovalDecision, ConversationItem, Event, ExecMode, MessageDelivery, Op, PermissionSettings,
};
use unicode_width::{UnicodeWidthChar as _, UnicodeWidthStr as _};

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self, String> {
        enable_raw_mode().map_err(|error| error.to_string())?;
        execute!(stdout(), EnterAlternateScreen, Hide).map_err(|error| error.to_string())?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(stdout(), Show, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

struct Options {
    connect: DaemonConnectConfig,
    session_id: Option<String>,
    new_session: bool,
}

fn options() -> Result<Options, String> {
    let mut options = Options {
        connect: DaemonConnectConfig {
            data_dir: pig_utils::data_dir(),
            cwd: std::env::current_dir().map_err(|error| error.to_string())?,
            config_path: None,
        },
        session_id: None,
        new_session: false,
    };
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--data-dir" => {
                options.connect.data_dir = PathBuf::from(
                    arguments
                        .next()
                        .ok_or("--data-dir requires a path".to_string())?,
                );
            }
            "--cwd" => {
                options.connect.cwd = PathBuf::from(
                    arguments
                        .next()
                        .ok_or("--cwd requires a path".to_string())?,
                );
            }
            "--config" => {
                options.connect.config_path = Some(PathBuf::from(
                    arguments
                        .next()
                        .ok_or("--config requires a path".to_string())?,
                ));
            }
            "--session" => {
                options.session_id = Some(
                    arguments
                        .next()
                        .ok_or("--session requires an id".to_string())?,
                );
            }
            "--new" => options.new_session = true,
            "--help" | "-h" => {
                println!(
                    "pig-tui [--data-dir PATH] [--cwd PATH] [--config PATH] [--session ID | --new]\n\n/open ID /new [PATH] /sessions /stop /allow ID /deny ID /answer ID TEXT /skip ID /quit"
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(options)
}

struct Tui {
    client: DaemonClient,
    cwd: PathBuf,
    session_id: Option<String>,
    auto_open: bool,
    awaiting_new_session: bool,
    show_sessions_once: bool,
    running: bool,
    items: Vec<ConversationItem>,
    notices: Vec<String>,
    input: String,
    permissions: PermissionSettings,
}

impl Tui {
    fn new(client: DaemonClient, options: &Options) -> Self {
        Self {
            client,
            cwd: options.connect.cwd.clone(),
            session_id: options.session_id.clone(),
            auto_open: !options.new_session && options.session_id.is_none(),
            awaiting_new_session: options.new_session,
            show_sessions_once: false,
            running: false,
            items: Vec::new(),
            notices: Vec::new(),
            input: String::new(),
            permissions: ExecMode::Auto.permissions(),
        }
    }

    fn matches_session(&self, session_id: &str) -> bool {
        self.session_id.as_deref() == Some(session_id)
    }

    fn apply(&mut self, event: Event) {
        match event {
            Event::SessionConfigured {
                session_id,
                cwd,
                permissions,
                ..
            } => {
                if !self.awaiting_new_session && !self.matches_session(&session_id) {
                    return;
                }
                if self.session_id.as_deref() != Some(session_id.as_str()) {
                    self.items.clear();
                    self.running = false;
                }
                self.session_id = Some(session_id);
                self.awaiting_new_session = false;
                self.auto_open = false;
                self.cwd = cwd;
                self.permissions = permissions;
            }
            Event::SessionList { sessions } => {
                if self.show_sessions_once {
                    self.show_sessions_once = false;
                    if sessions.is_empty() {
                        self.notices.push("no sessions".to_string());
                    } else {
                        self.notices.extend(
                            sessions
                                .iter()
                                .map(|session| format!("{}  {}", session.id, session.title)),
                        );
                    }
                }
                if self.auto_open
                    && self.session_id.is_none()
                    && let Some(session) = sessions.first()
                {
                    self.session_id = Some(session.id.clone());
                    self.auto_open = false;
                    let _ = self.client.send_op(Op::OpenSession {
                        session_id: session.id.clone(),
                    });
                }
            }
            Event::PermissionsChanged {
                session_id,
                permissions,
            } if self.matches_session(&session_id) => {
                self.permissions = permissions;
            }
            Event::ConversationSnapshot {
                session_id,
                agent_id: None,
                items,
                running,
                ..
            } if self.matches_session(&session_id) => {
                self.items = items;
                self.running = running;
            }
            Event::ConversationUpdate {
                session_id,
                agent_id: None,
                item,
                ..
            } if self.matches_session(&session_id) => {
                upsert(&mut self.items, item);
            }
            Event::ConversationRemove {
                session_id,
                agent_id: None,
                item_ids,
                ..
            } if self.matches_session(&session_id) => {
                self.items
                    .retain(|item| !item_ids.iter().any(|id| id == item.id()));
            }
            Event::TurnStarted { session_id, .. } if self.matches_session(&session_id) => {
                self.running = true;
            }
            Event::TurnComplete { session_id, .. } | Event::TurnAborted { session_id, .. }
                if self.matches_session(&session_id) =>
            {
                self.running = false;
            }
            Event::ApprovalRequested {
                session_id,
                request_id,
                tool,
                detail,
                ..
            } if self.matches_session(&session_id) => self.notices.push(format!(
                "/allow {request_id} | /deny {request_id}\n{tool}: {detail}"
            )),
            Event::ApprovalResolved {
                session_id,
                request_id,
            } if self.matches_session(&session_id) => {
                self.notices
                    .push(format!("approval resolved: {request_id}"));
            }
            Event::QuestionRequested {
                session_id,
                request_id,
                questions,
            } if self.matches_session(&session_id) => {
                let text = questions
                    .iter()
                    .map(|question| question.question.clone())
                    .collect::<Vec<_>>()
                    .join(" | ");
                self.notices.push(format!(
                    "/answer {request_id} ANSWER [| ANSWER...] | /skip {request_id}\n{text}"
                ));
            }
            Event::QuestionResolved {
                session_id,
                request_id,
            } if self.matches_session(&session_id) => {
                self.notices
                    .push(format!("question resolved: {request_id}"));
            }
            Event::Error {
                session_id, error, ..
            } if session_id
                .as_deref()
                .is_none_or(|id| self.matches_session(id)) =>
            {
                self.notices.push(format!("{error:?}"));
            }
            _ => {}
        }
        const MAX_NOTICES: usize = 100;
        if self.notices.len() > MAX_NOTICES {
            self.notices.drain(..self.notices.len() - MAX_NOTICES);
        }
    }

    fn submit(&mut self) {
        let input = std::mem::take(&mut self.input);
        let input = input.trim();
        if input.is_empty() {
            return;
        }
        if let Err(error) = self.submit_inner(input) {
            self.notices.push(error);
        }
    }

    fn submit_inner(&mut self, input: &str) -> Result<(), String> {
        if input == "/quit" {
            return Ok(());
        }
        if input == "/sessions" {
            self.show_sessions_once = true;
            return self.client.send_op(Op::ListSessions);
        }
        if input == "/new" || input.starts_with("/new ") {
            self.auto_open = false;
            self.awaiting_new_session = true;
            self.items.clear();
            let cwd = input
                .strip_prefix("/new ")
                .map(PathBuf::from)
                .unwrap_or_else(|| self.cwd.clone());
            return self.client.send_op(Op::NewSession {
                cwd,
                provider_id: None,
                model_id: None,
                reasoning_level: None,
                permissions: Some(self.permissions),
                plan_enabled: Some(false),
            });
        }
        if let Some(session_id) = input.strip_prefix("/open ") {
            let session_id = session_id.trim();
            if session_id.is_empty() {
                return Err("/open requires an id".to_string());
            }
            self.auto_open = false;
            self.awaiting_new_session = false;
            self.session_id = Some(session_id.to_string());
            self.items.clear();
            self.running = false;
            return self.client.send_op(Op::OpenSession {
                session_id: session_id.to_string(),
            });
        }
        if input == "/stop" {
            return self.client.send_op(Op::Interrupt {
                session_id: self.session()?,
            });
        }
        if let Some(request_id) = input.strip_prefix("/allow ") {
            return self.approval(request_id, ApprovalDecision::Allow);
        }
        if let Some(request_id) = input.strip_prefix("/deny ") {
            return self.approval(request_id, ApprovalDecision::Reject);
        }
        if let Some(request_id) = input.strip_prefix("/skip ") {
            return self.client.send_op(Op::QuestionReply {
                request_id: request_id.trim().to_string(),
                answers: None,
            });
        }
        if let Some(arguments) = input.strip_prefix("/answer ") {
            let (request_id, answers) = arguments
                .split_once(' ')
                .ok_or("/answer requires an id and an answer".to_string())?;
            let answers = answers
                .split('|')
                .map(|answer| {
                    answer
                        .split(',')
                        .map(str::trim)
                        .filter(|answer| !answer.is_empty())
                        .map(str::to_string)
                        .collect()
                })
                .collect();
            return self.client.send_op(Op::QuestionReply {
                request_id: request_id.to_string(),
                answers: Some(answers),
            });
        }
        if input.starts_with('/') {
            return Err(format!("unknown command: {input}"));
        }
        self.client.send_op(Op::SendMessage {
            session_id: self.session()?,
            content: input.to_string(),
            files: Vec::new(),
            images: Vec::new(),
            permissions: self.permissions,
            delivery: MessageDelivery::Queue,
        })
    }

    fn approval(&self, request_id: &str, decision: ApprovalDecision) -> Result<(), String> {
        if request_id.trim().is_empty() {
            return Err("approval command requires an id".to_string());
        }
        self.client.send_op(Op::ApprovalReply {
            request_id: request_id.trim().to_string(),
            decision,
            feedback: None,
        })
    }

    fn session(&self) -> Result<String, String> {
        self.session_id
            .clone()
            .ok_or("open or create a session first".to_string())
    }

    fn lines(&self) -> Vec<String> {
        self.items
            .iter()
            .map(render_item)
            .chain(self.notices.iter().cloned())
            .collect()
    }

    fn draw(&self) -> Result<(), String> {
        let (width, height) = size().map_err(|error| error.to_string())?;
        if width < 20 || height < 6 {
            return Ok(());
        }
        let mut output = stdout();
        queue!(output, Clear(ClearType::All), MoveTo(0, 0)).map_err(|error| error.to_string())?;
        let status = format!(
            "pig-tui | {} | {}",
            self.session_id.as_deref().unwrap_or("no session"),
            if self.running { "running" } else { "idle" }
        );
        let width_usize = width as usize;
        queue!(output, Print(truncate(&status, width_usize)))
            .map_err(|error| error.to_string())?;
        let body_height = height as usize - 4;
        let lines = self
            .lines()
            .iter()
            .flat_map(|line| wrap(line, width_usize))
            .collect::<Vec<_>>();
        let start = lines.len().saturating_sub(body_height);
        for (row, line) in lines[start..].iter().enumerate() {
            queue!(output, MoveTo(0, row as u16 + 1), Print(line))
                .map_err(|error| error.to_string())?;
        }
        let help = truncate(
            "/open /new /sessions /stop /allow /deny /answer /quit",
            width_usize,
        );
        let input_budget = width_usize.saturating_sub(3);
        let (visible_input, visible_width) = tail_within_width(&self.input, input_budget);
        queue!(
            output,
            MoveTo(0, height - 3),
            Print(help),
            MoveTo(0, height - 2),
            Print(format!("> {visible_input}")),
            MoveTo((visible_width as u16 + 2).min(width - 1), height - 2)
        )
        .map_err(|error| error.to_string())?;
        output.flush().map_err(|error| error.to_string())
    }
}

fn upsert(items: &mut Vec<ConversationItem>, item: ConversationItem) {
    if let Some(index) = items.iter().position(|current| current.id() == item.id()) {
        items[index] = item;
    } else {
        items.push(item);
    }
}

fn render_item(item: &ConversationItem) -> String {
    const MAX_TOOL_OUTPUT: usize = 4000;
    match item {
        ConversationItem::User { text, .. } => format!("You: {text}"),
        ConversationItem::Markdown { text, .. } => text.clone(),
        ConversationItem::Reasoning { text, .. } => format!("Reasoning: {text}"),
        ConversationItem::Tool {
            tool,
            summary,
            output,
            status,
            ..
        } => format!(
            "[{status:?}] {tool} {summary}\n{}",
            truncate(output, MAX_TOOL_OUTPUT)
        ),
        ConversationItem::Note { text, .. } => format!("Note: {text}"),
        ConversationItem::Notification { notifications, .. } => {
            format!("{} notification(s)", notifications.len())
        }
    }
}

fn truncate(text: &str, max_width: usize) -> String {
    if text.width() <= max_width {
        return text.to_string();
    }
    if max_width <= 3 {
        return ".".repeat(max_width);
    }
    let budget = max_width - 3;
    let mut used = 0;
    let mut output = String::new();
    for ch in text.chars() {
        let ch_width = ch.width().unwrap_or(0);
        if used + ch_width > budget {
            break;
        }
        output.push(ch);
        used += ch_width;
    }
    output.push_str("...");
    output
}

fn tail_within_width(text: &str, max_width: usize) -> (&str, usize) {
    let mut used = 0;
    let mut start_byte = text.len();
    for (byte_index, ch) in text.char_indices().rev() {
        let ch_width = ch.width().unwrap_or(0);
        if used + ch_width > max_width {
            break;
        }
        used += ch_width;
        start_byte = byte_index;
    }
    (&text[start_byte..], used)
}

fn wrap(line: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut rows = Vec::new();
    for line in line.split('\n') {
        let mut current = String::new();
        let mut current_width = 0;
        for character in line.chars() {
            let ch_width = character.width().unwrap_or(0);
            if current_width > 0 && current_width + ch_width > width {
                rows.push(std::mem::take(&mut current));
                current_width = 0;
            }
            current.push(character);
            current_width += ch_width;
        }
        rows.push(current);
    }
    rows
}

async fn run(mut tui: Tui) -> Result<(), String> {
    let _terminal = TerminalGuard::enter()?;
    let mut terminal_events = EventStream::new();
    tui.draw()?;
    loop {
        tokio::select! {
            biased;
            message = tui.client.recv() => {
                match message {
                    Ok(DaemonMessage::Subscription { resync_required: true, .. }) => {
                        if let Some(session_id) = tui.session_id.clone() {
                            tui.client.send_op(Op::OpenSession { session_id })?;
                        }
                    }
                    Ok(DaemonMessage::Subscription { .. }) => {}
                    Ok(DaemonMessage::Batch(batch)) => {
                        for event in batch.events {
                            tui.apply(event);
                        }
                    }
                    Ok(DaemonMessage::Snapshot(snapshot)) => {
                        for event in snapshot_events(&snapshot) {
                            tui.apply(event);
                        }
                    }
                    Ok(DaemonMessage::Error { detail }) => tui.notices.push(detail),
                    Err(_) => return Ok(()),
                }
            }
            event = terminal_events.next() => {
                match event {
                    Some(Ok(TerminalEvent::Key(KeyEvent { code, modifiers, kind, .. }))) => {
                        if kind == KeyEventKind::Release {
                            continue;
                        }
                        if modifiers.contains(KeyModifiers::CONTROL)
                            && matches!(code, KeyCode::Char('c') | KeyCode::Char('d'))
                        {
                            return Ok(());
                        }
                        match code {
                            KeyCode::Char(character) => tui.input.push(character),
                            KeyCode::Backspace => {
                                tui.input.pop();
                            }
                            KeyCode::Enter => {
                                if tui.input.trim() == "/quit" {
                                    return Ok(());
                                }
                                tui.submit();
                            }
                            _ => {}
                        }
                    }
                    Some(Ok(TerminalEvent::Resize(_, _))) => {}
                    Some(Ok(_)) => {}
                    Some(Err(error)) => return Err(error.to_string()),
                    None => return Ok(()),
                }
            }
        }
        tui.draw()?;
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let result = async {
        let options = options()?;
        let client = DaemonClient::connect_or_spawn(options.connect.clone())?;
        let mut tui = Tui::new(client.clone(), &options);
        client.send_op(Op::ListSessions)?;
        if options.new_session {
            client.send_op(Op::NewSession {
                cwd: options.connect.cwd.clone(),
                provider_id: None,
                model_id: None,
                reasoning_level: None,
                permissions: Some(ExecMode::Auto.permissions()),
                plan_enabled: Some(false),
            })?;
        } else if let Some(session_id) = &options.session_id {
            client.send_op(Op::OpenSession {
                session_id: session_id.clone(),
            })?;
        }
        tui.notices.push("connected".to_string());
        run(tui).await
    }
    .await;
    if let Err(error) = result {
        eprintln!("pig-tui: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_tui(session_id: Option<&str>, new_session: bool) -> Tui {
        let (ops, _ops_rx) = async_channel::bounded(16);
        let (_msg_tx, messages) = async_channel::bounded(16);
        let client = DaemonClient::from_channels(ops, messages);
        let options = Options {
            connect: DaemonConnectConfig {
                data_dir: PathBuf::from("."),
                cwd: PathBuf::from("."),
                config_path: None,
            },
            session_id: session_id.map(str::to_string),
            new_session,
        };
        Tui::new(client, &options)
    }

    fn configured(session_id: &str) -> Event {
        Event::SessionConfigured {
            session_id: session_id.to_string(),
            cwd: PathBuf::from("/workspace"),
            model: "model".to_string(),
            provider_name: "provider".to_string(),
            provider_id: Some("provider".to_string()),
            model_id: Some("model".to_string()),
            reasoning_level: None,
            permissions: PermissionSettings::default(),
            plan_enabled: false,
        }
    }

    #[test]
    fn wrap_preserves_text_and_respects_display_width() {
        assert_eq!(wrap("abcdef", 3), vec!["abc", "def"]);
        assert_eq!(wrap("a\nb", 3), vec!["a", "b"]);
        // CJK characters occupy 2 terminal columns each.
        assert_eq!(wrap("你好世界", 4), vec!["你好", "世界"]);
        assert_eq!(wrap("a你好b", 4), vec!["a你", "好b"]);
    }

    #[test]
    fn truncate_and_tail_respect_cjk_display_width() {
        assert_eq!(truncate("你好世界", 7), "你好...");
        assert_eq!(truncate("你好", 4), "你好");
        assert_eq!(tail_within_width("你好世界", 4), ("世界", 4));
        assert_eq!(tail_within_width("ab你好", 5), ("b你好", 5));
    }

    #[test]
    fn other_session_events_do_not_hijack_tui() {
        let mut tui = test_tui(Some("s1"), false);
        tui.apply(configured("s1"));
        tui.apply(Event::ConversationUpdate {
            session_id: "s1".to_string(),
            agent_id: None,
            run_id: None,
            item: ConversationItem::User {
                id: "u1".to_string(),
                text: "keep me".to_string(),
                files: Vec::new(),
                image_nums: Vec::new(),
                sent_at_ms: 1,
            },
        });

        // Another client creates/opens s2 and runs a turn with an approval prompt.
        tui.apply(configured("s2"));
        tui.apply(Event::TurnStarted {
            session_id: "s2".to_string(),
            turn_id: "t1".to_string(),
            run: None,
        });
        tui.apply(Event::ApprovalRequested {
            session_id: "s2".to_string(),
            request_id: "req-other".to_string(),
            tool: "ExecCommand".to_string(),
            detail: "rm -rf".to_string(),
            danger_key: None,
            requested_write_roots: Vec::new(),
            available_decisions: Vec::new(),
        });

        assert_eq!(tui.session_id.as_deref(), Some("s1"));
        assert_eq!(tui.items.len(), 1);
        assert!(!tui.running);
        assert!(tui.notices.is_empty());
    }
}
