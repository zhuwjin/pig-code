use std::io::{Write as _, stdout};
use std::path::PathBuf;

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{Event as TerminalEvent, EventStream, KeyCode, KeyEvent, KeyModifiers};
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
            running: false,
            items: Vec::new(),
            notices: Vec::new(),
            input: String::new(),
            permissions: ExecMode::Auto.permissions(),
        }
    }

    fn apply(&mut self, event: Event) {
        match event {
            Event::SessionConfigured {
                session_id,
                cwd,
                permissions,
                ..
            } => {
                if self.session_id.as_deref() != Some(session_id.as_str()) {
                    self.items.clear();
                }
                self.session_id = Some(session_id);
                self.auto_open = false;
                self.cwd = cwd;
                self.permissions = permissions;
            }
            Event::SessionList { sessions } => {
                self.notices.extend(
                    sessions
                        .iter()
                        .map(|session| format!("{}  {}", session.id, session.title)),
                );
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
            Event::ConversationSnapshot {
                session_id,
                agent_id: None,
                items,
                running,
                ..
            } if self.session_id.as_deref() == Some(session_id.as_str()) => {
                self.items = items;
                self.running = running;
            }
            Event::ConversationUpdate {
                session_id,
                agent_id: None,
                item,
                ..
            } if self.session_id.as_deref() == Some(session_id.as_str()) => {
                upsert(&mut self.items, item);
            }
            Event::ConversationRemove {
                session_id,
                agent_id: None,
                item_ids,
                ..
            } if self.session_id.as_deref() == Some(session_id.as_str()) => {
                self.items
                    .retain(|item| !item_ids.iter().any(|id| id == item.id()));
            }
            Event::TurnStarted { .. } => self.running = true,
            Event::TurnComplete { .. } | Event::TurnAborted { .. } => self.running = false,
            Event::ApprovalRequested {
                request_id,
                tool,
                detail,
                ..
            } => self.notices.push(format!(
                "/allow {request_id} | /deny {request_id}\n{tool}: {detail}"
            )),
            Event::ApprovalResolved { request_id, .. } => {
                self.notices
                    .push(format!("approval resolved: {request_id}"));
            }
            Event::QuestionRequested {
                request_id,
                questions,
                ..
            } => {
                let text = questions
                    .iter()
                    .map(|question| question.question.clone())
                    .collect::<Vec<_>>()
                    .join(" | ");
                self.notices.push(format!(
                    "/answer {request_id} ANSWER [| ANSWER...] | /skip {request_id}\n{text}"
                ));
            }
            Event::QuestionResolved { request_id, .. } => {
                self.notices
                    .push(format!("question resolved: {request_id}"));
            }
            Event::Error { error, .. } => self.notices.push(format!("{error:?}")),
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
            return self.client.send_op(Op::ListSessions);
        }
        if input == "/new" || input.starts_with("/new ") {
            self.auto_open = false;
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
            self.session_id = Some(session_id.to_string());
            self.items.clear();
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
        queue!(output, Print(truncate(&status, width as usize)))
            .map_err(|error| error.to_string())?;
        let body_height = height as usize - 4;
        let width = width as usize;
        let lines = self
            .lines()
            .iter()
            .flat_map(|line| wrap(line, width))
            .collect::<Vec<_>>();
        let start = lines.len().saturating_sub(body_height);
        for (row, line) in lines[start..].iter().enumerate() {
            queue!(output, MoveTo(0, row as u16 + 1), Print(line))
                .map_err(|error| error.to_string())?;
        }
        queue!(
            output,
            MoveTo(0, height - 3),
            Print("/open /new /sessions /stop /allow /deny /answer /quit"),
            MoveTo(0, height - 2),
            Print(format!("> {}", self.input)),
            MoveTo(
                (self.input.chars().count() as u16 + 2).min(width as u16 - 1),
                height - 2
            )
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

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    text.chars().take(limit).collect::<String>() + "..."
}

fn wrap(line: &str, width: usize) -> Vec<String> {
    let mut rows = Vec::new();
    for line in line.split('\n') {
        let mut current = String::new();
        for character in line.chars() {
            if current.chars().count() == width {
                rows.push(std::mem::take(&mut current));
            }
            current.push(character);
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
                    Some(Ok(TerminalEvent::Key(KeyEvent { code, modifiers, .. }))) => {
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

    #[test]
    fn wrap_preserves_text_and_respects_width() {
        assert_eq!(wrap("abcdef", 3), vec!["abc", "def"]);
        assert_eq!(wrap("a\nb", 3), vec!["a", "b"]);
    }
}
