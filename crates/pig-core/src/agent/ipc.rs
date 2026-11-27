use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;
use pig_protocol::{Event, MessageDelivery, Op};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};

use super::runtime::new_runtime_id;
use super::{
    AgentBackendError, AgentBackendResult, AgentExecutionCapability, AgentExecutionCursor,
    AgentExecutionHandle, AgentExecutionIdentity, AgentExecutionInput, AgentExecutionReceipt,
    AgentExecutionRelease, AgentExecutionStop, AgentExecutionUpdate,
};
use crate::AgentHandle;

pub const IPC_PROTOCOL_VERSION: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IpcCapability {
    Start,
    Send,
    Stop,
    Events,
    Reconnect,
    Resume,
    StructuredOutput,
    Worktree,
}

const IPC_CAPABILITIES: [IpcCapability; 6] = [
    IpcCapability::Start,
    IpcCapability::Send,
    IpcCapability::Stop,
    IpcCapability::Events,
    IpcCapability::Reconnect,
    IpcCapability::Resume,
];

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IpcRunSpec {
    pub config_path: Option<PathBuf>,
    pub cwd: PathBuf,
    pub data_dir: PathBuf,
    pub prompt: String,
    pub resume_from_run_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IpcRunResult {
    pub status: String,
    pub stop_reason: String,
    pub text: String,
    pub turns: usize,
    pub usage: (u64, u64, u64),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IpcRunEvent {
    pub sequence: u64,
    pub event: Event,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IpcRunInfo {
    pub session_id: String,
    pub latest_sequence: u64,
    pub terminal: Option<IpcRunResult>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IpcEventBatch {
    pub events: Vec<IpcRunEvent>,
    pub latest_sequence: u64,
    pub terminal: Option<IpcRunResult>,
}

#[derive(Debug, Serialize, Deserialize)]
struct IpcRequest {
    id: String,
    protocol: u32,
    method: String,
    params: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
struct IpcResponse {
    id: String,
    result: Option<serde_json::Value>,
    error: Option<String>,
}

struct WorkerRun {
    agent: tokio::sync::Mutex<Option<AgentHandle>>,
    execution: Mutex<AgentExecutionIdentity>,
    session_id: String,
    ops: async_channel::Sender<Op>,
    output: Mutex<WorkerRunOutput>,
    journal: PathBuf,
}

#[derive(Default)]
struct WorkerRunOutput {
    events: Vec<IpcRunEvent>,
    terminal: Option<IpcRunResult>,
    text: String,
    queued: usize,
    steering: usize,
    turns: usize,
    usage: (u64, u64, u64),
    started: bool,
}

impl WorkerRun {
    fn new(
        session_id: String,
        ops: async_channel::Sender<Op>,
        journal: PathBuf,
        execution: AgentExecutionIdentity,
    ) -> Self {
        Self {
            agent: tokio::sync::Mutex::new(None),
            execution: Mutex::new(execution),
            session_id,
            ops,
            output: Mutex::new(WorkerRunOutput::default()),
            journal,
        }
    }

    fn authorize(&self, execution: &AgentExecutionIdentity) -> Result<(), String> {
        let current = self
            .execution
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if &*current == execution {
            return Ok(());
        }
        Err(format!(
            "stale_attempt: run {} is owned by attempt {}, not {}",
            execution.run.run_id, current.attempt, execution.attempt
        ))
    }

    fn record_event(&self, event: Event) -> Result<u64, String> {
        use std::io::Write as _;

        let mut output = self
            .output
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let sequence = output.events.last().map_or(1, |event| event.sequence + 1);
        let record = IpcRunEvent { sequence, event };
        let encoded = serde_json::to_string(&record).map_err(|error| error.to_string())?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.journal)
            .map_err(|error| error.to_string())?;
        writeln!(file, "{encoded}").map_err(|error| error.to_string())?;
        file.sync_data().map_err(|error| error.to_string())?;
        output.fold_event(&record.event, &self.session_id);
        output.events.push(record);
        Ok(sequence)
    }

    fn output(&self) -> std::sync::MutexGuard<'_, WorkerRunOutput> {
        self.output
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl WorkerRunOutput {
    fn fold_event(&mut self, event: &Event, session_id: &str) {
        match event {
            Event::TextDelta { delta, .. } => self.text.push_str(delta),
            Event::QueueChanged { items, .. } => self.queued = items.len(),
            Event::SteerChanged { items, .. } => self.steering = items.len(),
            Event::TurnStarted { .. } => {
                self.started = true;
                self.terminal = None;
            }
            Event::TurnComplete {
                stats,
                finish_reason,
                ..
            } => {
                let stats = stats.clone().unwrap_or_default();
                self.turns = self.turns.saturating_add(stats.api_steps as usize);
                self.usage.0 = self.usage.0.saturating_add(stats.input);
                self.usage.1 = self.usage.1.saturating_add(stats.cache_read);
                self.usage.2 = self.usage.2.saturating_add(stats.output);
                self.terminal = (self.queued == 0 && self.steering == 0).then(|| {
                    let stop_reason = finish_reason
                        .as_ref()
                        .map_or_else(|| "completed".to_string(), |reason| format!("{reason:?}"));
                    IpcRunResult {
                        status: "completed".to_string(),
                        stop_reason,
                        text: self.text.clone(),
                        turns: self.turns,
                        usage: self.usage,
                    }
                });
            }
            Event::AgentControl { event }
                if event.kind == "run_settled" && event.agent_id.as_deref() == Some(session_id) =>
            {
                let status = event
                    .payload
                    .get("status")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("failed");
                if status != "completed" {
                    self.terminal = Some(IpcRunResult {
                        status: status.to_string(),
                        stop_reason: event
                            .payload
                            .get("stop_reason")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or(status)
                            .to_string(),
                        text: self.text.clone(),
                        turns: self.turns,
                        usage: self.usage,
                    });
                }
            }
            Event::Error {
                error, fatal: true, ..
            } if !self.started => {
                self.terminal = Some(IpcRunResult {
                    status: "failed".to_string(),
                    stop_reason: pig_provider::core_error_en(error),
                    text: self.text.clone(),
                    turns: self.turns,
                    usage: self.usage,
                });
            }
            _ => {}
        }
    }
}

#[derive(Serialize, Deserialize)]
struct WorkerRunRecord {
    run_id: String,
    session_id: String,
    execution: AgentExecutionIdentity,
    spec: IpcRunSpec,
}

struct WorkerState {
    runs: Mutex<HashMap<String, Arc<WorkerRun>>>,
    state_dir: PathBuf,
}

impl WorkerState {
    fn new(state_dir: PathBuf) -> Self {
        Self {
            runs: Mutex::new(HashMap::new()),
            state_dir,
        }
    }

    fn record_path(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("run.json")
    }

    fn journal_path(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("events.jsonl")
    }

    fn run_dir(&self, run_id: &str) -> PathBuf {
        use sha2::{Digest as _, Sha256};

        let key = format!("{:x}", Sha256::digest(run_id.as_bytes()));
        self.state_dir.join("runs").join(key)
    }
}

impl WorkerState {
    fn persist_record(&self, record: &WorkerRunRecord) -> Result<(), String> {
        let path = self.record_path(&record.run_id);
        std::fs::create_dir_all(path.parent().expect("worker run parent"))
            .map_err(|error| error.to_string())?;
        let bytes = serde_json::to_vec(record).map_err(|error| error.to_string())?;
        crate::tool::atomic_write(&path, &bytes)
    }

    fn restore(&self, run_id: &str) -> Result<Arc<WorkerRun>, String> {
        let path = self.record_path(run_id);
        if !path.is_file() {
            return Err(format!("IPC run {run_id} is not registered"));
        }
        let record: WorkerRunRecord =
            serde_json::from_slice(&std::fs::read(&path).map_err(|error| error.to_string())?)
                .map_err(|error| error.to_string())?;
        if record.run_id != run_id {
            return Err(format!("IPC run record identity mismatch for {run_id}"));
        }
        let journal = self.journal_path(run_id);
        let events = if journal.is_file() {
            std::fs::read_to_string(&journal)
                .map_err(|error| error.to_string())?
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(|line| {
                    serde_json::from_str::<IpcRunEvent>(line).map_err(|error| error.to_string())
                })
                .collect::<Result<Vec<_>, _>>()?
        } else {
            Vec::new()
        };
        for (index, event) in events.iter().enumerate() {
            if event.sequence != index as u64 + 1 {
                return Err(format!(
                    "IPC run {run_id} has a non-contiguous event journal"
                ));
            }
        }
        let (ops, _) = async_channel::bounded(256);
        let run = Arc::new(WorkerRun::new(
            record.session_id,
            ops,
            journal,
            record.execution,
        ));
        {
            let mut output = run.output();
            for event in &events {
                output.fold_event(&event.event, &run.session_id);
            }
            if output.terminal.is_none() {
                output.terminal = Some(IpcRunResult {
                    status: "failed".to_string(),
                    stop_reason: "worker restarted before the run settled".to_string(),
                    text: output.text.clone(),
                    turns: 0,
                    usage: (0, 0, 0),
                });
            }
            output.events = events;
        }
        self.runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(run_id.to_string(), run.clone());
        Ok(run)
    }

    fn remove_records(&self, run_id: &str) -> Result<(), String> {
        let directory = self.run_dir(run_id);
        if directory.exists() {
            std::fs::remove_dir_all(directory).map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    fn run(&self, run_id: &str) -> Result<Arc<WorkerRun>, String> {
        let existing = self
            .runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(run_id)
            .cloned();
        match existing {
            Some(run) => Ok(run),
            None => self.restore(run_id),
        }
    }

    fn info(&self, run_id: &str) -> Result<IpcRunInfo, String> {
        let run = self.run(run_id)?;
        let output = run.output();
        Ok(IpcRunInfo {
            session_id: run.session_id.clone(),
            latest_sequence: output.events.last().map_or(0, |event| event.sequence),
            terminal: output.terminal.clone(),
        })
    }

    async fn start(
        &self,
        execution: AgentExecutionIdentity,
        spec: IpcRunSpec,
    ) -> Result<IpcRunInfo, String> {
        let run_id = execution.run.run_id.clone();
        if self
            .runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(&run_id)
            || self.record_path(&run_id).is_file()
        {
            return Err(format!("IPC run {run_id} already exists"));
        }
        if spec.prompt.trim().is_empty() {
            return Err("IPC run requires a non-empty prompt".to_string());
        }
        let resume_session_id = if let Some(source_id) = &spec.resume_from_run_id {
            if source_id == &run_id {
                return Err("IPC run cannot resume from itself".to_string());
            }
            let source = self.run(source_id)?;
            if source.output().terminal.is_none() {
                return Err(format!("IPC source run {source_id} is still active"));
            }
            let source_agent = source.agent.lock().await.take();
            if let Some(source_agent) = source_agent {
                source_agent.close().await?;
            }
            Some(source.session_id.clone())
        } else {
            None
        };
        let credentials = if std::env::var_os("PIG_SELFTEST").is_some() {
            Arc::new(crate::credentials::CredentialManager::mock())
        } else {
            Arc::new(crate::credentials::CredentialManager::system(
                &spec.data_dir,
            ))
        };
        let cwd = spec.cwd.clone();
        let agent = crate::spawn_agent_with_data_dir_and_credentials(
            spec.config_path.clone(),
            spec.cwd.clone(),
            spec.data_dir.clone(),
            credentials,
        );
        if let Some(session_id) = &resume_session_id {
            agent
                .ops
                .send(Op::OpenSession {
                    session_id: session_id.clone(),
                })
                .await
                .map_err(|error| error.to_string())?;
        } else {
            agent
                .ops
                .send(Op::NewSession {
                    cwd,
                    provider_id: None,
                    model_id: None,
                    reasoning_level: None,
                    permissions: Some(pig_protocol::PermissionSettings::default()),
                    plan_enabled: None,
                    bot_id: None,
                    global: false,
                })
                .await
                .map_err(|error| error.to_string())?;
        }
        let startup_events = agent.events.clone();
        let session_id = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                match startup_events
                    .recv()
                    .await
                    .map_err(|error| error.to_string())?
                {
                    Event::SessionReplayComplete { session_id }
                        if resume_session_id.as_ref() == Some(&session_id) =>
                    {
                        return Ok(session_id);
                    }
                    Event::SessionConfigured { session_id, .. } if resume_session_id.is_none() => {
                        return Ok(session_id);
                    }
                    Event::SessionReplayComplete { .. } => {
                        return Err("IPC worker opened an unexpected session".to_string());
                    }
                    Event::SessionConfigured { .. } => {}
                    Event::Error { error, .. } => {
                        return Err(pig_provider::core_error_en(&error));
                    }
                    _ => {}
                }
            }
        })
        .await
        .map_err(|_| "timed out waiting for IPC worker session".to_string())??;
        drop(startup_events);
        let events = agent.events.clone();
        let run = Arc::new(WorkerRun::new(
            session_id.clone(),
            agent.ops.clone(),
            self.journal_path(&run_id),
            execution.clone(),
        ));
        *run.agent.lock().await = Some(agent);
        if let Err(error) = self.persist_record(&WorkerRunRecord {
            run_id: run_id.clone(),
            session_id: session_id.clone(),
            execution: execution.clone(),
            spec: spec.clone(),
        }) {
            let agent = run.agent.lock().await.take();
            if let Some(agent) = agent {
                agent.close().await?;
            }
            return Err(error);
        }
        self.runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(run_id.clone(), run.clone());
        tokio::spawn(collect_events(events, run.clone()));
        if resume_session_id.is_some() {
            run.ops
                .send(Op::ResumeQueue {
                    session_id: session_id.clone(),
                })
                .await
                .map_err(|error| error.to_string())?;
        }
        run.ops
            .send(Op::SendExecutionMessage {
                session_id,
                message_id: format!("{run_id}:initial"),
                content: spec.prompt.clone(),
                permissions: pig_protocol::PermissionSettings::default(),
                delivery: MessageDelivery::Queue,
            })
            .await
            .map_err(|error| error.to_string())?;
        self.info(&run_id)
    }

    fn reconnect(&self, execution: AgentExecutionIdentity) -> Result<IpcRunInfo, String> {
        let run_id = execution.run.run_id.clone();
        let run = self.run(&run_id)?;
        let mut current = run
            .execution
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *current == execution {
            return self.info(&run_id);
        }
        if current.run != execution.run || execution.attempt <= current.attempt {
            return Err(format!(
                "stale_attempt: run {run_id} is owned by attempt {}, not {}",
                current.attempt, execution.attempt
            ));
        }
        let mut record: WorkerRunRecord = serde_json::from_slice(
            &std::fs::read(self.record_path(&run_id)).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        record.execution = execution.clone();
        self.persist_record(&record)?;
        *current = execution;
        self.info(&run_id)
    }

    async fn send(
        &self,
        run_id: &str,
        execution: &AgentExecutionIdentity,
        input: AgentExecutionInput,
    ) -> Result<IpcRunInfo, String> {
        let run = self.run(run_id)?;
        run.authorize(execution)?;
        if run.agent.lock().await.is_none() {
            return Err(format!(
                "IPC run {run_id} has no live execution after worker restart"
            ));
        }
        if run.output().terminal.is_some() {
            return Err(format!("IPC run {run_id} has already settled"));
        }
        if input.message_id.is_empty() {
            return Err("IPC execution input requires a non-empty message_id".to_string());
        }
        let text = input
            .payload
            .get("text")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "IPC execution input requires a text field".to_string())?
            .to_string();
        let delivery = input
            .payload
            .get("delivery")
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|error| error.to_string())?
            .unwrap_or(MessageDelivery::Steer);
        run.ops
            .send(Op::SendExecutionMessage {
                session_id: run.session_id.clone(),
                message_id: input.message_id,
                content: text,
                permissions: pig_protocol::PermissionSettings::default(),
                delivery,
            })
            .await
            .map_err(|error| error.to_string())?;
        self.info(run_id)
    }

    fn events(&self, run_id: &str, after: u64) -> Result<IpcEventBatch, String> {
        let run = self.run(run_id)?;
        let output = run.output();
        let events = output
            .events
            .iter()
            .filter(|event| event.sequence > after)
            .cloned()
            .collect();
        Ok(IpcEventBatch {
            events,
            latest_sequence: output.events.last().map_or(0, |event| event.sequence),
            terminal: output.terminal.clone(),
        })
    }

    async fn stop(
        &self,
        run_id: &str,
        execution: &AgentExecutionIdentity,
    ) -> Result<IpcRunInfo, String> {
        let run = self.run(run_id)?;
        run.authorize(execution)?;
        run.ops
            .send(Op::Interrupt {
                session_id: run.session_id.clone(),
            })
            .await
            .map_err(|error| error.to_string())?;
        self.info(run_id)
    }

    async fn release(
        &self,
        run_id: &str,
        execution: &AgentExecutionIdentity,
        force: bool,
    ) -> Result<(), String> {
        let run = self.run(run_id)?;
        run.authorize(execution)?;
        if !force && run.output().terminal.is_none() {
            return Err(format!("IPC run {run_id} is still active"));
        }
        let agent = run.agent.lock().await.take();
        if let Some(agent) = agent {
            agent.close().await?;
        }
        self.runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(run_id);
        self.remove_records(run_id)
    }

    async fn dispatch(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        match method {
            "capabilities" => {
                Ok(serde_json::to_value(IPC_CAPABILITIES).map_err(|error| error.to_string())?)
            }
            "start" => {
                let execution: AgentExecutionIdentity =
                    serde_json::from_value(params["execution"].clone())
                        .map_err(|error| error.to_string())?;
                let spec: IpcRunSpec = serde_json::from_value(params["spec"].clone())
                    .map_err(|error| error.to_string())?;
                Ok(serde_json::to_value(self.start(execution, spec).await?)
                    .map_err(|error| error.to_string())?)
            }
            "send" => {
                let run_id = params["run_id"].as_str().ok_or("run_id is required")?;
                let execution: AgentExecutionIdentity =
                    serde_json::from_value(params["execution"].clone())
                        .map_err(|error| error.to_string())?;
                let input: AgentExecutionInput = serde_json::from_value(params["input"].clone())
                    .map_err(|error| error.to_string())?;
                Ok(
                    serde_json::to_value(self.send(run_id, &execution, input).await?)
                        .map_err(|error| error.to_string())?,
                )
            }
            "events" => {
                let run_id = params["run_id"].as_str().ok_or("run_id is required")?;
                let after = params["after"].as_u64().unwrap_or(0);
                Ok(serde_json::to_value(self.events(run_id, after)?)
                    .map_err(|error| error.to_string())?)
            }
            "stop" => {
                let run_id = params["run_id"].as_str().ok_or("run_id is required")?;
                let execution: AgentExecutionIdentity =
                    serde_json::from_value(params["execution"].clone())
                        .map_err(|error| error.to_string())?;
                Ok(serde_json::to_value(self.stop(run_id, &execution).await?)
                    .map_err(|error| error.to_string())?)
            }
            "info" => {
                let run_id = params["run_id"].as_str().ok_or("run_id is required")?;
                Ok(serde_json::to_value(self.info(run_id)?).map_err(|error| error.to_string())?)
            }
            "reconnect" => {
                let execution: AgentExecutionIdentity =
                    serde_json::from_value(params["execution"].clone())
                        .map_err(|error| error.to_string())?;
                Ok(serde_json::to_value(self.reconnect(execution)?)
                    .map_err(|error| error.to_string())?)
            }
            "release" => {
                let run_id = params["run_id"].as_str().ok_or("run_id is required")?;
                let execution: AgentExecutionIdentity =
                    serde_json::from_value(params["execution"].clone())
                        .map_err(|error| error.to_string())?;
                let force = params["force"].as_bool().unwrap_or(false);
                self.release(run_id, &execution, force).await?;
                Ok(serde_json::json!({ "released": true }))
            }
            other => Err(format!("unknown IPC method: {other}")),
        }
    }
}

async fn collect_events(receiver: async_channel::Receiver<Event>, run: Arc<WorkerRun>) {
    while let Ok(event) = receiver.recv().await {
        if let Err(error) = run.record_event(event) {
            tracing::warn!(%error, "failed to persist IPC worker event");
            let mut output = run.output();
            output.terminal = Some(IpcRunResult {
                status: "failed".to_string(),
                stop_reason: format!("failed to persist IPC worker event: {error}"),
                text: output.text.clone(),
                turns: 0,
                usage: (0, 0, 0),
            });
            break;
        }
    }
}

async fn handle_connection(stream: TcpStream, state: Arc<WorkerState>) -> Result<(), String> {
    let (reader, writer) = stream.into_split();
    read_worker_connection(reader, writer, state).await
}

async fn read_worker_connection(
    reader: OwnedReadHalf,
    mut writer: OwnedWriteHalf,
    state: Arc<WorkerState>,
) -> Result<(), String> {
    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await.map_err(|error| error.to_string())? {
        let response = match serde_json::from_str::<IpcRequest>(&line) {
            Ok(request) if request.protocol == IPC_PROTOCOL_VERSION => {
                let result = state.dispatch(&request.method, request.params).await;
                IpcResponse {
                    id: request.id,
                    result: result.as_ref().ok().cloned(),
                    error: result.err(),
                }
            }
            Ok(request) => IpcResponse {
                id: request.id,
                result: None,
                error: Some("unsupported IPC protocol version".to_string()),
            },
            Err(error) => IpcResponse {
                id: String::new(),
                result: None,
                error: Some(error.to_string()),
            },
        };
        let mut encoded = serde_json::to_string(&response).map_err(|error| error.to_string())?;
        encoded.push('\n');
        writer
            .write_all(encoded.as_bytes())
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

pub async fn serve_ipc_worker(listener: TcpListener, state_dir: PathBuf) -> Result<(), String> {
    let state = Arc::new(WorkerState::new(state_dir));
    loop {
        let (stream, _) = listener.accept().await.map_err(|error| error.to_string())?;
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(error) = handle_connection(stream, state).await {
                tracing::warn!(%error, "IPC worker connection failed");
            }
        });
    }
}

pub async fn ipc_call(
    address: SocketAddr,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let stream = TcpStream::connect(address)
        .await
        .map_err(|error| error.to_string())?;
    let (reader, mut writer) = stream.into_split();
    let request = IpcRequest {
        id: new_runtime_id("ipc"),
        protocol: IPC_PROTOCOL_VERSION,
        method: method.to_string(),
        params,
    };
    let mut encoded = serde_json::to_string(&request).map_err(|error| error.to_string())?;
    encoded.push('\n');
    writer
        .write_all(encoded.as_bytes())
        .await
        .map_err(|error| error.to_string())?;
    let mut lines = BufReader::new(reader).lines();
    let line = tokio::time::timeout(std::time::Duration::from_secs(60), lines.next_line())
        .await
        .map_err(|_| format!("IPC method {method} timed out"))?
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("IPC method {method} returned no response"))?;
    let response: IpcResponse = serde_json::from_str(&line).map_err(|error| error.to_string())?;
    if response.id != request.id {
        return Err("IPC response identity mismatch".to_string());
    }
    if let Some(error) = response.error {
        return Err(error);
    }
    response
        .result
        .ok_or_else(|| format!("IPC method {method} returned no result"))
}

pub async fn ipc_capabilities(
    address: SocketAddr,
) -> Result<std::collections::BTreeSet<IpcCapability>, String> {
    let value = ipc_call(address, "capabilities", serde_json::json!({})).await?;
    serde_json::from_value(value).map_err(|error| error.to_string())
}

pub async fn ipc_start_with_requirements(
    address: SocketAddr,
    execution: AgentExecutionIdentity,
    spec: IpcRunSpec,
    requirements: std::collections::BTreeSet<IpcCapability>,
) -> Result<IpcRunInfo, String> {
    let capabilities = ipc_capabilities(address).await?;
    if let Some(missing) = requirements.difference(&capabilities).next() {
        return Err(format!(
            "IPC backend does not support required capability {missing:?}"
        ));
    }
    let value = ipc_call(
        address,
        "start",
        serde_json::json!({ "execution": execution, "spec": spec }),
    )
    .await?;
    serde_json::from_value(value).map_err(|error| error.to_string())
}

pub async fn ipc_start(
    address: SocketAddr,
    execution: AgentExecutionIdentity,
    spec: IpcRunSpec,
) -> Result<IpcRunInfo, String> {
    ipc_start_with_requirements(
        address,
        execution,
        spec,
        [
            IpcCapability::Start,
            IpcCapability::Events,
            IpcCapability::Stop,
            IpcCapability::Reconnect,
        ]
        .into_iter()
        .collect(),
    )
    .await
}

pub async fn ipc_send(
    address: SocketAddr,
    run_id: &str,
    execution: &AgentExecutionIdentity,
    input: AgentExecutionInput,
) -> Result<IpcRunInfo, String> {
    let value = ipc_call(
        address,
        "send",
        serde_json::json!({ "run_id": run_id, "execution": execution, "input": input }),
    )
    .await?;
    serde_json::from_value(value).map_err(|error| error.to_string())
}

pub async fn ipc_events(
    address: SocketAddr,
    run_id: &str,
    after: u64,
) -> Result<IpcEventBatch, String> {
    let value = ipc_call(
        address,
        "events",
        serde_json::json!({ "run_id": run_id, "after": after }),
    )
    .await?;
    serde_json::from_value(value).map_err(|error| error.to_string())
}

pub async fn ipc_stop(
    address: SocketAddr,
    execution: &AgentExecutionIdentity,
) -> Result<IpcRunInfo, String> {
    let value = ipc_call(
        address,
        "stop",
        serde_json::json!({ "run_id": execution.run.run_id, "execution": execution }),
    )
    .await?;
    serde_json::from_value(value).map_err(|error| error.to_string())
}

pub async fn ipc_reconnect(address: SocketAddr, run_id: &str) -> Result<IpcRunInfo, String> {
    let value = ipc_call(address, "info", serde_json::json!({ "run_id": run_id })).await?;
    serde_json::from_value(value).map_err(|error| error.to_string())
}

pub async fn ipc_reconnect_execution(
    address: SocketAddr,
    execution: AgentExecutionIdentity,
) -> Result<IpcRunInfo, String> {
    let value = ipc_call(
        address,
        "reconnect",
        serde_json::json!({ "execution": execution }),
    )
    .await?;
    serde_json::from_value(value).map_err(|error| error.to_string())
}

pub async fn ipc_release(
    address: SocketAddr,
    execution: &AgentExecutionIdentity,
    force: bool,
) -> Result<(), String> {
    ipc_call(
        address,
        "release",
        serde_json::json!({
            "run_id": execution.run.run_id,
            "execution": execution,
            "force": force,
        }),
    )
    .await
    .map(|_| ())
}

fn ipc_execution_error(detail: String) -> AgentBackendError {
    if detail.starts_with("stale_attempt:") {
        AgentBackendError::InvalidContext(detail)
    } else {
        AgentBackendError::Disconnected(detail)
    }
}

fn ipc_handle_result(result: IpcRunResult) -> Result<AgentBackendResult, AgentBackendError> {
    let status = match result.status.as_str() {
        "completed" => pig_protocol::AgentRunStatus::Completed,
        "cancelled" => pig_protocol::AgentRunStatus::Cancelled,
        "failed" => pig_protocol::AgentRunStatus::Failed,
        other => {
            return Err(AgentBackendError::Failed(format!(
                "IPC returned unknown run status {other}"
            )));
        }
    };
    Ok(AgentBackendResult {
        status,
        stop_reason: result.stop_reason,
        text: result.text,
        turns: result.turns,
        queued: false,
        usage: result.usage,
    })
}

pub struct IpcAgentExecutionHandle {
    address: SocketAddr,
    identity: AgentExecutionIdentity,
    cursor: AgentExecutionCursor,
    pending: VecDeque<IpcRunEvent>,
    terminal: Option<(AgentExecutionCursor, AgentBackendResult)>,
    detached: Option<AgentExecutionReceipt>,
    released: Option<AgentExecutionRelease>,
    stop_requested: bool,
}

impl IpcAgentExecutionHandle {
    pub async fn start(
        address: SocketAddr,
        identity: AgentExecutionIdentity,
        spec: IpcRunSpec,
    ) -> Result<Self, AgentBackendError> {
        ipc_start_with_requirements(
            address,
            identity.clone(),
            spec,
            [
                IpcCapability::Start,
                IpcCapability::Send,
                IpcCapability::Stop,
                IpcCapability::Events,
                IpcCapability::Reconnect,
            ]
            .into_iter()
            .collect(),
        )
        .await
        .map_err(AgentBackendError::Disconnected)?;
        Ok(Self::new(address, identity, 0))
    }

    pub async fn reconnect(
        address: SocketAddr,
        identity: AgentExecutionIdentity,
        cursor: AgentExecutionCursor,
    ) -> Result<Self, AgentBackendError> {
        ipc_reconnect_execution(address, identity.clone())
            .await
            .map_err(ipc_execution_error)?;
        Ok(Self::new(address, identity, cursor.sequence))
    }

    fn new(address: SocketAddr, identity: AgentExecutionIdentity, sequence: u64) -> Self {
        let cursor = AgentExecutionCursor {
            attempt: identity.attempt,
            backend_epoch: identity.backend_epoch.clone(),
            sequence,
        };
        Self {
            address,
            identity,
            cursor,
            pending: VecDeque::new(),
            terminal: None,
            detached: None,
            released: None,
            stop_requested: false,
        }
    }

    fn cursor(&self, sequence: u64) -> AgentExecutionCursor {
        AgentExecutionCursor {
            attempt: self.identity.attempt,
            backend_epoch: self.identity.backend_epoch.clone(),
            sequence,
        }
    }

    fn receipt(&self) -> AgentExecutionReceipt {
        AgentExecutionReceipt {
            cursor: self.cursor.clone(),
        }
    }

    fn check_active(&self) -> Result<(), AgentBackendError> {
        if self.detached.is_some() {
            return Err(AgentBackendError::Disconnected(
                "IPC execution is detached".to_string(),
            ));
        }
        if self.terminal.is_some() {
            return Err(AgentBackendError::InvalidContext(
                "IPC execution has already settled".to_string(),
            ));
        }
        if self.stop_requested {
            return Err(AgentBackendError::InvalidContext(
                "IPC execution is stopping".to_string(),
            ));
        }
        Ok(())
    }
}

impl AgentExecutionHandle for IpcAgentExecutionHandle {
    fn identity(&self) -> &AgentExecutionIdentity {
        &self.identity
    }

    fn capabilities(&self) -> std::collections::BTreeSet<AgentExecutionCapability> {
        [
            AgentExecutionCapability::Background,
            AgentExecutionCapability::Steer,
            AgentExecutionCapability::Reconnect,
            AgentExecutionCapability::Stop,
            AgentExecutionCapability::Detach,
        ]
        .into_iter()
        .collect()
    }

    fn send(
        &mut self,
        input: AgentExecutionInput,
    ) -> BoxFuture<'_, Result<AgentExecutionReceipt, AgentBackendError>> {
        Box::pin(async move {
            self.check_active()?;
            ipc_send(
                self.address,
                &self.identity.run.run_id,
                &self.identity,
                input,
            )
            .await
            .map_err(ipc_execution_error)?;
            Ok(self.receipt())
        })
    }

    fn stop(
        &mut self,
        stop: AgentExecutionStop,
    ) -> BoxFuture<'_, Result<AgentExecutionReceipt, AgentBackendError>> {
        Box::pin(async move {
            self.check_active()?;
            match stop {
                AgentExecutionStop::Graceful { .. } => {}
                AgentExecutionStop::Force { .. } => {
                    return Err(AgentBackendError::UnsupportedCapability {
                        backend_id: "ipc".to_string(),
                        capability: AgentExecutionCapability::ForceStop,
                    });
                }
            }
            ipc_stop(self.address, &self.identity)
                .await
                .map_err(ipc_execution_error)?;
            self.stop_requested = true;
            Ok(self.receipt())
        })
    }

    fn detach(&mut self) -> BoxFuture<'_, Result<AgentExecutionReceipt, AgentBackendError>> {
        Box::pin(async move {
            if let Some(receipt) = &self.detached {
                return Ok(receipt.clone());
            }
            let receipt = self.receipt();
            self.detached = Some(receipt.clone());
            Ok(receipt)
        })
    }

    fn next(&mut self) -> BoxFuture<'_, Result<AgentExecutionUpdate, AgentBackendError>> {
        Box::pin(async move {
            if self.detached.is_some() {
                return Err(AgentBackendError::Disconnected(
                    "IPC execution is detached".to_string(),
                ));
            }
            loop {
                if let Some(output) = self.pending.pop_front() {
                    self.cursor = self.cursor(output.sequence);
                    return Ok(AgentExecutionUpdate::Output {
                        sequence: output.sequence,
                        event: output.event,
                    });
                }
                if let Some((cursor, result)) = &self.terminal {
                    self.cursor = cursor.clone();
                    return Ok(AgentExecutionUpdate::Settled {
                        cursor: cursor.clone(),
                        result: result.clone(),
                    });
                }
                let batch = match ipc_events(
                    self.address,
                    &self.identity.run.run_id,
                    self.cursor.sequence,
                )
                .await
                {
                    Ok(batch) => batch,
                    Err(detail) => {
                        return Ok(AgentExecutionUpdate::Disconnected {
                            cursor: self.cursor.clone(),
                            detail,
                        });
                    }
                };
                self.pending.extend(batch.events);
                if let Some(terminal) = batch.terminal {
                    let cursor = self.cursor(batch.latest_sequence);
                    self.terminal = Some((cursor, ipc_handle_result(terminal)?));
                }
                if self.pending.is_empty() && self.terminal.is_none() {
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                }
            }
        })
    }

    fn release(&mut self) -> BoxFuture<'_, Result<AgentExecutionRelease, AgentBackendError>> {
        Box::pin(async move {
            if let Some(released) = &self.released {
                return Ok(released.clone());
            }
            if self.terminal.is_none() {
                let info = ipc_reconnect_execution(self.address, self.identity.clone())
                    .await
                    .map_err(ipc_execution_error)?;
                let terminal = info.terminal.ok_or_else(|| {
                    AgentBackendError::InvalidContext(
                        "IPC execution must settle before release".to_string(),
                    )
                })?;
                let cursor = self.cursor(info.latest_sequence);
                self.terminal = Some((cursor, ipc_handle_result(terminal)?));
                self.pending.clear();
            }
            ipc_release(self.address, &self.identity, false)
                .await
                .map_err(ipc_execution_error)?;
            let cursor = self
                .terminal
                .as_ref()
                .map(|(cursor, _)| cursor.clone())
                .unwrap_or_else(|| self.cursor.clone());
            self.cursor = cursor.clone();
            let released = AgentExecutionRelease {
                identity: self.identity.clone(),
                cursor,
                released_at_ms: crate::model_io::now_ms(),
            };
            self.released = Some(released.clone());
            Ok(released)
        })
    }
}

pub struct IpcAgentBackendFactory {
    address: SocketAddr,
}

impl IpcAgentBackendFactory {
    pub fn new(address: SocketAddr) -> Self {
        Self { address }
    }
}

impl super::AgentBackendFactory for IpcAgentBackendFactory {
    type Context = IpcRunSpec;

    fn capabilities(&self) -> std::collections::BTreeSet<super::AgentExecutionCapability> {
        [
            super::AgentExecutionCapability::Background,
            super::AgentExecutionCapability::Steer,
            super::AgentExecutionCapability::Resume,
            super::AgentExecutionCapability::Reconnect,
            super::AgentExecutionCapability::Stop,
            super::AgentExecutionCapability::Detach,
        ]
        .into_iter()
        .collect()
    }

    fn create(
        &self,
        context: Self::Context,
    ) -> BoxFuture<'static, Result<Box<dyn super::AgentExecutionBackend>, super::AgentBackendError>>
    {
        let backend = IpcAgentExecutionBackend {
            address: self.address,
            spec: context,
        };
        Box::pin(async move { Ok(Box::new(backend) as Box<dyn super::AgentExecutionBackend>) })
    }
}

struct ManagedIpcWorker {
    address: SocketAddr,
    child: tokio::process::Child,
}

impl Drop for ManagedIpcWorker {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

/// Production IPC backend factory registered by default under `"ipc"`.
/// Connects to `PIG_AGENT_WORKER_ADDR` if configured, or lazily launches the
/// sibling `pig-agent-worker` executable on loopback and reuses it across runs.
#[derive(Clone, Default)]
pub struct ManagedIpcAgentBackendFactory {
    worker: Arc<tokio::sync::Mutex<Option<ManagedIpcWorker>>>,
}

impl ManagedIpcAgentBackendFactory {
    fn worker_binary() -> Result<PathBuf, AgentBackendError> {
        let exe = std::env::current_exe().map_err(|error| {
            AgentBackendError::Failed(format!("failed to locate current executable: {error}"))
        })?;
        let name = if cfg!(windows) {
            "pig-agent-worker.exe"
        } else {
            "pig-agent-worker"
        };
        let mut candidates = Vec::new();
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join(name));
            // Unit/integration test binaries live under `target/debug/deps/`.
            if let Some(parent) = dir.parent() {
                candidates.push(parent.join(name));
            }
        }
        for candidate in &candidates {
            if candidate.is_file() {
                return Ok(candidate.clone());
            }
        }
        Err(AgentBackendError::Failed(format!(
            "pig-agent-worker executable not found (checked {})",
            candidates
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )))
    }

    async fn resolve_address(&self, data_dir: &std::path::Path) -> Result<SocketAddr, AgentBackendError> {
        if let Ok(raw) = std::env::var("PIG_AGENT_WORKER_ADDR") {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                return trimmed.parse::<SocketAddr>().map_err(|error| {
                    AgentBackendError::InvalidContext(format!(
                        "invalid PIG_AGENT_WORKER_ADDR `{trimmed}`: {error}"
                    ))
                });
            }
        }
        let mut guard = self.worker.lock().await;
        if let Some(existing) = guard.as_mut() {
            match existing.child.try_wait() {
                Ok(None) => return Ok(existing.address),
                Ok(Some(_)) | Err(_) => {
                    *guard = None;
                }
            }
        }
        let binary = Self::worker_binary()?;
        let state_dir = data_dir.join("worker-ipc");
        std::fs::create_dir_all(&state_dir).map_err(|error| {
            AgentBackendError::Failed(format!(
                "failed to create IPC worker state directory {}: {error}",
                state_dir.display()
            ))
        })?;
        let mut command = tokio::process::Command::new(&binary);
        command
            .arg("--bind=127.0.0.1:0")
            .arg(format!("--state-dir={}", state_dir.display()))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        crate::NoConsoleExt::no_console(&mut command);
        let mut child = command.spawn().map_err(|error| {
            AgentBackendError::Failed(format!(
                "failed to spawn pig-agent-worker {}: {error}",
                binary.display()
            ))
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            AgentBackendError::Failed("pig-agent-worker stdout pipe unavailable".to_string())
        })?;
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        let read = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            reader.read_line(&mut line),
        )
        .await
        .map_err(|_| {
            let _ = child.start_kill();
            AgentBackendError::Failed("timed out waiting for pig-agent-worker address".to_string())
        })?
        .map_err(|error| {
            let _ = child.start_kill();
            AgentBackendError::Failed(format!("failed reading pig-agent-worker stdout: {error}"))
        })?;
        if read == 0 {
            let _ = child.start_kill();
            return Err(AgentBackendError::Failed(
                "pig-agent-worker exited before announcing its listen address".to_string(),
            ));
        }
        let raw_addr = line
            .trim()
            .strip_prefix("PIG_AGENT_WORKER_ADDR=")
            .ok_or_else(|| {
                let _ = child.start_kill();
                AgentBackendError::Failed(format!(
                    "unexpected pig-agent-worker startup banner: {}",
                    line.trim()
                ))
            })?;
        let address = raw_addr.parse::<SocketAddr>().map_err(|error| {
            let _ = child.start_kill();
            AgentBackendError::Failed(format!(
                "invalid pig-agent-worker listen address `{raw_addr}`: {error}"
            ))
        })?;
        *guard = Some(ManagedIpcWorker { address, child });
        Ok(address)
    }
}

impl super::AgentBackendFactory for ManagedIpcAgentBackendFactory {
    type Context = IpcRunSpec;

    fn capabilities(&self) -> std::collections::BTreeSet<super::AgentExecutionCapability> {
        [
            super::AgentExecutionCapability::Background,
            super::AgentExecutionCapability::Steer,
            super::AgentExecutionCapability::Resume,
            super::AgentExecutionCapability::Reconnect,
            super::AgentExecutionCapability::Stop,
            super::AgentExecutionCapability::Detach,
        ]
        .into_iter()
        .collect()
    }

    fn create(
        &self,
        context: Self::Context,
    ) -> BoxFuture<'static, Result<Box<dyn super::AgentExecutionBackend>, super::AgentBackendError>>
    {
        let factory = self.clone();
        Box::pin(async move {
            let address = factory.resolve_address(&context.data_dir).await?;
            Ok(Box::new(IpcAgentExecutionBackend {
                address,
                spec: context,
            }) as Box<dyn super::AgentExecutionBackend>)
        })
    }
}

struct IpcAgentExecutionBackend {
    address: SocketAddr,
    spec: IpcRunSpec,
}

impl super::AgentExecutionBackend for IpcAgentExecutionBackend {
    fn capabilities(&self) -> std::collections::BTreeSet<super::AgentExecutionCapability> {
        [
            super::AgentExecutionCapability::Background,
            super::AgentExecutionCapability::Steer,
            super::AgentExecutionCapability::Resume,
            super::AgentExecutionCapability::Reconnect,
            super::AgentExecutionCapability::Stop,
            super::AgentExecutionCapability::Detach,
        ]
        .into_iter()
        .collect()
    }

    fn start(
        self: Box<Self>,
        request: super::AgentExecutionRequest,
    ) -> BoxFuture<'static, Result<Box<dyn AgentExecutionHandle>, AgentBackendError>> {
        Box::pin(async move {
            if request.identity != request.execution.run {
                return Err(AgentBackendError::Failed(
                    "IPC execution identity mismatch".to_string(),
                ));
            }
            let execution = request.execution.clone();
            match request.activation {
                super::AgentExecutionActivation::Start => {
                    let handle =
                        IpcAgentExecutionHandle::start(self.address, execution, self.spec).await?;
                    Ok(Box::new(handle) as Box<dyn AgentExecutionHandle>)
                }
                super::AgentExecutionActivation::Reconnect => {
                    let cursor = request.cursor.ok_or_else(|| {
                        AgentBackendError::InvalidContext(
                            "IPC reconnect requires an event cursor".to_string(),
                        )
                    })?;
                    if cursor.run != request.identity {
                        return Err(AgentBackendError::Failed(
                            "IPC reconnect cursor identity mismatch".to_string(),
                        ));
                    }
                    let handle = IpcAgentExecutionHandle::reconnect(
                        self.address,
                        execution.clone(),
                        AgentExecutionCursor {
                            attempt: execution.attempt,
                            backend_epoch: execution.backend_epoch.clone(),
                            sequence: cursor.sequence,
                        },
                    )
                    .await?;
                    Ok(Box::new(handle) as Box<dyn AgentExecutionHandle>)
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentRunIdentity;

    #[test]
    fn failed_journal_write_does_not_publish_or_advance_cursor() {
        let root = std::env::temp_dir().join(new_runtime_id("ipc-journal-fault"));
        let journal = root.join("events.jsonl");
        std::fs::create_dir_all(&journal).expect("journal fault injection directory");
        let (ops, _receiver) = async_channel::unbounded();
        let run = WorkerRun::new(
            "session".into(),
            ops,
            journal,
            AgentExecutionIdentity {
                run: AgentRunIdentity {
                    session_id: "session".into(),
                    agent_id: "agent".into(),
                    run_id: "run".into(),
                },
                attempt: 1,
                backend_epoch: "epoch".into(),
            },
        );
        let error = run
            .record_event(Event::TextDelta {
                session_id: "session".into(),
                item_id: "item".into(),
                delta: "must not become visible".into(),
            })
            .expect_err("writing a file over a directory must fail");
        assert!(!error.is_empty());
        let output = run.output();
        assert!(output.events.is_empty());
        assert!(output.text.is_empty());
        assert!(output.terminal.is_none());
        drop(output);
        std::fs::remove_dir_all(root).expect("remove journal fault injection directory");
    }

    #[test]
    fn terminal_worker_output_waits_for_the_root_run_settlement() {
        let mut output = WorkerRunOutput::default();
        output.fold_event(
            &Event::TurnStarted {
                session_id: "root".into(),
                turn_id: "turn".into(),
                run: None,
            },
            "root",
        );
        output.fold_event(
            &Event::TurnAborted {
                session_id: "root".into(),
                run: None,
            },
            "root",
        );
        assert!(output.terminal.is_none());
        let settled = |agent_id: &str| Event::AgentControl {
            event: pig_protocol::AgentControlEvent {
                session_id: "root".into(),
                sequence: 1,
                event_id: "settled".into(),
                kind: "run_settled".into(),
                agent_id: Some(agent_id.into()),
                run_id: Some("run".into()),
                attempt: Some(1),
                backend_epoch: Some("epoch".into()),
                payload: serde_json::json!({"status":"cancelled", "stop_reason":"cancelled"}),
                created_at_ms: 1,
            },
        };
        output.fold_event(&settled("child"), "root");
        assert!(output.terminal.is_none(), "a child cannot settle the root");
        output.fold_event(&settled("root"), "root");
        assert_eq!(
            output
                .terminal
                .as_ref()
                .map(|result| result.status.as_str()),
            Some("cancelled")
        );

        let mut rejected = WorkerRunOutput::default();
        rejected.fold_event(
            &Event::Error {
                session_id: Some("root".into()),
                error: pig_protocol::CoreError::NoModelConfigured,
                fatal: true,
            },
            "root",
        );
        assert_eq!(
            rejected
                .terminal
                .as_ref()
                .map(|result| result.status.as_str()),
            Some("failed"),
            "a pre-start rejection must not wait forever"
        );
    }
}
