//! Pig Code's agent engine: Session / turn loop / OpenAI-compatible provider / tool execution.
//! Runs on a dedicated tokio runtime thread via `spawn_agent`, exchanging Op/Event with the
//! UI over channels.
//!
//! Language-agnostic (Clean/Hexagonal): core holds no i18n registry — errors are carried by
//! pig-protocol's CoreError enum + English detail, with localization deferred to pig-app
//! render points; model-facing text (prompts/tool results/refusal receipts) is always
//! English constants (see the root AGENTS.md convention).

pub mod agent;
pub mod config;
pub mod files;
pub mod git;
pub mod mcp;
pub mod mock;
pub mod model_io;
pub mod models_registry;
pub mod paths;
pub mod permissions;
mod prompt;
pub mod provider;
pub mod rollout;
pub mod session;
pub mod skills;
pub mod store;
pub mod task;
pub mod text;
pub mod tool;

use std::path::PathBuf;

use pig_protocol::{Event, Op};

pub use pig_protocol::AppConfig;

/// Data directory: the PIG_DATA_DIR environment variable first (self-test isolation); default ~/.pigcode.
pub fn data_dir() -> PathBuf {
    std::env::var_os("PIG_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("USERPROFILE")
                .or_else(|| std::env::var_os("HOME"))
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".pigcode")
        })
}

/// Windows release is a GUI subsystem (no console): every console child process
/// (git/cmd/bash/npx.cmd...) pops its own console window on spawn — with git's high call
/// rate this shows up as frantic window flashing. All child processes uniformly go through
/// `.no_console()` adding CREATE_NO_WINDOW; a no-op on non-Windows.
#[cfg(windows)]
pub(crate) trait NoConsoleExt {
    fn no_console(&mut self) -> &mut Self;
}

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[cfg(windows)]
impl NoConsoleExt for std::process::Command {
    fn no_console(&mut self) -> &mut Self {
        use std::os::windows::process::CommandExt as _;
        self.creation_flags(CREATE_NO_WINDOW);
        self
    }
}

#[cfg(windows)]
impl NoConsoleExt for tokio::process::Command {
    fn no_console(&mut self) -> &mut Self {
        self.creation_flags(CREATE_NO_WINDOW);
        self
    }
}

#[cfg(not(windows))]
pub(crate) trait NoConsoleExt {
    fn no_console(&mut self) -> &mut Self;
}

#[cfg(not(windows))]
impl<T> NoConsoleExt for T {
    fn no_console(&mut self) -> &mut Self {
        self
    }
}

pub struct AgentHandle {
    pub ops: async_channel::Sender<Op>,
    pub events: async_channel::Receiver<Event>,
    _thread: std::thread::JoinHandle<()>,
}

impl AgentHandle {
    pub fn shutdown(&self) {
        let _ = self.ops.send_blocking(Op::Shutdown);
    }
}

impl Drop for AgentHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Start the agent thread. `config_path` None reads the default ~/.pigcode/config.toml.
pub fn spawn_agent(config_path: Option<PathBuf>, cwd: PathBuf) -> AgentHandle {
    spawn_agent_with_data_dir(config_path, cwd, data_dir())
}

/// Full-chain network probe (triggered by PIG_NET_TEST=full in pig-app):
/// spawn_agent + a real message send (with system prompt and tools), printing the
/// timestamped event stream until the turn ends, for diagnosing "turn does not complete"
/// issues.
pub fn net_test_full_turn(config_path: Option<PathBuf>) {
    let cwd = std::env::current_dir().expect("cwd");
    let agent = spawn_agent(config_path, cwd);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(async {
        agent
            .ops
            .send(Op::NewSession {
                cwd: std::env::current_dir().expect("cwd"),
                provider_id: None,
                model_id: None,
                reasoning_level: None,
                exec_mode: None,
                plan_enabled: None,
            })
            .await
            .expect("send NewSession");
        let start = std::time::Instant::now();
        let mut session_id = String::new();
        let mut sent = false;
        loop {
            let event =
                match tokio::time::timeout(std::time::Duration::from_secs(90), agent.events.recv())
                    .await
                {
                    Ok(Ok(event)) => event,
                    _ => {
                        println!("[net-test] timeout: turn did not finish within 90s");
                        break;
                    }
                };
            let elapsed = start.elapsed().as_millis();
            let label = match &event {
                Event::SessionConfigured {
                    session_id: sid, ..
                } => {
                    session_id = sid.clone();
                    format!("SessionConfigured({sid})")
                }
                Event::TurnStarted { .. } => "TurnStarted".to_string(),
                Event::TextDelta { delta, .. } => {
                    format!("TextDelta({} chars)", delta.chars().count())
                }
                Event::TextDone { .. } => "TextDone".to_string(),
                Event::ReasoningDelta { delta, .. } => {
                    format!("ReasoningDelta({} chars)", delta.chars().count())
                }
                Event::ToolCallBegin { tool, .. } => format!("ToolCallBegin({tool})"),
                Event::ToolCallEnd { is_error, .. } => {
                    format!("ToolCallEnd(is_error={is_error})")
                }
                Event::ApprovalRequested {
                    request_id, tool, ..
                } => {
                    // The probe auto-approves so continuation-turn requests (with thinking passed back) really happen
                    let request_id = request_id.clone();
                    agent
                        .ops
                        .send(Op::ApprovalReply {
                            request_id,
                            decision: pig_protocol::ApprovalDecision::Allow,
                            feedback: None,
                        })
                        .await
                        .expect("send ApprovalReply");
                    format!("ApprovalRequested({tool}) -> auto-approved")
                }
                Event::ContextUsage { used, total, .. } => format!("ContextUsage({used}/{total})"),
                Event::TurnComplete { duration_ms, .. } => {
                    format!("TurnComplete({duration_ms}ms)")
                }
                Event::TurnAborted { .. } => "TurnAborted".to_string(),
                Event::Error { error, .. } => format!("Error({error:?})"),
                other => format!("{other:?}"),
            };
            println!("[net-test] +{elapsed}ms {label}");
            if matches!(event, Event::SessionConfigured { .. }) && !sent {
                sent = true;
                agent
                    .ops
                    .send(Op::SendMessage {
                        session_id: session_id.clone(),
                        content: "Read Cargo.toml and summarize it in one sentence".to_string(),
                        files: vec![],
                        images: vec![],
                        mode: pig_protocol::ExecMode::AutoEdit,
                    })
                    .await
                    .expect("send SendMessage");
            }
            if matches!(
                event,
                Event::TurnComplete { .. } | Event::TurnAborted { .. } | Event::Error { .. }
            ) {
                break;
            }
        }
    });
    agent.shutdown();
}

/// Same as `spawn_agent`, but with an explicit data directory (for tests/self-test isolation).
pub fn spawn_agent_with_data_dir(
    config_path: Option<PathBuf>,
    cwd: PathBuf,
    data_dir: PathBuf,
) -> AgentHandle {
    let (op_tx, op_rx) = async_channel::unbounded::<Op>();
    let (event_tx, event_rx) = async_channel::unbounded::<Event>();
    let thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(2)
            .build()
            .expect("agent runtime");
        runtime.block_on(session::agent_loop(
            op_rx,
            event_tx,
            config_path,
            cwd,
            data_dir,
        ));
    });
    AgentHandle {
        ops: op_tx,
        events: event_rx,
        _thread: thread,
    }
}
