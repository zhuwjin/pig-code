//! JSONL session persistence (codex-rollout style): `{data_dir}/sessions/{session_id}.jsonl`
//! with a first meta line, then one line per durable unit; live deltas are not persisted.
//! The session index and panel current state (todos/file changes/raw snapshots) live in
//! store.sqlite (see store.rs). Image bytes of user messages live in the adjacent
//! `{session_id}.media/` directory (ImageRef references them by path).

use std::io::Write as _;
use std::path::{Path, PathBuf};

use pig_protocol::{CoreError, SessionMeta};
use serde::{Deserialize, Serialize};

use crate::provider::{ChatImage, ChatMsg, ToolCall};

/// Persisted reference to an image attached to a user message (a file in the media directory; base64 is not stored)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImageRef {
    pub path: PathBuf,
    pub media_type: String,
    pub width: u32,
    pub height: u32,
}

/// Media directory: `{sessions}/{session_id}.media/` (adjacent to the jsonl)
pub fn media_dir(sessions_dir: &Path, session_id: &str) -> PathBuf {
    sessions_dir.join(format!("{session_id}.media"))
}

/// Media file naming: continuing sequence numbers inside the directory (1.png, 2.png...;
/// `N.orig.ext` originals follow the main number). They must not be named by the in-message
/// index — later turns in the same session renumber from 1 and would overwrite the files old
/// ImageRefs point to.
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

/// ImageRef -> ChatImage (for replay history rebuild); missing file -> None
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

/// Media-file numbers of the images attached to a user message (the N of
/// {sessions}/{id}.media/{N}.ext): the UserMessage event uses them so the UI can load
/// thumbnails (the link text is no longer embedded in the event text — clean body text goes
/// to the model/rollout, the display layer renders by number, decoupled from the UI
/// language).
pub fn image_nums(image_refs: &[ImageRef]) -> Vec<u32> {
    image_refs
        .iter()
        .filter_map(|image_ref| image_ref.path.file_stem()?.to_str()?.parse().ok())
        .collect()
}

// The persistence format favors readability/evolvability: the ToolCall variant (with the
// batch agent-card list) being several hundred bytes larger than the other variants is
// expected; records are transient serialization units, no Box split just to save memory
#[allow(clippy::large_enum_variant)]
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
        /// Attached images: media-file references (bytes under {sessions}/{id}.media/, base64 not stored)
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
        /// Raw arguments JSON (for history rebuild)
        arguments: String,
        output: String,
        is_error: bool,
        /// Diff of this edit (restores the tool card's inline diff view on replay)
        edit: Option<pig_protocol::EditDiff>,
        /// Agent-card metadata for the Agent tool card (replay rebuilds the agent card)
        agent_card: Option<AgentCardRecord>,
        /// Agent-card metadata for the AgentSwarm tool card (one per subagent; replay
        /// rebuilds all)
        agent_cards: Vec<AgentCardRecord>,
    },
    /// A turn's file changes (replay restores the per-turn changes panel in the message stream)
    TurnChanges {
        files: Vec<pig_protocol::EditDiff>,
    },
    /// A turn's token usage and timing (replay restores footer stats and session totals; the
    /// context usage watermark is restored by StepUsage). duration_ms is the turn's
    /// wall-clock time; api_ms is pure provider request time; ttft_ms is the summed
    /// time-to-first-token within it and api_steps the request count (average first token =
    /// ttft_ms / api_steps; decode speed excluding the first token uses api_ms - ttft_ms)
    TurnStats {
        input: u64,
        cache_read: u64,
        output: u64,
        duration_ms: u64,
        api_ms: u64,
        ttft_ms: u64,
        api_steps: u64,
    },
    /// Token usage of a single API request (one record per request, persisted immediately
    /// with the Usage event). Replay restores the context usage watermark: applied record by
    /// record in order, the last record's used wins. Session totals live in TurnStats; this
    /// record is not accumulated at replay.
    StepUsage {
        input: u64,
        cache_read: u64,
        output: u64,
        used: u64,
    },
    Compact {
        note: String,
        omitted: usize,
        /// true = auto-triggered at the pre-sampling watermark; false = user-initiated /compact (replay display only)
        automatic: bool,
        /// Estimated token usage watermark of the post-compact history (replay restores
        /// last_total_tokens, so that after reopening, the watermark check does not
        /// immediately fire another auto-compact on the stale pre-compact high value)
        used_after: Option<u64>,
    },
}

/// Agent-card metadata for the Agent tool card (persisted with RolloutRecord::ToolCall, for replay rebuild)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentCardRecord {
    pub agent_id: String,
    pub profile: String,
    pub description: String,
    /// "{provider_name} · {model}" (may carry a reasoning-level suffix)
    pub model: String,
    /// This run is background: the background card's running state is driven by the subagent
    /// lifecycle; after replay, core re-emits SubagentActivity finished to settle the final
    /// state (tasks do not outlive the process)
    pub background: bool,
}

pub struct Rollout {
    file: std::fs::File,
}

impl Rollout {
    pub fn create(dir: &Path, meta: &SessionMeta) -> Result<Self, CoreError> {
        std::fs::create_dir_all(dir).map_err(|e| CoreError::SessionsDirCreate {
            detail: e.to_string(),
        })?;
        let path = dir.join(format!("{}.jsonl", meta.id));
        let mut file = std::fs::File::create(&path).map_err(|e| CoreError::RolloutCreate {
            path: path.display().to_string(),
            detail: e.to_string(),
        })?;
        let record = RolloutRecord::Meta {
            id: meta.id.clone(),
            cwd: meta.cwd.clone(),
            title: meta.title.clone(),
            created_at: meta.created_at,
        };
        Self::write_line(&mut file, &record).map_err(|e| CoreError::RolloutCreate {
            path: path.display().to_string(),
            detail: e,
        })?;
        Ok(Self { file })
    }

    /// resume continuation: opens the existing rollout in append mode (does not overwrite meta or history lines)
    pub fn open_append(dir: &Path, id: &str) -> Result<Self, CoreError> {
        let path = dir.join(format!("{id}.jsonl"));
        let file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .map_err(|e| CoreError::RolloutOpen {
                path: path.display().to_string(),
                detail: e.to_string(),
            })?;
        Ok(Self { file })
    }

    pub fn append(&mut self, record: &RolloutRecord) {
        // Persistence failure is non-fatal: log and continue
        let _ = Self::write_line(&mut self.file, record);
    }

    fn write_line(file: &mut std::fs::File, record: &RolloutRecord) -> Result<(), String> {
        let mut line = serde_json::to_string(record).map_err(|e| e.to_string())?;
        line.push('\n');
        file.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
        file.flush().map_err(|e| e.to_string())
    }

    pub fn load(path: &Path) -> Result<Vec<RolloutRecord>, CoreError> {
        let content = std::fs::read_to_string(path).map_err(|e| CoreError::RolloutRead {
            path: path.display().to_string(),
            detail: e.to_string(),
        })?;
        content
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                serde_json::from_str(line).map_err(|e| CoreError::RolloutLineParse {
                    detail: e.to_string(),
                })
            })
            .collect()
    }
}

/// Rebuild a continuable conversation history from rollout records.
/// Structure restoration: text -> assistant message; tool_call attaches to the nearest
/// preceding assistant message in order, appending the tool result; reasoning attaches to
/// the assistant message immediately following it (Anthropic thinking mode requires passing
/// it back).
pub fn rebuild_history(records: &[RolloutRecord], system: String) -> Vec<ChatMsg> {
    let mut history = vec![ChatMsg::system(system)];
    let mut pending_reasoning: Option<String> = None;
    let meta_cwd = records.iter().find_map(|record| match record {
        RolloutRecord::Meta { cwd, .. } => Some(cwd.clone()),
        _ => None,
    });
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
                // Buffered reasoning must not wrongly attach across turns before a new user message
                pending_reasoning = None;
                let mut text = text.clone();
                if !files.is_empty() {
                    // Same pointer shape as live (path + size, no content read); old records
                    // (body-embedded <file> blocks or a "Referenced files" suffix) stay
                    // as-is — the history is the message
                    if let Some(cwd) = &meta_cwd {
                        text = crate::session::pointer_file_references(cwd, &text, files);
                    } else {
                        text.push_str("\n\nReferenced files: ");
                        text.push_str(&files.join(", "));
                    }
                }
                // Images are rebuilt by reading bytes back from ImageRef (ZCode-style rehydrate); missing files get a placeholder
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
                    text.push_str(&format!("\n\n[{missing} image(s) no longer available]"));
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
            // The per-turn changes panel is pure UI display data, never enters model history
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Batch cards serialize/read back with the record (the data source for swarm replay rebuild)
    #[test]
    fn tool_call_agent_cards_roundtrip() {
        let card = |n: u32| AgentCardRecord {
            agent_id: format!("a1-{n}"),
            profile: "explore".into(),
            description: format!("task {n}"),
            model: "p · m".into(),
            background: true,
        };
        let record = RolloutRecord::ToolCall {
            tool: "AgentSwarm".into(),
            summary: "subagent batch (2 items)".into(),
            arguments: "{}".into(),
            output: "receipt".into(),
            is_error: false,
            edit: None,
            agent_card: None,
            agent_cards: vec![card(1), card(2)],
        };
        let line = serde_json::to_string(&record).expect("serialize");
        let back: RolloutRecord = serde_json::from_str(&line).expect("read back");
        let RolloutRecord::ToolCall { agent_cards, .. } = back else {
            panic!("should be a ToolCall record");
        };
        assert_eq!(agent_cards.len(), 2);
        assert_eq!(agent_cards[0].agent_id, "a1-1");
        assert!(
            agent_cards[1].background,
            "background flag survives the roundtrip"
        );
    }
}
