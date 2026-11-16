//! JSONL session persistence (codex-rollout style): `{data_dir}/sessions/{session_id}.jsonl`
//! with a first meta line, then one line per durable unit; live deltas are not persisted.
//! The session index and panel current state (todos/file changes/raw snapshots) live in
//! store.sqlite (see store.rs). Image bytes referenced by messages live in the adjacent
//! `{session_id}.media/` directory (ImageRef references them by path).
//! `search_sessions_by_content` is the read-only content search behind the UI quick
//! switcher's in-content session group.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use pig_protocol::{CoreError, SessionMeta};
use serde::{Deserialize, Serialize};

use pig_provider::{ChatMsg, ToolCallWire};

mod conversation;
pub(crate) mod jsonl;
mod media;
mod rebuild;

pub(crate) use conversation::ConversationStore;

/// Prefix of the marked user-role message that hands a compaction summary to
/// the next model request. Replay uses it to distinguish a compaction handoff
/// from a real user message; legacy system-role notes have no marker.
pub(crate) const COMPACTION_HANDOFF_MARKER: &str =
    "[compaction handoff | synthetic user message, not a new request]";

/// A compaction note installed in the model context: new notes are user-role
/// with an explicit marker, legacy notes (no marker) stay system-role so old
/// rollouts replay exactly as they ran.
pub(crate) fn compaction_note_message(note: String) -> ChatMsg {
    if is_compaction_handoff_note(&note) {
        ChatMsg::user(note)
    } else {
        ChatMsg::system(note)
    }
}

/// Whether a note text was produced by the current compaction code.
pub(crate) fn is_compaction_handoff_note(note: &str) -> bool {
    note.starts_with(COMPACTION_HANDOFF_MARKER)
}

/// Whether a message is a compaction summary handoff (never a real request).
pub(crate) fn is_compaction_handoff(message: &ChatMsg) -> bool {
    message.role == "user"
        && message
            .content
            .as_deref()
            .is_some_and(is_compaction_handoff_note)
}

/// Whether a message is either a new user-role handoff or a legacy system
/// note produced by an earlier compaction.
pub(crate) fn is_compaction_note(message: &ChatMsg) -> bool {
    is_compaction_handoff(message)
        || (message.role == "system"
            && message
                .content
                .as_deref()
                .is_some_and(|text| text.starts_with("[Earlier context compacted")))
}

/// Number of leading non-conversation messages: the frozen system prefix and
/// any compaction handoff immediately after it. These are reserved by the
/// budget and never count as compactable user work.
pub(crate) fn fixed_prefix_len(history: &[ChatMsg]) -> usize {
    let mut index = 0;
    while history
        .get(index)
        .is_some_and(|message| message.role == "system" || is_compaction_handoff(message))
    {
        index += 1;
    }
    index
}

/// Whether a user message starts its own run. A compaction handoff before it
/// is a synthetic boundary, not part of the user's run.
pub(crate) fn is_user_run_start(history: &[ChatMsg], index: usize) -> bool {
    history.get(index).is_some_and(|message| {
        message.role == "user"
            && !is_compaction_handoff(message)
            && (index == 0
                || history[index - 1].role != "user"
                || is_compaction_handoff(&history[index - 1]))
    })
}

/// Whether `cut` starts a user run whose first message is the preserved
/// checkpoint installed directly after a compaction note. Such a run may be
/// split at a later message: the checkpoint is summarized with the old note
/// while the remaining requests/instructions stay verbatim.
pub(crate) fn is_compaction_checkpoint_run(history: &[ChatMsg], index: usize) -> bool {
    if !history
        .get(index)
        .is_some_and(|message| message.role == "user")
    {
        return false;
    }
    let mut run_start = index;
    while run_start > 0
        && history[run_start - 1].role == "user"
        && !is_compaction_handoff(&history[run_start - 1])
    {
        run_start -= 1;
    }
    run_start < index
        && run_start == fixed_prefix_len(history)
        && run_start > 0
        && is_compaction_note(&history[run_start - 1])
}

pub(crate) use media::document_token_estimate;
pub use media::{
    DocumentRef, ImageRef, image_nums, next_media_index, persist_chat_documents,
    persist_chat_images, rehydrate_documents, rehydrate_image,
};
#[cfg(test)]
pub(crate) use media::{media_reads, reset_media_reads};
pub use rebuild::{
    notification_delivery_receipts, rebuild_history, rebuild_history_with_media_root,
};

fn default_true() -> bool {
    true
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn deserialize_permission_reminder<'de, D>(
    deserializer: D,
) -> Result<Option<(pig_protocol::PermissionSettings, bool, String)>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let values = Option::<Vec<serde_json::Value>>::deserialize(deserializer)?;
    let Some(values) = values else {
        return Ok(None);
    };
    if values.len() != 2 && values.len() != 3 {
        return Err(serde::de::Error::custom(
            "permission reminder must have two or three entries",
        ));
    }
    let permissions =
        serde_json::from_value(values[0].clone()).map_err(serde::de::Error::custom)?;
    let plan = serde_json::from_value(values[1].clone()).map_err(serde::de::Error::custom)?;
    let access = if values.len() == 3 {
        serde_json::from_value(values[2].clone()).map_err(serde::de::Error::custom)?
    } else {
        String::new()
    };
    Ok(Some((permissions, plan, access)))
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PromptReminderState {
    pub date: String,
    pub agents: String,
    pub memory: String,
    #[serde(default, deserialize_with = "deserialize_permission_reminder")]
    pub permissions: Option<(pig_protocol::PermissionSettings, bool, String)>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PromptSnapshot {
    pub text: String,
    pub git_snapshot: Option<String>,
    pub agents_prompt: String,
    pub skills_prompt: String,
    #[serde(default)]
    pub skills_state: String,
    pub memory_prompt: String,
    #[serde(default)]
    pub memory_enabled: bool,
    pub profiles_summary: String,
    pub date_frozen: String,
    pub reminders: PromptReminderState,
}

// The persistence format favors readability/evolvability: the ToolCall variant (with the
// batch agent-card list) being several hundred bytes larger than the other variants is
// expected; records are transient serialization units, no Box split just to save memory
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationKind {
    #[default]
    Root,
    Subagent,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildContextMode {
    #[default]
    Fresh,
    Fork,
}

#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RolloutRecord {
    Meta {
        id: String,
        cwd: PathBuf,
        title: String,
        created_at: u64,
        #[serde(default)]
        kind: ConversationKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_id: Option<String>,
        /// Durable lower bound below the root; None on a root session.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        delegation_depth: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_mode: Option<ChildContextMode>,
        /// Number of inherited message records before the fork notice and new
        /// task. The copied records themselves remain the replay authority.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        inherited_record_count: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    /// The system/developer instructions for a subagent conversation. Root
    /// sessions keep their prompt in `PromptSnapshot`; both rebuild paths use
    /// the latest one.
    System {
        text: String,
    },
    PromptSnapshot {
        snapshot: PromptSnapshot,
    },
    Assistant {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replay: Option<pig_provider::AssistantReplay>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCallWire>,
        #[serde(default)]
        ts_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_ms: Option<u64>,
    },
    User {
        text: String,
        files: Vec<String>,
        /// Clean attachment references used by the UI and legacy replay.
        images: Vec<ImageRef>,
        /// When the message was sent (unix ms; absent in records written
        /// before this field existed → 0). The user bubble's action row prints
        /// it after a reload.
        #[serde(default)]
        sent_at_ms: u64,
        /// Exact model-facing text after reminders, pointers and image projection.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sent_text: Option<String>,
        /// Images actually present in the model-facing message; empty for non-vision projection.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        sent_images: Vec<ImageRef>,
        /// Model-facing text before the input_image capability projection.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sent_input_image: Option<bool>,
    },
    TaskNotificationBatch {
        notifications: Vec<pig_protocol::TaskNotification>,
        model_text: String,
        sent_at_ms: u64,
    },
    Reasoning {
        text: String,
        /// Ordered native items for provider continuation; legacy records only have text.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replay: Option<pig_provider::AssistantReplay>,
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
        #[serde(default, skip_serializing_if = "is_false")]
        stopped: bool,
        /// Diff of this edit (restores the tool card's inline diff view on replay)
        edit: Option<pig_protocol::EditDiff>,
        /// Agent-card metadata for the Agent tool card (replay rebuilds the agent card)
        agent_card: Option<AgentCardRecord>,
        /// Agent-card metadata for the AgentSwarm tool card (one per subagent; replay
        /// rebuilds all)
        agent_cards: Vec<AgentCardRecord>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImageRef>,
        /// PDF references from ReadMediaFile (bytes live in the session media
        /// directory; legacy records read as empty)
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        documents: Vec<DocumentRef>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        delivered_notification_ids: Vec<String>,
    },
    /// A turn's file changes (replay restores the per-turn changes panel in the message stream)
    TurnChanges {
        files: Vec<pig_protocol::EditDiff>,
    },
    /// Context injected by a Stop-hook continuation (the hook asked the agent
    /// to keep going with extra guidance): rebuilds as a user message so the
    /// conversation continues coherently on resume; replay emits no UI event
    /// for it (it is not a user-visible message)
    HookContext {
        text: String,
    },
    /// A turn's token usage and timing (replay restores footer stats and session totals; the
    /// context usage watermark is restored by StepUsage). duration_ms is the turn's
    /// wall-clock time; api_ms is model time summed over the turn's steps (each step from its
    /// start to the stream's end); ttft_ms is the summed time-to-first-token within it,
    /// `ttft_steps` the steps that recorded one (average first token = ttft_ms / ttft_steps;
    /// decode speed excluding the first token uses api_ms - ttft_ms) and api_steps the
    /// request count
    TurnStats {
        input: u64,
        cache_read: u64,
        output: u64,
        duration_ms: u64,
        /// Wall-clock end of the turn (unix ms; absent in records written
        /// before this field existed → 0). The UI prints it as the turn's end
        /// time; SQLite keeps the same moment in turn_usage.ended_at_ms.
        #[serde(default)]
        ended_at_ms: u64,
        api_ms: u64,
        ttft_ms: u64,
        /// Steps that recorded a first token (the average TTFT divisor)
        #[serde(default)]
        ttft_steps: u64,
        api_steps: u64,
        /// Summed per-call tool spans (gate + approval + execution)
        #[serde(default)]
        tool_ms: u64,
        #[serde(default = "default_true")]
        has_usage: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        finish_reason: Option<pig_protocol::FinishReason>,
        /// Durable per-turn identity shared with this turn's StepUsage records:
        /// SQLite usage insertion is unique on (session, turn_id), so a crash
        /// between the SQLite insert and this record (or the reverse) cannot
        /// double count.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
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
        /// The turn's durable identity (see TurnStats.turn_id).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
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
        /// Pre-compaction context usage watermark (None on old records / when no Usage
        /// sample existed yet); the UI divider shows it as "before → after tokens"
        used_before: Option<u64>,
        /// Bare model summary (None = truncation fallback / old records); the note embeds
        /// it in model-facing guidance — the UI's summary panel shows the bare text
        summary: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tail_len: Option<usize>,
        /// Index of the active real user message in the pre-compaction rebuilt
        /// history when the cut passed it (an assistant-starting tail or a
        /// later hook-only user segment). Replay extracts that already-rebuilt
        /// User (media and capability projection included) instead of storing
        /// a second copy; legacy records and tails that include the user omit
        /// it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preserved_user: Option<usize>,
        /// "watermark" or "overflow" for subagent compaction diagnostics.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trigger: Option<String>,
    },
    /// Recall marker (tombstone, append-only): excludes the last User record and
    /// everything after it — the whole turn that user message triggered — from the
    /// rebuilt context and the replayed transcript. The recalled content records stay
    /// in the file and the turn's TurnStats/StepUsage keep counting toward session
    /// statistics; multiple Recall records stack, each recalling one more preceding
    /// user message
    Recall,
}

impl RolloutRecord {
    pub fn root_meta(meta: &SessionMeta) -> Self {
        Self::Meta {
            id: meta.id.clone(),
            cwd: meta.cwd.clone(),
            title: meta.title.clone(),
            created_at: meta.created_at,
            kind: ConversationKind::Root,
            parent_id: None,
            delegation_depth: None,
            context_mode: None,
            inherited_record_count: None,
            profile: None,
            description: None,
            provider: None,
            model: None,
        }
    }

    pub fn subagent_meta(
        id: String,
        parent_id: String,
        cwd: PathBuf,
        description: String,
        profile: String,
        provider: String,
        model: String,
        delegation_depth: u32,
        created_at: u64,
    ) -> Self {
        Self::subagent_meta_with_context(
            id,
            parent_id,
            cwd,
            description,
            profile,
            provider,
            model,
            delegation_depth,
            ChildContextMode::Fresh,
            None,
            created_at,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn subagent_meta_with_context(
        id: String,
        parent_id: String,
        cwd: PathBuf,
        description: String,
        profile: String,
        provider: String,
        model: String,
        delegation_depth: u32,
        context_mode: ChildContextMode,
        inherited_record_count: Option<usize>,
        created_at: u64,
    ) -> Self {
        Self::Meta {
            id,
            cwd,
            title: description.clone(),
            created_at,
            kind: ConversationKind::Subagent,
            parent_id: Some(parent_id),
            delegation_depth: Some(delegation_depth),
            context_mode: Some(context_mode),
            inherited_record_count,
            profile: Some(profile),
            description: Some(description),
            provider: Some(provider),
            model: Some(model),
        }
    }
}

/// Agent-card metadata for the Agent tool card (persisted with RolloutRecord::ToolCall, for replay rebuild)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentCardRecord {
    pub agent_id: String,
    pub run_id: String,
    pub profile: String,
    pub description: String,
    /// "{provider_name} · {model}" (may carry a reasoning-level suffix)
    pub model: String,
    /// This run is background: the background card's running state is driven by the subagent
    /// lifecycle; after replay, core re-emits ConversationStatus to settle the final
    /// state (tasks do not outlive the process)
    pub background: bool,
}

/// A rollout append/sync failure. The caller-visible form is a structured
/// [`CoreError`]; the kind exists so recovery logic and tests can distinguish a
/// failed write (the rollout refuses further appends) from a serialization
/// error (nothing was written) and a sync failure (the line is structurally
/// present but not yet durable).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RolloutFailureKind {
    /// The record could not be serialized; no bytes were written.
    Serialize,
    /// The line was partially written. The rollout is poisoned and refuses
    /// later appends until it is reopened through [`Rollout::load`], which can
    /// recover the last valid prefix.
    Write,
    /// A previous failed append left the file in a state where appending would
    /// corrupt the final line.
    Poisoned,
    /// The file could not be flushed to stable storage. The bytes are present
    /// in the file but a crash may still lose them.
    Sync,
}

impl RolloutFailureKind {
    fn label(self) -> &'static str {
        match self {
            Self::Serialize => "serialize",
            Self::Write => "write",
            Self::Poisoned => "poisoned",
            Self::Sync => "sync",
        }
    }
}

/// One persisted conversation record stream: `{session_id}.jsonl`.
///
/// Appends are fallible by design. A failed append is reported to the caller
/// and the file is poisoned when a partial line may have landed, so no later
/// record can be concatenated onto a broken line. The next `load` recovers a
/// torn final line (backup + warning) while middle corruption stays a hard
/// error. Durability is requested explicitly with [`Rollout::sync`] at turn
/// boundaries — `File::flush` is not an fsync and is not used as one.
pub struct Rollout {
    path: PathBuf,
    file: std::fs::File,
    /// Set after a partial write that could not be rolled back. Every later
    /// append fails immediately instead of corrupting the file further.
    poisoned: Option<String>,
}

/// Summary of a recovered rollout load: the torn final line that was dropped
/// and the backup holding the original bytes.
#[derive(Clone, Debug)]
pub struct RolloutRecovery {
    pub discarded_line: usize,
    pub backup: PathBuf,
}

impl Rollout {
    pub fn create(dir: &Path, meta: &SessionMeta) -> Result<Self, CoreError> {
        std::fs::create_dir_all(dir).map_err(|e| CoreError::SessionsDirCreate {
            detail: e.to_string(),
        })?;
        let path = dir.join(format!("{}.jsonl", meta.id));
        let file = std::fs::File::create(&path).map_err(|e| CoreError::RolloutCreate {
            path: path.display().to_string(),
            detail: e.to_string(),
        })?;
        let record = RolloutRecord::root_meta(meta);
        let mut rollout = Self {
            path,
            file,
            poisoned: None,
        };
        rollout
            .append(&record)
            .map_err(|error| error.into_core(&rollout.path))?;
        // The meta line identifies the file; make it durable before the first
        // turn can append after it.
        rollout
            .sync()
            .map_err(|error| error.into_core(&rollout.path))?;
        Ok(rollout)
    }

    /// resume continuation: opens the existing rollout in append mode (does not overwrite meta or history lines)
    pub fn open_append(dir: &Path, id: &str) -> Result<Self, CoreError> {
        let path = dir.join(format!("{id}.jsonl"));
        // Appending after a torn final line would glue the new record onto the
        // broken bytes. Session::load always recovers first; this guard keeps a
        // direct caller from corrupting the file.
        if let Ok(metadata) = std::fs::metadata(&path)
            && metadata.len() > 0
        {
            use std::io::Seek as _;
            let mut last = [0u8; 1];
            if let Ok(mut file) = std::fs::File::open(&path)
                && file.seek(std::io::SeekFrom::End(-1)).is_ok()
                && file.read_exact(&mut last).is_ok()
                && last[0] != b'\n'
            {
                return Err(CoreError::RolloutOpen {
                    path: path.display().to_string(),
                    detail: "the file ends with an incomplete line; load it first so the torn tail is backed up and recovered".to_string(),
                });
            }
        }
        let file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .map_err(|e| CoreError::RolloutOpen {
                path: path.display().to_string(),
                detail: e.to_string(),
            })?;
        Ok(Self {
            path,
            file,
            poisoned: None,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one durable record. On error the caller must surface it; a
    /// `Write` failure that could not be rolled back also disables every later
    /// append until the session is reopened through [`Rollout::load`].
    pub fn append(&mut self, record: &RolloutRecord) -> Result<(), RolloutError> {
        if let Some(detail) = &self.poisoned {
            return Err(RolloutError {
                kind: RolloutFailureKind::Poisoned,
                detail: detail.clone(),
            });
        }
        let mut line = serde_json::to_string(record).map_err(|error| RolloutError {
            kind: RolloutFailureKind::Serialize,
            detail: error.to_string(),
        })?;
        line.push('\n');
        let start = self.file.metadata().map(|meta| meta.len()).ok();
        if let Err(error) = self.file.write_all(line.as_bytes()) {
            // Try to remove a partial line. If that fails, the file may hold a
            // torn record and must not receive another append.
            let rolled_back = start.is_some_and(|start| self.file.set_len(start).is_ok());
            if !rolled_back {
                let detail = format!("failed to append a rollout record: {error}");
                self.poisoned = Some(detail.clone());
                return Err(RolloutError {
                    kind: RolloutFailureKind::Write,
                    detail,
                });
            }
            tracing::warn!(
                path = %self.path.display(),
                "partially written rollout line rolled back; continuing",
            );
            return Err(RolloutError {
                kind: RolloutFailureKind::Write,
                detail: error.to_string(),
            });
        }
        Ok(())
    }

    /// Real durability at a turn boundary (`sync_data`, not `flush`).
    pub fn sync(&mut self) -> Result<(), RolloutError> {
        self.file.sync_data().map_err(|error| RolloutError {
            kind: RolloutFailureKind::Sync,
            detail: error.to_string(),
        })
    }

    /// Load every record. A torn **final** line is recovered from the last
    /// valid prefix: the original file is copied to a timestamped backup and
    /// atomically replaced by the valid prefix. Corruption anywhere else is a
    /// hard error (never silently dropped).
    pub fn load(path: &Path) -> Result<Vec<RolloutRecord>, CoreError> {
        Self::load_with_recovery(path).map(|(records, _)| records)
    }

    pub fn load_with_recovery(
        path: &Path,
    ) -> Result<(Vec<RolloutRecord>, Option<RolloutRecovery>), CoreError> {
        let bytes = std::fs::read(path).map_err(|e| CoreError::RolloutRead {
            path: path.display().to_string(),
            detail: e.to_string(),
        })?;
        let parsed = parse_rollout_bytes(&bytes)?;
        if let Some(line_no) = parsed.torn_line {
            if parsed.records.is_empty() {
                return Err(CoreError::RolloutLineParse {
                    detail: format!("line {line_no}: the rollout has no readable prefix"),
                });
            }
            return Self::recover_torn_tail(
                path,
                &bytes,
                parsed.valid_end,
                line_no,
                parsed.records,
            );
        }
        // A syntactically complete final line without its newline is repaired
        // here: appending after it would otherwise concatenate two records.
        if !bytes.is_empty()
            && !jsonl::inspect_tail(&bytes).terminated
            && !parsed.records.is_empty()
        {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(path)
                .map_err(|e| CoreError::RolloutRead {
                    path: path.display().to_string(),
                    detail: e.to_string(),
                })?;
            file.write_all(b"\n").map_err(|e| CoreError::RolloutRead {
                path: path.display().to_string(),
                detail: e.to_string(),
            })?;
            let _ = file.sync_data();
        }
        Ok((parsed.records, None))
    }

    /// Read-only load for snapshots that may be written concurrently (forking
    /// a running session): an in-flight final line is ignored, never repaired
    /// and never allowed to touch the source file. Middle corruption is still
    /// a hard error.
    pub fn load_snapshot(path: &Path) -> Result<Vec<RolloutRecord>, CoreError> {
        let bytes = std::fs::read(path).map_err(|e| CoreError::RolloutRead {
            path: path.display().to_string(),
            detail: e.to_string(),
        })?;
        Ok(parse_rollout_bytes(&bytes)?.records)
    }

    fn recover_torn_tail(
        path: &Path,
        original: &[u8],
        valid_end: usize,
        discarded_line: usize,
        records: Vec<RolloutRecord>,
    ) -> Result<(Vec<RolloutRecord>, Option<RolloutRecovery>), CoreError> {
        let backup = backup_path(path);
        // Never rewrite before the original bytes are safely copied: a failed
        // backup leaves the (recoverable) file untouched.
        std::fs::write(&backup, original).map_err(|error| CoreError::RolloutRead {
            path: path.display().to_string(),
            detail: format!(
                "a torn final line was found but the backup {} could not be written ({error}); the file was left unchanged",
                backup.display()
            ),
        })?;
        let temp = path.with_extension(format!("jsonl.recover-{}.tmp", std::process::id()));
        let result = (|| -> std::io::Result<()> {
            let mut file = std::fs::File::create(&temp)?;
            file.write_all(&original[..valid_end])?;
            file.sync_all()?;
            std::fs::rename(&temp, path)
        })();
        if let Err(error) = result {
            let _ = std::fs::remove_file(&temp);
            return Err(CoreError::RolloutCreate {
                path: path.display().to_string(),
                detail: format!("failed to atomically restore the valid rollout prefix: {error}"),
            });
        }
        tracing::warn!(
            path = %path.display(),
            backup = %backup.display(),
            line = discarded_line,
            "recovered a torn final rollout line from the last valid prefix",
        );
        Ok((
            records,
            Some(RolloutRecovery {
                discarded_line,
                backup,
            }),
        ))
    }
}

fn parse_line(body: &[u8], line_no: usize) -> Result<RolloutRecord, CoreError> {
    let text = std::str::from_utf8(body).map_err(|error| CoreError::RolloutLineParse {
        detail: format!("line {line_no}: {error}"),
    })?;
    serde_json::from_str(text).map_err(|error| CoreError::RolloutLineParse {
        detail: format!("line {line_no}: {error}"),
    })
}

struct ParsedRollout {
    records: Vec<RolloutRecord>,
    /// Byte offset just past the last valid line.
    valid_end: usize,
    /// Line number of a malformed **final** line, when everything before it
    /// parsed cleanly.
    torn_line: Option<usize>,
}

fn parse_rollout_bytes(bytes: &[u8]) -> Result<ParsedRollout, CoreError> {
    let mut records = Vec::new();
    let mut valid_end = 0usize;
    for line in jsonl::lines(bytes) {
        if line.body.iter().all(u8::is_ascii_whitespace) {
            valid_end = line.start + line.chunk.len();
            continue;
        }
        match parse_line(line.body, line.number) {
            Ok(record) => {
                records.push(record);
                valid_end = line.start + line.chunk.len();
            }
            Err(error) => {
                let is_final = !bytes[line.start + line.chunk.len()..]
                    .iter()
                    .any(|byte| !byte.is_ascii_whitespace());
                if !is_final || records.is_empty() {
                    return Err(error);
                }
                return Ok(ParsedRollout {
                    records,
                    valid_end,
                    torn_line: Some(line.number),
                });
            }
        }
    }
    Ok(ParsedRollout {
        records,
        valid_end,
        torn_line: None,
    })
}

fn backup_path(path: &Path) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "rollout.jsonl".to_string());
    let mut backup = path.with_file_name(format!("{file_name}.corrupt-{stamp}"));
    let mut suffix = 1;
    while backup.exists() {
        backup = path.with_file_name(format!("{file_name}.corrupt-{stamp}-{suffix}"));
        suffix += 1;
    }
    backup
}

/// The public append/sync failure type: structured kind plus the detail.
#[derive(Clone, Debug)]
pub struct RolloutError {
    pub kind: RolloutFailureKind,
    pub detail: String,
}

impl std::fmt::Display for RolloutError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "rollout {} failed: {}",
            self.kind.label(),
            self.detail
        )
    }
}

impl std::error::Error for RolloutError {}

impl RolloutError {
    /// Convert to the UI-facing structured error.
    pub fn into_core(self, path: &Path) -> CoreError {
        CoreError::RolloutWrite {
            path: path.display().to_string(),
            detail: format!("{}: {}", self.kind.label(), self.detail),
        }
    }
}

/// Conversation record line prefixes (`#[serde(tag = "type")]` serializes the
/// tag first) scanned by [`search_sessions_by_content`]: user messages,
/// assistant text, reasoning, and stop-hook context. Tool calls, usage/compact
/// records and the meta line are not conversation.
const CONVERSATION_LINE_PREFIXES: [&str; 5] = [
    "{\"type\":\"user\"",
    "{\"type\":\"assistant\"",
    "{\"type\":\"text\"",
    "{\"type\":\"reasoning\"",
    "{\"type\":\"hook_context\"",
];

/// One content-search hit: the session id plus the match context (original
/// casing) pre-split for highlighted rendering — [before] hit [after]
pub struct ContentHit {
    pub session_id: String,
    /// Snippet before the match ("…"-prefixed when truncated)
    pub before: String,
    /// The matched text itself
    pub hit: String,
    /// Snippet after the match ("…"-suffixed when truncated)
    pub after: String,
}

/// Snippet window sizes in chars (CJK counts one per char, like the display)
const SNIPPET_BEFORE_CHARS: usize = 40;
const SNIPPET_AFTER_CHARS: usize = 60;

/// Content search for the UI quick switcher: a session matches when any
/// conversation record's text contains the query, case-insensitively
/// (serde_json writes non-ASCII raw, so CJK queries match UTF-8 text
/// directly). The `*.model-io.jsonl` debug traces are skipped; subagent
/// rollouts (`{id}.agents/`) live in subdirectories and are not scanned.
/// Read-only and uncached — meant to be called off the UI thread
/// (debounced). Returns the hits in directory read order (callers re-sort by
/// recency).
pub fn search_sessions_by_content(sessions_dir: &Path, query: &str) -> Vec<ContentHit> {
    let needle = query.to_lowercase();
    let Ok(entries) = std::fs::read_dir(sessions_dir) else {
        return Vec::new();
    };
    let mut hits = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.ends_with(".jsonl") || name.ends_with(".model-io.jsonl") {
            continue;
        }
        if let Some((before, hit, after)) = rollout_file_snippet(&entry.path(), &needle)
            && let Some(id) = name.strip_suffix(".jsonl")
        {
            hits.push(ContentHit {
                session_id: id.to_string(),
                before,
                hit,
                after,
            });
        }
    }
    hits
}

/// First-match-wins scan of one rollout file (conversation lines only);
/// the line buffer is reused so a large rollout does not allocate per line.
/// Returns the match context as (before, hit, after)
fn rollout_file_snippet(path: &Path, needle_lower: &str) -> Option<(String, String, String)> {
    use std::io::BufRead as _;
    let Ok(file) = std::fs::File::open(path) else {
        return None;
    };
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();
    loop {
        line.clear();
        let Ok(read) = reader.read_line(&mut line) else {
            return None;
        };
        if read == 0 {
            return None;
        }
        // Cheap raw-line gate first (the needle may have matched outside the
        // text — e.g. a path in a user record's "files" — so the text field
        // itself is verified below)
        if !CONVERSATION_LINE_PREFIXES
            .iter()
            .any(|prefix| line.starts_with(prefix))
            || !line.to_lowercase().contains(needle_lower)
        {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let combined;
        let text = if value["type"] == "assistant" {
            combined = ["reasoning", "text"]
                .into_iter()
                .filter_map(|field| value[field].as_str())
                .collect::<Vec<_>>()
                .join("\n");
            combined.as_str()
        } else if let Some(text) = value.get("text").and_then(|text| text.as_str()) {
            text
        } else {
            continue;
        };
        // Single-line display: whitespace runs (incl. newlines) collapse to
        // one space before windowing
        let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let Some(start) = flat.to_lowercase().find(needle_lower) else {
            continue;
        };
        let end = start + needle_lower.len();
        // Byte offsets come from the lowercased copy; for exotic casing
        // (e.g. İ) they can fall off `flat`'s char boundaries — skip rather
        // than slice wrong (CJK/latin queries are unaffected)
        let Some(hit) = flat.get(start..end) else {
            continue;
        };
        let before_full = &flat[..start];
        let before_chars = before_full.chars().count();
        let before = if before_chars > SNIPPET_BEFORE_CHARS {
            let skip = before_chars - SNIPPET_BEFORE_CHARS;
            format!("…{}", before_full.chars().skip(skip).collect::<String>())
        } else {
            before_full.to_string()
        };
        let after_full = &flat[end..];
        let after = if after_full.chars().count() > SNIPPET_AFTER_CHARS {
            format!(
                "{}…",
                after_full
                    .chars()
                    .take(SNIPPET_AFTER_CHARS)
                    .collect::<String>()
            )
        } else {
            after_full.to_string()
        };
        return Some((before, hit.to_string(), after));
    }
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
            run_id: format!("run-{n}"),
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
            stopped: false,
            edit: None,
            agent_card: None,
            agent_cards: vec![card(1), card(2)],
            images: vec![],
            documents: vec![],
            delivered_notification_ids: vec![],
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

    fn native_replay() -> pig_provider::AssistantReplay {
        pig_provider::AssistantReplay {
            origin: pig_provider::ReplayOrigin {
                api_format: pig_protocol::ApiFormat::OpenAiResponses,
                base_url: "http://localhost/v1".into(),
                model: None,
                credential: None,
            },
            items: vec![
                pig_provider::AssistantReplayItem::ResponsesReasoning {
                    id: Some("rs_1".into()),
                    summary: vec!["reasoning".into()],
                    content: vec![],
                    encrypted_content: Some("encrypted".into()),
                },
                pig_provider::AssistantReplayItem::Text {
                    text: "answer".into(),
                    citations: vec![],
                },
                pig_provider::AssistantReplayItem::ToolCall {
                    call: pig_provider::ToolCall {
                        id: "call_1".into(),
                        name: "Read".into(),
                        arguments: "{\"path\":\"a\"}".into(),
                    }
                    .to_wire(),
                },
                pig_provider::AssistantReplayItem::ToolCall {
                    call: pig_provider::ToolCall {
                        id: "call_2".into(),
                        name: "Read".into(),
                        arguments: "{\"path\":\"b\"}".into(),
                    }
                    .to_wire(),
                },
            ],
        }
    }

    fn tool_record(output: &str) -> RolloutRecord {
        RolloutRecord::ToolCall {
            tool: "Read".into(),
            summary: "Read".into(),
            arguments: "{}".into(),
            output: output.into(),
            is_error: false,
            stopped: false,
            edit: None,
            agent_card: None,
            agent_cards: vec![],
            images: vec![],
            documents: vec![],
            delivered_notification_ids: vec![],
        }
    }

    #[test]
    fn legacy_records_without_new_fields_stay_readable() {
        let reasoning: RolloutRecord =
            serde_json::from_str(r#"{"type":"reasoning","text":"legacy"}"#)
                .expect("legacy reasoning");
        assert!(matches!(
            reasoning,
            RolloutRecord::Reasoning { replay: None, .. }
        ));

        let stats: RolloutRecord = serde_json::from_str(
            r#"{"type":"turn_stats","input":1,"cache_read":2,"output":3,"duration_ms":4,"api_ms":5,"ttft_ms":6,"api_steps":1,"tool_ms":0}"#,
        )
        .expect("legacy turn stats");
        assert!(matches!(
            stats,
            RolloutRecord::TurnStats {
                has_usage: true,
                finish_reason: None,
                ended_at_ms: 0,
                ..
            }
        ));
    }

    #[test]
    fn structured_replay_restores_original_calls_without_duplicates() {
        let records = vec![
            RolloutRecord::Reasoning {
                text: "reasoning".into(),
                replay: Some(native_replay()),
            },
            RolloutRecord::Text {
                text: "answer".into(),
            },
            tool_record("first output"),
            tool_record("second output"),
        ];
        let (history, _) = rebuild_history(&records, "system".into());
        assert_eq!(history.len(), 4);
        let assistant = &history[1];
        let calls = assistant.tool_calls.as_ref().expect("two original calls");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[1].id, "call_2");
        assert_eq!(history[2].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(history[3].tool_call_id.as_deref(), Some("call_2"));
        assert_eq!(history[2].content.as_deref(), Some("first output"));
        assert_eq!(history[3].content.as_deref(), Some("second output"));
        let replay = assistant.replay.as_ref().expect("native replay");
        assert_eq!(replay.items.len(), 4);
    }

    #[test]
    fn crashed_replay_drops_unanswered_tool_calls() {
        let records = vec![
            RolloutRecord::Reasoning {
                text: "reasoning".into(),
                replay: Some(native_replay()),
            },
            RolloutRecord::Text {
                text: "answer".into(),
            },
            tool_record("only completed output"),
        ];
        let (history, _) = rebuild_history(&records, "system".into());
        let assistant = &history[1];
        let calls = assistant.tool_calls.as_ref().expect("answered call");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        let replay = assistant.replay.as_ref().expect("native replay");
        assert_eq!(
            replay
                .items
                .iter()
                .filter(|item| matches!(item, pig_provider::AssistantReplayItem::ToolCall { .. }))
                .count(),
            1
        );
        assert_eq!(history.len(), 3);
        assert_eq!(history[2].tool_call_id.as_deref(), Some("call_1"));
    }

    #[test]
    fn reasoning_only_native_replay_is_not_lost() {
        let mut replay = native_replay();
        replay.items.retain(|item| {
            matches!(
                item,
                pig_provider::AssistantReplayItem::ResponsesReasoning { .. }
            )
        });
        let records = vec![RolloutRecord::Reasoning {
            text: "reasoning only".into(),
            replay: Some(replay),
        }];
        let (history, _) = rebuild_history(&records, "system".into());
        assert_eq!(history.len(), 2);
        assert_eq!(history[1].reasoning.as_deref(), Some("reasoning only"));
        assert!(history[1].replay.is_some());
        assert!(history[1].tool_calls.is_none());
    }

    /// Content search hits only conversation lines (user/text/reasoning),
    /// never tool calls or model-io traces, and is case-insensitive
    #[test]
    fn search_sessions_by_content_conversation_only() {
        let dir =
            std::env::temp_dir().join(format!("pig-core-rollout-search-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create sessions dir");
        std::fs::write(
            dir.join("a.jsonl"),
            "{\"type\":\"meta\",\"id\":\"a\",\"cwd\":\"/tmp\",\"title\":\"t\",\"created_at\":1}\n\
             {\"type\":\"user\",\"text\":\"deploy the Kiwi-Fruit service\",\"files\":[],\"images\":[]}\n",
        )
        .expect("write a");
        // Tool-call output mentions the needle but is not conversation
        std::fs::write(
            dir.join("b.jsonl"),
            "{\"type\":\"tool_call\",\"tool\":\"ExecCommand\",\"summary\":\"s\",\"arguments\":\"{}\",\"output\":\"Kiwi-Fruit here\",\"is_error\":false,\"edit\":null,\"agent_card\":null,\"agent_cards\":[]}\n",
        )
        .expect("write b");
        // The model-io debug trace is not a conversation either
        std::fs::write(dir.join("c.model-io.jsonl"), "Kiwi-Fruit\n").expect("write c");
        std::fs::write(
            dir.join("d.jsonl"),
            "{\"type\":\"text\",\"text\":\"nothing relevant\"}\n",
        )
        .expect("write d");

        let hits = search_sessions_by_content(&dir, "kiwi-fruit");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(hits.len(), 1, "only the conversation-line file may match");
        let hit = &hits[0];
        assert_eq!(hit.session_id, "a");
        // Snippet: original casing, full text kept when it fits the window
        assert_eq!(hit.before, "deploy the ");
        assert_eq!(hit.hit, "Kiwi-Fruit");
        assert_eq!(hit.after, " service");
    }

    /// Snippet windowing: long context is trimmed with … on the cut ends,
    /// whitespace (incl. newlines) collapses to single spaces
    #[test]
    fn search_sessions_by_content_snippet_window() {
        let dir =
            std::env::temp_dir().join(format!("pig-core-rollout-snippet-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create sessions dir");
        let text = format!("{}\n\nKiwi-Fruit\t{}", "x".repeat(80), "y".repeat(100));
        let line = serde_json::to_string(&RolloutRecord::Text { text }).expect("serialize");
        std::fs::write(dir.join("a.jsonl"), format!("{line}\n")).expect("write a");

        let hits = search_sessions_by_content(&dir, "kiwi-fruit");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert_eq!(hit.hit, "Kiwi-Fruit");
        // before_full = 80 x's + collapsed space (81 chars) → last 40 kept
        assert_eq!(hit.before, format!("…{} ", "x".repeat(39)));
        // after_full = collapsed space + 100 y's (101 chars) → first 60 kept
        assert_eq!(hit.after, format!(" {}…", "y".repeat(59)));
    }

    #[test]
    fn legacy_context_fields_use_compatibility_defaults() {
        let user: RolloutRecord =
            serde_json::from_str(r#"{"type":"user","text":"legacy","files":[],"images":[]}"#)
                .expect("legacy user");
        assert!(matches!(
            user,
            RolloutRecord::User {
                sent_at_ms: 0,
                sent_text: None,
                sent_images,
                ..
            } if sent_images.is_empty()
        ));
        let tool: RolloutRecord = serde_json::from_str(
            r#"{"type":"tool_call","tool":"Read","summary":"s","arguments":"{}","output":"ok","is_error":false,"edit":null,"agent_card":null,"agent_cards":[]}"#,
        )
        .expect("legacy tool");
        assert!(matches!(
            tool,
            RolloutRecord::ToolCall { images, .. } if images.is_empty()
        ));
        let compact: RolloutRecord = serde_json::from_str(
            r#"{"type":"compact","note":"n","omitted":2,"automatic":false,"used_after":null,"used_before":null,"summary":null}"#,
        )
        .expect("legacy compact");
        assert!(matches!(
            compact,
            RolloutRecord::Compact { tail_len: None, .. }
        ));
        // Canonical records written before the preserved-user field keep
        // deserializing (the split boundary is simply absent).
        let canonical: RolloutRecord = serde_json::from_str(
            r#"{"type":"compact","note":"n","omitted":2,"automatic":false,"used_after":null,"used_before":null,"summary":null,"tail_len":2}"#,
        )
        .expect("compact without a preserved user");
        assert!(matches!(
            canonical,
            RolloutRecord::Compact {
                tail_len: Some(2),
                preserved_user: None,
                ..
            }
        ));
    }

    #[test]
    fn legacy_two_part_permission_snapshot_is_readable() {
        let snapshot = PromptSnapshot {
            text: "system".into(),
            git_snapshot: None,
            agents_prompt: String::new(),
            skills_prompt: String::new(),
            skills_state: String::new(),
            memory_prompt: String::new(),
            memory_enabled: false,
            profiles_summary: String::new(),
            date_frozen: "2026-01-01".into(),
            reminders: PromptReminderState {
                date: "2026-01-01".into(),
                agents: String::new(),
                memory: String::new(),
                permissions: Some((
                    pig_protocol::PermissionSettings::default(),
                    false,
                    String::new(),
                )),
            },
        };
        let mut value = serde_json::to_value(RolloutRecord::PromptSnapshot { snapshot })
            .expect("serialize snapshot");
        value["snapshot"]["reminders"]["permissions"] =
            serde_json::json!([pig_protocol::PermissionSettings::default(), false]);
        value["snapshot"]["web_search"] = serde_json::Value::Bool(false);
        let record: RolloutRecord = serde_json::from_value(value).expect("legacy snapshot");
        let RolloutRecord::PromptSnapshot { snapshot } = record else {
            panic!("snapshot record");
        };
        let (_, plan, access) = snapshot.reminders.permissions.expect("permission state");
        assert!(!plan);
        assert!(access.is_empty());
    }

    #[test]
    fn canonical_assistant_is_searchable() {
        let dir = std::env::temp_dir().join(format!(
            "pig-core-rollout-assistant-search-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create sessions dir");
        let record = RolloutRecord::Assistant {
            text: "answer with Kiwi-Fruit".into(),
            reasoning: None,
            replay: None,
            tool_calls: vec![],
            ts_ms: 0,
            reasoning_ms: None,
        };
        let line = serde_json::to_string(&record).expect("serialize");
        std::fs::write(dir.join("a.jsonl"), format!("{line}\n")).expect("write record");
        let hits = search_sessions_by_content(&dir, "kiwi-fruit");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "a");
        assert_eq!(hits[0].hit, "Kiwi-Fruit");
    }

    #[test]
    fn canonical_reasoning_is_searchable() {
        let dir = std::env::temp_dir().join(format!(
            "pig-core-rollout-reasoning-search-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create sessions dir");
        let record = RolloutRecord::Assistant {
            text: "plain answer".into(),
            reasoning: Some("reasoning mentions Kiwi-Fruit".into()),
            replay: None,
            tool_calls: vec![],
            ts_ms: 0,
            reasoning_ms: None,
        };
        let line = serde_json::to_string(&record).expect("serialize");
        std::fs::write(dir.join("a.jsonl"), format!("{line}\n")).expect("write record");
        let hits = search_sessions_by_content(&dir, "kiwi-fruit");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].hit, "Kiwi-Fruit");
    }

    fn meta_line(id: &str) -> String {
        format!(
            "{{\"type\":\"meta\",\"id\":\"{id}\",\"cwd\":\"/tmp\",\"title\":\"t\",\"created_at\":1}}\n"
        )
    }

    fn user_line(text: &str) -> String {
        format!("{{\"type\":\"user\",\"text\":\"{text}\",\"files\":[],\"images\":[]}}\n")
    }

    fn temp_rollout(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("pig-core-rollout-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create rollout dir");
        (dir.clone(), dir.join("s1.jsonl"))
    }

    /// A torn final line is recovered from the last valid prefix: the original
    /// bytes are backed up, the file is atomically rewritten, and a following
    /// append succeeds (opening the raw torn file would be refused).
    #[test]
    fn torn_final_line_recovers_with_backup() {
        let (dir, path) = temp_rollout("torn");
        let original = format!(
            "{}{} {{\"type\":\"user\",\"text\":\"tor",
            meta_line("s1"),
            user_line("hi")
        );
        std::fs::write(&path, &original).expect("write torn rollout");
        assert!(
            Rollout::open_append(&dir, "s1").is_err(),
            "a torn tail must not be appended to directly"
        );
        let (records, recovery) = Rollout::load_with_recovery(&path).expect("torn tail recovers");
        assert_eq!(records.len(), 2);
        let recovery = recovery.expect("recovery summary");
        assert_eq!(recovery.discarded_line, 3);
        assert_eq!(
            std::fs::read(&recovery.backup).expect("backup readable"),
            original.as_bytes(),
            "the backup keeps the exact original bytes"
        );
        let recovered = std::fs::read_to_string(&path).expect("recovered rollout");
        assert!(recovered.ends_with('\n'), "{recovered:?}");
        assert!(!recovered.contains("tor"), "{recovered:?}");
        let mut rollout = Rollout::open_append(&dir, "s1").expect("open after recovery");
        rollout
            .append(&RolloutRecord::Text {
                text: "after".into(),
            })
            .expect("append after recovery");
        rollout.sync().expect("sync");
        let records = Rollout::load(&path).expect("reload");
        assert_eq!(records.len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Corruption in the middle is a hard error with its line number and
    /// leaves the file untouched; it is never silently skipped.
    #[test]
    fn middle_corruption_is_a_hard_error() {
        let (dir, path) = temp_rollout("middle");
        let original = format!(
            "{}{}not-json\n{}",
            meta_line("s1"),
            user_line("hi"),
            user_line("later")
        );
        std::fs::write(&path, &original).expect("write corrupt rollout");
        let error = Rollout::load_with_recovery(&path).expect_err("middle corruption");
        assert!(
            format!("{error:?}").contains("line 3"),
            "the error names the corrupt line: {error:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("file intact"),
            original
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A syntactically complete final line without its newline is repaired
    /// before append; no record is lost.
    #[test]
    fn unterminated_valid_final_line_is_repaired() {
        let (dir, path) = temp_rollout("unterminated");
        let line = serde_json::to_string(&RolloutRecord::Text {
            text: "tail".into(),
        })
        .expect("serialize");
        std::fs::write(
            &path,
            format!("{}{}{}", meta_line("s1"), user_line("hi"), line),
        )
        .expect("write");
        let records = Rollout::load(&path).expect("load");
        assert_eq!(records.len(), 3);
        let repaired = std::fs::read_to_string(&path).expect("read repaired");
        assert!(repaired.ends_with('\n'));
        Rollout::open_append(&dir, "s1").expect("append after repair");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A live source (fork) is read without repairing or truncating it: the
    /// in-flight final line is simply ignored.
    #[test]
    fn snapshot_load_never_repairs_the_source() {
        let (dir, path) = temp_rollout("snapshot");
        let original = format!("{}{} {{\"type\":\"user\"", meta_line("s1"), user_line("hi"));
        std::fs::write(&path, &original).expect("write in-flight rollout");
        let records = Rollout::load_snapshot(&path).expect("snapshot load");
        assert_eq!(records.len(), 2);
        assert_eq!(
            std::fs::read_to_string(&path).expect("source intact"),
            original,
            "load_snapshot must not rewrite the live source"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Once a partial write poisoned the handle, later appends fail instead of
    /// concatenating onto a broken line.
    #[test]
    fn poisoned_rollout_refuses_later_appends() {
        let (dir, path) = temp_rollout("poisoned");
        std::fs::write(&path, format!("{}{}", meta_line("s1"), user_line("hi"))).expect("write");
        let mut rollout = Rollout::open_append(&dir, "s1").expect("open");
        rollout.poisoned = Some("simulated partial write".into());
        let error = rollout
            .append(&RolloutRecord::Text {
                text: "lost".into(),
            })
            .expect_err("poisoned append");
        assert_eq!(error.kind, RolloutFailureKind::Poisoned);
        let records = Rollout::load(&path).expect("file still valid");
        assert_eq!(records.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
