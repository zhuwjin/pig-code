//! pig-code 的 agent 引擎：Session / turn 循环 / OpenAI 兼容 provider / 工具执行。
//! 通过 `spawn_agent` 在独立线程的 tokio runtime 上运行，与 UI 用 channel 交换 Op/Event。

pub mod config;
pub mod git;
pub mod mock;
pub mod models_registry;
pub mod paths;
mod prompt;
pub mod provider;
pub mod rollout;
pub mod session;
pub mod store;
pub mod task;
pub mod tool;

use std::path::PathBuf;

use pig_protocol::{Event, Op};

pub use pig_protocol::AppConfig;

/// 数据目录：PIG_DATA_DIR 环境变量优先（自测隔离），默认 ~/.pigcode。
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

/// 启动 agent 线程。`config_path` 为 None 时读默认 ~/.pigcode/config.toml。
pub fn spawn_agent(config_path: Option<PathBuf>, cwd: PathBuf) -> AgentHandle {
    spawn_agent_with_data_dir(config_path, cwd, data_dir())
}

/// 完整链路网络探针（pig-app 的 PIG_NET_TEST=full 触发）：
/// spawn_agent + 真实发送一条消息（含系统提示词与工具），
/// 打印带时间戳的事件流直到回合结束，用于定位「回合不完成」类问题。
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
                        println!("[net-test] 超时：90s 内回合未结束");
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
                Event::TextDelta { delta, .. } => format!("TextDelta({}字)", delta.chars().count()),
                Event::TextDone { .. } => "TextDone".to_string(),
                Event::ReasoningDelta { delta, .. } => {
                    format!("ReasoningDelta({}字)", delta.chars().count())
                }
                Event::ToolCallBegin { tool, .. } => format!("ToolCallBegin({tool})"),
                Event::ToolCallEnd { is_error, .. } => {
                    format!("ToolCallEnd(is_error={is_error})")
                }
                Event::ApprovalRequested {
                    request_id, tool, ..
                } => {
                    // 探针自动批准，让续轮请求（带 thinking 回传）真实发生
                    let request_id = request_id.clone();
                    agent
                        .ops
                        .send(Op::ApprovalReply {
                            request_id,
                            decision: pig_protocol::ApprovalDecision::Allow,
                        })
                        .await
                        .expect("send ApprovalReply");
                    format!("ApprovalRequested({tool}) → 自动批准")
                }
                Event::ContextUsage { used, total, .. } => format!("ContextUsage({used}/{total})"),
                Event::TurnComplete { duration_ms, .. } => {
                    format!("TurnComplete({duration_ms}ms)")
                }
                Event::TurnAborted { .. } => "TurnAborted".to_string(),
                Event::Error { message, .. } => format!("Error({message})"),
                other => format!("{other:?}"),
            };
            println!("[net-test] +{elapsed}ms {label}");
            if matches!(event, Event::SessionConfigured { .. }) && !sent {
                sent = true;
                agent
                    .ops
                    .send(Op::SendMessage {
                        session_id: session_id.clone(),
                        content: "用 Read 读取 Cargo.toml，然后一句话总结".to_string(),
                        files: vec![],
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

/// 同 `spawn_agent`，但显式指定数据目录（测试/自测隔离用）。
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
