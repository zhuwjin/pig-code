use super::estimate::{
    context_breakdown_with_tools, estimate_history_tokens, estimate_leading_system_tokens,
    estimate_message_tokens, estimate_schemas_tokens, estimate_system_tokens,
};
use super::*;

/// Reserved headroom inside the model's context window for the next response
/// and provider framing.
pub(crate) const COMPACT_CONTEXT_BUFFER: u64 = 13_000;
/// Changed-file paths named in the compact note. The note is model-facing
/// context, not a git report: a long dirty workspace must not spend the tail
/// budget on a path dump.
const COMPACT_CHANGED_FILES_MAX: usize = 20;
/// Character cap for the same changed-file sentence (paths are user data and
/// may contain multi-byte characters).
const COMPACT_CHANGED_FILES_CHARS: usize = 800;

/// Why compaction runs. Only `Manual` is user-initiated; the other two keep
/// the UI-facing `automatic` value true.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CompactTrigger {
    Manual,
    /// Proactive preflight/watermark compaction before sampling.
    Watermark,
    /// Reactive recovery after a typed context-window error.
    Overflow,
}

impl CompactTrigger {
    fn automatic(self) -> bool {
        !matches!(self, Self::Manual)
    }

    /// Retained fraction of compactable content: manual/watermark keeps at most
    /// half, overflow at most one third (recovery must actually free room).
    pub(crate) fn keep_divisor(self) -> u64 {
        match self {
            Self::Overflow => 3,
            Self::Manual | Self::Watermark => 2,
        }
    }
}

/// Local outcome of a compaction attempt (not persisted; the UI-facing
/// `automatic` flag stays the only protocol value).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CompactOutcome {
    Completed,
    /// Nothing could be compacted (short history / no completed work to omit).
    Skipped,
    Cancelled,
    /// A **manual** compaction's summary failed; history was deliberately left
    /// untouched. Automatic compaction instead falls back to the note-only
    /// truncation after its bounded retries.
    Failed,
}

/// The point where the budget can accept the model's next request.
pub(crate) fn compact_trigger(config: &ResolvedModel) -> u64 {
    config
        .context_window
        .saturating_sub(config.max_output_tokens + COMPACT_CONTEXT_BUFFER)
}

/// Scan the durable session JSONL rollout on disk and partition its lines
/// (1-based, inclusive) into compaction windows separated by existing
/// `"type":"compact"` records. Returns `None` when the file is unreadable or
/// has no conversation lines beyond line 1 (`meta`).
pub(crate) fn rollout_compaction_windows(path: &Path) -> Option<Vec<(usize, usize)>> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut windows: Vec<(usize, usize)> = Vec::new();
    let mut window_start: usize = 2; // line 1 is `meta`
    let mut total_lines: usize = 0;
    for (idx, line) in content.lines().enumerate() {
        let line_no = idx + 1;
        total_lines = line_no;
        if line_no == 1 {
            continue;
        }
        let is_compact = serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_string))
            .is_some_and(|t| t == "compact");
        if is_compact {
            let end = line_no.saturating_sub(1);
            if end >= window_start {
                windows.push((window_start, end));
            }
            window_start = line_no + 1;
        }
    }
    if total_lines >= window_start {
        windows.push((window_start, total_lines));
    }
    (!windows.is_empty()).then_some(windows)
}

/// Build a Kimi-style `## Context Recovery` footer pointing the model at the
/// session's append-only JSONL transcript and exact 1-based line ranges of
/// compacted windows, so it can `Grep` and `Read` earlier details on demand
/// without guessing.
pub(crate) fn context_recovery_footer(path: &Path, windows: &[(usize, usize)]) -> Option<String> {
    if windows.is_empty() {
        return None;
    }
    let last_ix = windows.len() - 1;
    let mut window_lines = Vec::with_capacity(windows.len() + 1);
    for (ix, (start, end)) in windows.iter().enumerate() {
        let label = format!("  window {}: lines {start}-{end}", ix + 1);
        if ix == last_ix {
            window_lines.push(format!(
                "{label}  <- the conversation this note summarizes"
            ));
        } else {
            window_lines.push(label);
        }
    }
    let next_start = windows[last_ix].1 + 1;
    window_lines.push(format!(
        "  window {} (current) starts at line {next_start} with this compact record — already in your context; no need to read it.",
        windows.len() + 1
    ));
    Some(format!(
        "## Context Recovery\n\
         Everything before this note remains on disk in this session's append-only JSONL rollout (read-only):\n\
           {}\n\
         {}\n\
         If you need exact earlier command output, error text, file snippets, or user wording, look it up there instead of guessing:\n\
         - One JSON object per line (`type`: `user`, `assistant`, `tool_call`, `compact`, `recall`; skip bookkeeping records like `step_usage`, `turn_stats`, `prompt_snapshot`).\n\
         - Lines can be long: use Grep on the file for a keyword to locate exact line numbers, then Read only those lines with `offset` and `limit`.",
        path.display(),
        window_lines.join("\n"),
    ))
}

/// Changed-file sentence in the compact note, capped by count and characters
/// with an explicit remainder. Paths are untrusted user data; the note is not a
/// place for an unbounded dirty-tree dump.
fn changed_files_note(paths: &[String]) -> String {
    if paths.is_empty() {
        return "No files were changed in the meantime.".to_string();
    }
    let mut shown: Vec<&str> = Vec::new();
    let mut chars = 0usize;
    for path in paths {
        if shown.len() >= COMPACT_CHANGED_FILES_MAX {
            break;
        }
        let path_chars = path.chars().count();
        if !shown.is_empty() && chars + path_chars > COMPACT_CHANGED_FILES_CHARS {
            break;
        }
        shown.push(path);
        chars += path_chars + 2;
    }
    let remaining = paths.len() - shown.len();
    if remaining == 0 {
        format!("Files changed in the meantime: {}", shown.join(", "))
    } else {
        format!(
            "Files changed in the meantime: {}{} and {remaining} more",
            shown.join(", "),
            if shown.is_empty() { "" } else { "," }
        )
    }
}

impl Session {
    /// Conservatively project the next request's context occupancy. With a
    /// valid anchor, add only messages and schema growth after the persisted
    /// reply; otherwise fall back to the current-history estimate and the last
    /// real usage watermark.
    pub(crate) fn projected_context_tokens(
        &self,
        config: &ResolvedModel,
        tools_tokens: u64,
    ) -> u64 {
        project_context_tokens(
            self.run_state.usage_anchor.as_ref(),
            &self.agent.history,
            self.run_state.last_total_tokens,
            config,
            tools_tokens,
        )
    }

    /// Compact the conversation. The summary sidecar receives the omitted
    /// prefix (including media and leading handoffs) plus the instruction,
    /// never the verbatim retained tail. History is rebuilt as the frozen
    /// system, the marked user-role handoff note, the separately preserved
    /// active user (if the cut passed it), and a tail selected by token budget
    /// at complete user-segment or assistant-round boundaries.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn run_compact(
        &mut self,
        config: Option<&ResolvedModel>,
        trigger: CompactTrigger,
        instruction: Option<&str>,
        preserve_latest: bool,
        pause_pin: Option<usize>,
        tx: &async_channel::Sender<Event>,
        cancel: &CancellationToken,
    ) -> CompactOutcome {
        if let Some(config) = config {
            self.select_plugin_tools(config, tx).await;
        }
        self.run_compact_with_current_tools(
            config,
            trigger,
            instruction,
            preserve_latest,
            pause_pin,
            tx,
            cancel,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn run_compact_with_current_tools(
        &mut self,
        config: Option<&ResolvedModel>,
        trigger: CompactTrigger,
        instruction: Option<&str>,
        preserve_latest: bool,
        pause_pin: Option<usize>,
        tx: &async_channel::Sender<Event>,
        cancel: &CancellationToken,
    ) -> CompactOutcome {
        let automatic = trigger.automatic();
        // A manual compaction needs a model for the summary sidecar. Without
        // one there is no summary to install, and silently truncating the
        // history would violate the manual-failure contract; reject with a
        // typed nonfatal error and leave history untouched. Automatic triggers
        // always carry the resolved model.
        if trigger == CompactTrigger::Manual && config.is_none() {
            self.emit(
                |session_id| Event::Error {
                    session_id: Some(session_id),
                    error: CoreError::NoModelConfigured,
                    fatal: false,
                },
                tx,
            );
            return CompactOutcome::Failed;
        }
        // Manual /compact can follow a model switch without a turn: price
        // window-derived PDF reserves for the summarizing model before any
        // selection or budget math. Measured costs are untouched.
        if let Some(config) = config {
            self.reprice_document_estimates(config.context_window);
        }
        // Manual /compact must price and send the same tool set as a normal
        // step, so connect the lazy MCP snapshot before schemas are built.
        self.ensure_mcp_connected(tx).await;
        let leading_tokens = estimate_leading_system_tokens(&self.agent.history);
        let schemas = config.map(|_| self.root_schemas());
        let tools_tokens = schemas
            .as_ref()
            .map_or(0, |schemas| estimate_schemas_tokens(schemas));
        let changed_note = changed_files_note(&self.tracker.tracked_paths());
        let active_reminder = crate::task::notification::active_task_reminder(&self.state.tasks);
        let recovery_footer = self.rollout.as_ref().and_then(|rollout| {
            let path = rollout.path();
            let windows = rollout_compaction_windows(path)?;
            context_recovery_footer(path, &windows)
        });
        let combined_suffix = match (active_reminder.as_deref(), recovery_footer.as_deref()) {
            (Some(a), Some(b)) => Some(format!("{a}\n\n{b}")),
            (Some(a), None) => Some(a.to_string()),
            (None, Some(b)) => Some(b.to_string()),
            (None, None) => None,
        };
        let note_reserve = compaction_note_reserve(automatic, Some(&changed_note))
            + combined_suffix
                .as_deref()
                .map(estimate_system_tokens)
                .unwrap_or(0);
        let budget = match config {
            Some(config) => tail_budget(
                config.context_window,
                config.max_output_tokens,
                leading_tokens,
                tools_tokens,
                note_reserve,
            ),
            // No model: keep the newer half of the compactable content by the
            // same density, without a window to compare against.
            None => u64::MAX / 4,
        };
        // A pause continuation uses the full remaining budget; a skipped
        // pinned range would silently restart the turn from a steer message.
        let target = compaction_target(
            budget,
            compactable_tokens(&self.agent.history),
            trigger,
            preserve_latest,
        );
        let active_user = active_real_user_ix(&self.agent.history, &self.user_ixs);
        let pinned = pause_pin.filter(|_| preserve_latest);
        let Some(plan) = select_tail_plan(
            &self.agent.history,
            CompactionSelection {
                target_tokens: target,
                pin: pinned,
                keep_latest_segment: preserve_latest,
                active_user,
                active_run_start: active_run_start(&self.agent.history, active_user),
            },
        ) else {
            if preserve_latest {
                tracing::warn!(
                    paused = ?pinned,
                    "preserve-latest compaction cannot retain the paused assistant/server state and the steer within the budget; skipping"
                );
            }
            // Manual /compact needs a terminal event for its UI state; an
            // automatic preflight that cannot shrink stays silent so repeated
            // steps do not stack "nothing to compact" dividers.
            if trigger == CompactTrigger::Manual {
                let used_before = self.run_state.last_total_tokens;
                let run = self.run_envelope();
                self.emit(
                    |session_id| Event::ContextCompacted {
                        session_id,
                        run,
                        omitted: 0,
                        note: "The current conversation has no earlier completed work that compaction can omit; keeping it intact.".to_string(),
                        automatic,
                        used_before,
                        used_after: None,
                        summary: None,
                    },
                    tx,
                );
            }
            return CompactOutcome::Skipped;
        };
        // Composition is reported before the sidecar request so the panel
        // reflects the request that is about to be summarized.
        let run = self.run_envelope();
        self.emit(
            |session_id| Event::CompactStarted {
                session_id,
                automatic,
                run,
            },
            tx,
        );

        // One summary attempt plus the service's single typed-overflow retry.
        // Manual failure keeps the exact history; automatic failure falls back
        // to the note-only truncation after those bounded attempts.
        let summary_result = if let Some(config) = config {
            let empty = [];
            let summary_tools = schemas.as_deref().unwrap_or(&empty);
            CompactService::summarize(
                &SummaryCall {
                    session_id: &self.agent.id,
                    store: &self.store,
                    agent_id: None,
                    background: false,
                    step: None,
                },
                config,
                &self.agent.history,
                plan.tail_start,
                InstructionFlavor::Root,
                instruction,
                summary_tools,
                cancel,
            )
            .await
        } else {
            SummaryResult {
                summary: None,
                error: None,
                cancelled: false,
            }
        };
        if summary_result.cancelled || cancel.is_cancelled() {
            self.emit_turn_aborted(tx);
            return CompactOutcome::Cancelled;
        }
        if let Some(error) = &summary_result.error {
            if trigger == CompactTrigger::Manual {
                // Manual failure is not a silent truncation: keep the exact
                // history and surface a nonfatal error. TurnAborted clears the
                // UI's compacting state without recording a compaction.
                self.emit(
                    |session_id| Event::Error {
                        session_id: Some(session_id),
                        error: CoreError::Internal {
                            detail: format!(
                                "Compaction summary failed; the conversation was not changed: {error:?}"
                            ),
                        },
                        fatal: false,
                    },
                    tx,
                );
                self.emit_turn_aborted(tx);
                return CompactOutcome::Failed;
            }
            tracing::warn!(
                "summary request failed after bounded retries; automatic compaction uses the note-only truncation fallback: {error:?}"
            );
        }
        let summary = summary_result.summary;

        // Build the candidate history and recall stack first. Nothing is
        // installed until the Compact marker has been appended: a marker
        // failure must leave the live context exactly on the pre-compaction
        // shape, never compacted live while the durable rollout is not.
        let installed = match CompactService::install(
            &self.agent.history,
            plan,
            automatic,
            summary.as_deref(),
            Some(&changed_note),
            combined_suffix.as_deref(),
            tools_tokens,
        ) {
            Ok(installed) => installed,
            Err(error) => {
                tracing::warn!(
                    "compaction plan does not match the live history: {error}; skipping"
                );
                let used_before = self.run_state.last_total_tokens;
                let run = self.run_envelope();
                self.emit(
                    |session_id| Event::ContextCompacted {
                        session_id,
                        run,
                        omitted: 0,
                        note: "The compaction plan no longer matched the conversation; keeping the current context intact.".to_string(),
                        automatic,
                        used_before,
                        used_after: None,
                        summary: None,
                    },
                    tx,
                );
                return CompactOutcome::Skipped;
            }
        };
        let note = installed.note.clone();
        let tail_start = installed.tail_start;
        let omitted = installed.omitted;
        let preserved_user = installed.preserved_user;
        let preserved_count = installed.preserved_count;
        let tail_len = installed.tail_len;
        let used_after = installed.used_after;
        let next_history = installed.history;
        // Remap the recallable-message stack into the rewritten history:
        // entries inside the kept tail shift to their new position; entries
        // compacted away become None (the rollout file still holds them, so a
        // later recall stays possible as a tombstone-only operation). The
        // separately preserved user is a model-context checkpoint, not a new
        // recallable User record: its old entry stays a tombstone so recalling
        // it cannot expose the compact note.
        let next_user_ixs: Vec<Option<usize>> = self
            .user_ixs
            .iter()
            .map(|entry| {
                entry.and_then(|ix| {
                    (ix >= tail_start).then(|| 2 + preserved_count + (ix - tail_start))
                })
            })
            .collect();
        // The watermark estimate describes the candidate context; tool
        // schemas are part of the next request but not of the history.
        let used_before = self.run_state.last_total_tokens;
        self.record(&RolloutRecord::Compact {
            note: note.clone(),
            omitted,
            automatic,
            used_after: Some(used_after),
            used_before,
            summary: summary.clone(),
            tail_len: Some(tail_len),
            preserved_user,
            trigger: None,
        });
        if self.rollout_failure.is_some() {
            // The marker was not appended (or an earlier append already
            // poisoned the rollout). Keep the pre-compaction live state; the
            // sticky error is surfaced below. A manual /compact also needs a
            // terminal event so its in-progress UI state clears.
            self.report_rollout_failure(tx);
            if trigger == CompactTrigger::Manual {
                self.emit_turn_aborted(tx);
            }
            return CompactOutcome::Failed;
        }
        // The marker line is present. A sync failure means a crash could still
        // lose it, but the process is about to keep using the file in the same
        // shape a restart would replay (marker present); report it and keep the
        // installed context consistent with the file.
        self.sync_rollout();
        self.report_rollout_failure(tx);
        self.agent.history = next_history;
        self.agent.provider_session.reset();
        self.user_ixs = next_user_ixs;
        // Reset the usage watermark to an estimate of the compacted history
        // (corrected by the next real sample): without the reset the watermark
        // check would see the pre-compaction high value and immediately trigger
        // another automatic compaction, compacting the fresh summary again.
        self.run_state.last_total_tokens = Some(used_after);
        // The anchor describes a request shape that no longer exists.
        self.run_state.usage_anchor = None;
        // Composition of the rewritten surface (tools stay priced separately)
        let breakdown = context_breakdown_with_tools(&self.agent.history, tools_tokens);
        self.emit(
            |session_id| Event::ContextBreakdownChanged {
                session_id,
                breakdown,
            },
            tx,
        );
        // Refresh the capacity chip immediately (skipped when no model is configured; no window to report)
        if let Some(config) = config {
            let (cache_read_total, input_total) =
                (self.run_state.cache_read_total, self.run_state.input_total);
            let total = config.context_window;
            self.emit(
                |session_id| Event::ContextUsage {
                    session_id,
                    used: used_after,
                    total,
                    cache_read_total,
                    input_total,
                },
                tx,
            );
        }
        let run = self.run_envelope();
        self.emit(
            |session_id| Event::ContextCompacted {
                session_id,
                run,
                omitted,
                note,
                automatic,
                used_before,
                used_after: Some(used_after),
                summary,
            },
            tx,
        );
        self.touch_index();
        CompactOutcome::Completed
    }
}

mod select;
mod service;
mod summary;
#[cfg(test)]
mod tests;

#[cfg(test)]
use self::select::select_tail;
pub(crate) use self::select::{
    CompactionSelection, TailPlan, active_real_user_ix, active_run_start, compactable_tokens,
    project_context_tokens, segments, select_tail_plan, tail_budget,
};
pub(crate) use self::service::{
    CompactService, SummaryCall, SummaryResult, child_compaction_plan, compaction_note_reserve,
    compaction_target,
};
#[cfg(test)]
pub(crate) use self::service::{compaction_note, install_compaction};
pub use self::summary::COMPACTION_MARKER;
pub(crate) use self::summary::{InstructionFlavor, compaction_instruction};
