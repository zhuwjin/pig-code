//! pig-code 的 agent 引擎：Session / turn 循环 / OpenAI 兼容 provider / 工具执行。
//! 通过 `spawn_agent` 在独立线程的 tokio runtime 上运行，与 UI 用 channel 交换 Op/Event。

pub mod config;
pub mod git;
pub mod mock;
pub mod project;
pub mod rollout;
mod prompt;
pub mod provider;
pub mod session;
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
        runtime.block_on(session::agent_loop(op_rx, event_tx, config_path, cwd, data_dir));
    });
    AgentHandle {
        ops: op_tx,
        events: event_rx,
        _thread: thread,
    }
}
