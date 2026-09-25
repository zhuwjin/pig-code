//! JSONL 会话持久化（codex rollout 式）：`{data_dir}/sessions/{session_id}.jsonl`
//! 首行 meta，之后每个 durable 单元一行；live delta 不落盘。
//! 会话索引与面板当前态（待办/文件改动/原始快照）在 store.sqlite（见 store.rs）。
//! 用户消息的图片字节在旁边的 `{session_id}.media/` 目录（ImageRef 按路径引用）。

use std::io::Write as _;
use std::path::{Path, PathBuf};

use pig_protocol::SessionMeta;
use serde::{Deserialize, Serialize};

use crate::provider::{ChatImage, ChatMsg, ToolCall};

/// 用户消息附带图片的落盘引用（媒体目录里的文件；不存 base64）
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImageRef {
    pub path: PathBuf,
    pub media_type: String,
    pub width: u32,
    pub height: u32,
}

/// 媒体目录：`{sessions}/{session_id}.media/`（与 jsonl 相邻）
pub fn media_dir(sessions_dir: &Path, session_id: &str) -> PathBuf {
    sessions_dir.join(format!("{session_id}.media"))
}

/// 媒体文件命名：目录内续排序号（1.png、2.png…；`N.orig.ext` 原图跟随主序号）。
/// 不能按消息内序号命名——同会话后续回合会从 1 重排，覆盖旧 ImageRef 指向的文件。
pub fn next_media_index(media_dir: &Path) -> usize {
    let Ok(rd) = std::fs::read_dir(media_dir) else {
        return 1;
    };
    rd.filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name();
            let stem = name.to_str()?.split('.').next()?;
            stem.parse::<usize>().ok()
        })
        .max()
        .unwrap_or(0)
        + 1
}

/// ImageRef → ChatImage（replay 重建 history 用）；文件丢失 → None
pub fn rehydrate_image(image_ref: &ImageRef) -> Option<ChatImage> {
    let bytes = std::fs::read(&image_ref.path).ok()?;
    let label = image_ref
        .path
        .file_name()
        .map(|n| n.to_string_lossy().to_string());
    Some(ChatImage {
        media_type: image_ref.media_type.clone(),
        data_base64: crate::tool::base64_encode(&bytes),
        label,
    })
}

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
        /// 附带的图片：媒体文件引用（字节在 {sessions}/{id}.media/ 下，不存 base64；
        /// serde default 兼容旧记录）
        #[serde(default)]
        images: Vec<ImageRef>,
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
        /// 本次编辑的 diff（回放时恢复工具卡片的内联 diff 视图）
        #[serde(default)]
        edit: Option<pig_protocol::EditDiff>,
    },
    /// 一轮的文件改动（回放恢复消息流里的每轮改动面板）
    TurnChanges {
        files: Vec<pig_protocol::EditDiff>,
    },
    /// 一轮的 token 用量与耗时（回放恢复 footer 统计与会话累计；上下文水位由 StepUsage 恢复）。
    /// duration_ms 为回合墙钟耗时；api_ms 为纯 provider 请求耗时，
    /// ttft_ms 为其中等首 token 的时间之和、api_steps 为请求次数
    ///（平均首字 = ttft_ms / api_steps；不含首字的解码速度用 api_ms - ttft_ms）
    TurnStats {
        input: u64,
        cache_read: u64,
        output: u64,
        duration_ms: u64,
        #[serde(default)]
        api_ms: u64,
        #[serde(default)]
        ttft_ms: u64,
        #[serde(default)]
        api_steps: u64,
    },
    /// 单次 API 请求的 token 用量（每请求一条，随 Usage 事件即时落盘）。
    /// 回放恢复上下文水位：按记录顺序逐条覆盖，最后一条的 used 生效。
    /// 会话累计在 TurnStats 里，回放时本条不做累计。
    StepUsage {
        input: u64,
        cache_read: u64,
        output: u64,
        used: u64,
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

    /// resume 续写：追加模式打开已有 rollout（不覆盖 meta 与历史行）
    pub fn open_append(dir: &Path, id: &str) -> Result<Self, String> {
        let path = dir.join(format!("{id}.jsonl"));
        let file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .map_err(|e| format!("打开 rollout 失败 {}: {e}", path.display()))?;
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
            RolloutRecord::TurnStats { .. } => {}
            RolloutRecord::StepUsage { .. } => {}
            RolloutRecord::User {
                text,
                files,
                images,
            } => {
                // 新用户消息前缓冲的思考不应跨轮误挂
                pending_reasoning = None;
                let mut text = text.clone();
                if !files.is_empty() {
                    text.push_str("\n\n引用文件: ");
                    text.push_str(&files.join(", "));
                }
                // 图片按 ImageRef 读回字节重建（ZCode 式 rehydrate）；丢失的文件占位
                let mut missing = 0usize;
                let chat_images: Vec<ChatImage> = images
                    .iter()
                    .filter_map(|image_ref| {
                        let image = rehydrate_image(image_ref);
                        if image.is_none() {
                            missing += 1;
                        }
                        image
                    })
                    .collect();
                if missing > 0 {
                    text.push_str(&format!("\n\n[{missing} 张图片已失效]"));
                }
                let mut msg = ChatMsg::user(text);
                msg.images = chat_images;
                history.push(msg);
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
            // 每轮改动面板是纯 UI 展示数据，不进模型历史
            RolloutRecord::TurnChanges { .. } => {}
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
