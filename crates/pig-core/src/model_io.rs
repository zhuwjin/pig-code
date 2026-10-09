//! Model-call trace persistence: `{sessions_dir}/{session_id}.model-io.jsonl`, one line per
//! provider call (aligned with ZCode's model-io: the UI's "view call trace" reads this file
//! directly by session id to restore requests/responses/tool calls, without relying on rollout snapshots).

use crate::provider::{ChatMsg, ToolCall};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Input-message projection for a single call: keeps only text and structural info; images are not persisted as base64
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ModelIoMessage {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Tool calls carried by the assistant message (id + name only)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<String>,
    /// The corresponding call id for the tool role
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Number of images entering context with the message (content is not recorded in the trace)
    #[serde(default, skip_serializing_if = "usize_is_zero")]
    pub images: usize,
}

fn usize_is_zero(n: &usize) -> bool {
    *n == 0
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ModelIoUsage {
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub output: u64,
    /// Cumulative context occupancy reported by the model (usage watermark), 0 when unknown
    #[serde(default)]
    pub used: u64,
    #[serde(default)]
    pub total: u64,
    /// Reasoning/thinking slice of output (informational split; 0 on old
    /// records / providers that don't report it)
    #[serde(default)]
    pub reasoning_output: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ModelIoToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// Complete record of one provider call (normal/tool-call/failure/cancellation are all persisted)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelIoRecord {
    /// Persistence time (Unix timestamp in milliseconds)
    pub ts_ms: u64,
    /// Turn/step identifier (e.g. "{turn_id}-s{step}")
    pub turn: String,
    /// Call source: main = main-session step (extensible to subagent/compact/title later)
    pub source: String,
    pub provider: String,
    pub model: String,
    pub duration_ms: u64,
    pub ttft_ms: u64,
    pub usage: ModelIoUsage,
    /// stop / tool_calls / error / cancelled
    pub finish: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reasoning: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ModelIoToolCall>,
    /// Delta input: this record's input stores only the tail differing from the previous record's
    /// full input; a prefix of that many entries is shared with the previous record (aligned with
    /// ZCode model-io's delta storage; 0 = full. read_all expands it back in order when reading)
    #[serde(default)]
    pub input_offset: usize,
    pub input: Vec<ModelIoMessage>,
}

/// Trace file path (the caller holds sessions_dir; same directory as the session rollout)
pub fn model_io_path(sessions_dir: &Path, session_id: &str) -> PathBuf {
    sessions_dir.join(format!("{session_id}.model-io.jsonl"))
}

/// Append one call record. Creates the directory if missing; failure returns Err (the caller
/// treats it as non-fatal, same policy as rollout.append).
pub fn append(sessions_dir: &Path, session_id: &str, record: &ModelIoRecord) -> Result<(), String> {
    use std::io::Write as _;
    std::fs::create_dir_all(sessions_dir).map_err(|e| {
        format!(
            "Failed to create session directory {}: {e}",
            sessions_dir.display()
        )
    })?;
    let path = model_io_path(sessions_dir, session_id);
    let mut line = serde_json::to_string(record).map_err(|e| e.to_string())?;
    line.push('\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("Failed to open model io trace {}: {e}", path.display()))?;
    file.write_all(line.as_bytes())
        .map_err(|e| format!("Failed to write model io trace {}: {e}", path.display()))
}

/// Read all call records (missing file = empty list), expanding delta inputs in order into full
/// sequences (record N's full input = the first input_offset entries of record N-1's full input + this record's delta).
/// If the expansion chain breaks (offset exceeds the previous record's length, which should not happen), fall
/// back to treating it as full. Bad lines are skipped with the line number logged at warn level — the trace is diagnostic
/// data, and one bad line must not make the whole page unopenable.
pub fn read_all(path: &Path) -> Vec<ModelIoRecord> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return vec![];
    };
    let mut records: Vec<ModelIoRecord> = vec![];
    for (ix, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let mut record: ModelIoRecord = match serde_json::from_str(line) {
            Ok(record) => record,
            Err(e) => {
                tracing::warn!(
                    "failed to parse model io trace line {}, skipped: {e}",
                    ix + 1
                );
                continue;
            }
        };
        // Delta expansion: prepend the previous record's shared prefix
        if let Some(previous) = records.last().filter(|_| record.input_offset > 0) {
            let offset = record.input_offset.min(previous.input.len());
            let mut full = previous.input[..offset].to_vec();
            full.append(&mut record.input);
            record.input = full;
        }
        records.push(record);
    }
    records
}

/// Number of common-prefix entries between two input projections (all fields equal per entry; the split point for delta persistence)
pub fn common_prefix_len(a: &[ModelIoMessage], b: &[ModelIoMessage]) -> usize {
    a.iter()
        .zip(b)
        .take_while(|(x, y)| {
            x.role == y.role
                && x.content == y.content
                && x.tool_calls == y.tool_calls
                && x.tool_call_id == y.tool_call_id
                && x.images == y.images
        })
        .count()
}

/// Pre-request input history -> trace projection (deep content truncated to keep one record from bloating the file: first 4000 chars at a char boundary)
pub fn project_input(messages: &[ChatMsg]) -> Vec<ModelIoMessage> {
    messages
        .iter()
        .map(|msg| ModelIoMessage {
            role: msg.role.clone(),
            content: msg
                .content
                .as_ref()
                .map(|c| c.chars().take(4000).collect::<String>()),
            tool_calls: msg
                .tool_calls
                .as_ref()
                .map(|calls| calls.iter().map(|c| c.function.name.clone()).collect())
                .unwrap_or_default(),
            tool_call_id: msg.tool_call_id.clone(),
            images: msg.images.len(),
        })
        .collect()
}

/// Response-accumulated tool calls -> trace projection (argument JSON can be long; truncated likewise)
pub fn project_tool_calls(calls: &[ToolCall]) -> Vec<ModelIoToolCall> {
    calls
        .iter()
        .map(|call| ModelIoToolCall {
            id: call.id.clone(),
            name: call.name.clone(),
            arguments: call.arguments.chars().take(4000).collect(),
        })
        .collect()
}

/// Current Unix millisecond timestamp (for trace records and UI display)
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ModelIoRecord {
        ModelIoRecord {
            ts_ms: 1,
            turn: "t1-s0".into(),
            source: "main".into(),
            provider: "p".into(),
            model: "m".into(),
            duration_ms: 12,
            ttft_ms: 3,
            usage: ModelIoUsage {
                input: 10,
                cache_read: 5,
                output: 7,
                used: 22,
                total: 1000,
                reasoning_output: 0,
            },
            finish: "tool_calls".into(),
            error: None,
            reasoning: "thinking".into(),
            text: "answer".into(),
            tool_calls: vec![ModelIoToolCall {
                id: "c1".into(),
                name: "Bash".into(),
                arguments: "{}".into(),
            }],
            input_offset: 0,
            input: vec![ModelIoMessage {
                role: "user".into(),
                content: Some("hi".into()),
                ..Default::default()
            }],
        }
    }

    #[test]
    fn append_and_read_round_trip() {
        let dir = std::env::temp_dir().join(format!("pig-model-io-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let record = sample();
        append(&dir, "s1", &record).unwrap();
        append(&dir, "s1", &record).unwrap();
        let path = model_io_path(&dir, "s1");
        let all = read_all(&path);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].turn, "t1-s0");
        assert_eq!(all[0].usage.input, 10);
        assert_eq!(all[0].tool_calls[0].name, "Bash");
        assert_eq!(all[0].input[0].content.as_deref(), Some("hi"));
        // Missing file = empty
        assert!(read_all(&dir.join("nope.jsonl")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delta_input_expanded_on_read() {
        let dir = std::env::temp_dir().join(format!("pig-model-io-delta-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut first = sample();
        first.input = vec![
            ModelIoMessage {
                role: "system".into(),
                content: Some("sys".into()),
                ..Default::default()
            },
            ModelIoMessage {
                role: "user".into(),
                content: Some("q1".into()),
                ..Default::default()
            },
        ];
        // The second record stores only the delta: 2 shared-prefix entries, plus a new assistant + user
        let mut second = sample();
        second.input_offset = 2;
        second.input = vec![
            ModelIoMessage {
                role: "assistant".into(),
                content: Some("a1".into()),
                ..Default::default()
            },
            ModelIoMessage {
                role: "user".into(),
                content: Some("q2".into()),
                ..Default::default()
            },
        ];
        append(&dir, "s1", &first).unwrap();
        append(&dir, "s1", &second).unwrap();
        let all = read_all(&model_io_path(&dir, "s1"));
        assert_eq!(all.len(), 2);
        // First record is full
        assert_eq!(all[0].input.len(), 2);
        // Second record expanded = prefix 2 + delta 2
        assert_eq!(all[1].input.len(), 4);
        assert_eq!(all[1].input[0].role, "system");
        assert_eq!(all[1].input[2].content.as_deref(), Some("a1"));
        // Prefix computation: common prefix 2
        assert_eq!(common_prefix_len(&first.input, &all[1].input), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_lines_skipped() {
        let dir = std::env::temp_dir().join(format!("pig-model-io-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            model_io_path(&dir, "s1"),
            "not json\n{\"ts_ms\":1,\"turn\":\"t\",\"source\":\"main\",\"provider\":\"p\",\"model\":\"m\",\"duration_ms\":0,\"ttft_ms\":0,\"usage\":{},\"finish\":\"stop\",\"input\":[]}\n\n",
        )
        .unwrap();
        let all = read_all(&model_io_path(&dir, "s1"));
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].finish, "stop");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
