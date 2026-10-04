//! 模型调用轨迹落盘：`{sessions_dir}/{session_id}.model-io.jsonl`，每行一次
//! provider 调用（对齐 ZCode 的 model-io：UI「查看调用轨迹」按会话 id 直读
//! 该文件还原请求/响应/工具调用，不依赖 rollout 快照）。

use crate::provider::{ChatMsg, ToolCall};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 单次调用的输入消息投影：只留文本与结构信息，图片不落 base64
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ModelIoMessage {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// assistant 消息携带的工具调用（仅 id + 名称）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<String>,
    /// tool 角色的对应调用 id
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// 随消息进上下文的图片张数（内容不入轨迹）
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
    /// 模型上报的累计上下文占用（水位），未知为 0
    #[serde(default)]
    pub used: u64,
    #[serde(default)]
    pub total: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ModelIoToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// 一次 provider 调用的完整记录（正常/工具调用/失败/取消都落盘）
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelIoRecord {
    /// 落盘时刻（毫秒 Unix 时间戳）
    pub ts_ms: u64,
    /// 回合/步骤标识（如 "{turn_id}-s{step}"）
    pub turn: String,
    /// 调用来源：main = 主会话步骤（后续可扩展 subagent/compact/title）
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
    /// 增量输入：本条 input 只存与上一条完整输入不同的尾部，
    /// 此前缀条数与上一条共享（对齐 ZCode model-io 的 delta 存储；
    /// 0 = 全量。读取时由 read_all 顺序展开还原）
    #[serde(default)]
    pub input_offset: usize,
    pub input: Vec<ModelIoMessage>,
}

/// 轨迹文件路径（调用方持有 sessions_dir，与会话 rollout 同目录）
pub fn model_io_path(sessions_dir: &Path, session_id: &str) -> PathBuf {
    sessions_dir.join(format!("{session_id}.model-io.jsonl"))
}

/// 追加一条调用记录。目录不存在则创建；失败返回 Err（调用方按非致命处理，
/// 与 rollout.append 同口径）。
pub fn append(sessions_dir: &Path, session_id: &str, record: &ModelIoRecord) -> Result<(), String> {
    use std::io::Write as _;
    std::fs::create_dir_all(sessions_dir)
        .map_err(|e| format!("创建会话目录失败 {}: {e}", sessions_dir.display()))?;
    let path = model_io_path(sessions_dir, session_id);
    let mut line = serde_json::to_string(record).map_err(|e| e.to_string())?;
    line.push('\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("打开调用轨迹失败 {}: {e}", path.display()))?;
    file.write_all(line.as_bytes())
        .map_err(|e| format!("写入调用轨迹失败 {}: {e}", path.display()))
}

/// 读入全部调用记录（文件缺失 = 空列表），并把增量输入顺序展开成完整序列
///（record N 的完整输入 = record N-1 完整输入前 input_offset 条 + 本条 delta）。
/// 展开链断裂（offset 超出上一条长度，不应发生）时按全量兜底。坏行跳过并
/// eprintln 行号——轨迹是诊断数据，不能因一条坏行整页打不开。
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
                eprintln!("调用轨迹第 {} 行解析失败，已跳过: {e}", ix + 1);
                continue;
            }
        };
        // delta 展开：拼上上一条的共享前缀
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

/// 两条输入投影的公共前缀条数（逐条字段全等；delta 落盘的切分点）
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

/// 请求前的输入历史 → 轨迹投影（深内容截断防单条撑爆文件：字符边界取前 4000）
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

/// 响应累积的工具调用 → 轨迹投影（参数 JSON 可能很长，同样截断）
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

/// 当前 Unix 毫秒时间戳（轨迹记录与 UI 展示用）
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
            },
            finish: "tool_calls".into(),
            error: None,
            reasoning: "想".into(),
            text: "答".into(),
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
        // 文件缺失 = 空
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
        // 第二条只存 delta：共享前缀 2 条，新增 assistant + user
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
        // 首条全量
        assert_eq!(all[0].input.len(), 2);
        // 次条展开 = 前缀 2 + delta 2
        assert_eq!(all[1].input.len(), 4);
        assert_eq!(all[1].input[0].role, "system");
        assert_eq!(all[1].input[2].content.as_deref(), Some("a1"));
        // 前缀计算：公共前缀 2
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
