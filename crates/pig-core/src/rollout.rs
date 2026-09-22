//! JSONL 会话持久化（codex rollout 式）：`{data_dir}/sessions/{session_id}.jsonl`
//! 首行 meta，之后每个 durable 单元一行；live delta 不落盘。
//! 会话索引与面板当前态（待办/文件改动/原始快照）在 store.sqlite（见 store.rs）。

use std::io::Write as _;
use std::path::{Path, PathBuf};

use pig_protocol::SessionMeta;
use serde::{Deserialize, Serialize};

use crate::provider::{ChatMsg, ToolCall};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RolloutRecord {
    Meta {
        id: String,
        cwd: PathBuf,
        title: String,
        created_at: u64,
    },
    User {
        text: String,
        files: Vec<String>,
    },
    Reasoning {
        text: String,
    },
    Text {
        text: String,
    },
    ToolCall {
        tool: String,
        summary: String,
        /// 原始参数 JSON（重建历史用）
        arguments: String,
        output: String,
        is_error: bool,
    },
    Compact {
        note: String,
        omitted: usize,
    },
}

pub struct Rollout {
    file: std::fs::File,
}

impl Rollout {
    pub fn create(dir: &Path, meta: &SessionMeta) -> Result<Self, String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("创建 sessions 目录失败: {e}"))?;
        let path = dir.join(format!("{}.jsonl", meta.id));
        let mut file = std::fs::File::create(&path)
            .map_err(|e| format!("创建 rollout 失败 {}: {e}", path.display()))?;
        let record = RolloutRecord::Meta {
            id: meta.id.clone(),
            cwd: meta.cwd.clone(),
            title: meta.title.clone(),
            created_at: meta.created_at,
        };
        Self::write_line(&mut file, &record)?;
        Ok(Self { file })
    }

    pub fn append(&mut self, record: &RolloutRecord) {
        // 持久化失败不致命：打日志继续
        let _ = Self::write_line(&mut self.file, record);
    }

    fn write_line(file: &mut std::fs::File, record: &RolloutRecord) -> Result<(), String> {
        let mut line = serde_json::to_string(record).map_err(|e| e.to_string())?;
        line.push('\n');
        file.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
        file.flush().map_err(|e| e.to_string())
    }

    pub fn load(path: &Path) -> Result<Vec<RolloutRecord>, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("读取 rollout 失败 {}: {e}", path.display()))?;
        content
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).map_err(|e| format!("rollout 行解析失败: {e}")))
            .collect()
    }
}

/// 从 rollout 记录重建可继续对话的历史。
/// 结构还原：text → assistant 消息；tool_call 依次挂到最近的 assistant 消息并追加 tool 结果；
/// reasoning 挂到紧随其后的 assistant 消息上（Anthropic thinking 模式要求回传）。
pub fn rebuild_history(records: &[RolloutRecord], system: String) -> Vec<ChatMsg> {
    let mut history = vec![ChatMsg::system(system)];
    let mut pending_reasoning: Option<String> = None;
    for record in records {
        match record {
            RolloutRecord::Meta { .. } => {}
            RolloutRecord::User { text, files } => {
                // 新用户消息前缓冲的思考不应跨轮误挂
                pending_reasoning = None;
                let mut text = text.clone();
                if !files.is_empty() {
                    text.push_str("\n\n引用文件: ");
                    text.push_str(&files.join(", "));
                }
                history.push(ChatMsg::user(text));
            }
            RolloutRecord::Reasoning { text } => {
                pending_reasoning = Some(text.clone());
            }
            RolloutRecord::Text { text } => {
                history.push(ChatMsg::assistant(
                    text.clone(),
                    vec![],
                    pending_reasoning.take(),
                ));
            }
            RolloutRecord::ToolCall {
                tool,
                arguments,
                output,
                ..
            } => {
                let call_id = format!("replay-{}", history.len());
                let wire_call = ToolCall {
                    id: call_id.clone(),
                    name: tool.clone(),
                    arguments: arguments.clone(),
                };
                if let Some(ChatMsg {
                    role,
                    tool_calls: Some(calls),
                    ..
                }) = history.last_mut()
                    && role == "assistant"
                {
                    calls.push(wire_call.to_wire());
                } else {
                    history.push(ChatMsg::assistant(
                        String::new(),
                        vec![wire_call],
                        pending_reasoning.take(),
                    ));
                }
                history.push(ChatMsg::tool_result(&call_id, output.clone()));
            }
            RolloutRecord::Compact { note, .. } => {
                history.push(ChatMsg::system(note.clone()));
            }
        }
    }
    history
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
