use super::*;

impl Session {
    /// Compact: prefer a model summary; fall back to plain truncation on failure. Returns false = cancelled.
    /// History is rebuilt as system + summary message + verbatim tail messages (cut at a user boundary, at most 4).
    /// instruction = the user's special request for this summary (appended by manual /compact; None for automatic compaction)
    pub async fn run_compact(
        &mut self,
        config: Option<&ResolvedModel>,
        automatic: bool,
        instruction: Option<&str>,
        tx: &async_channel::Sender<Event>,
        cancel: &CancellationToken,
    ) -> bool {
        const KEEP: usize = 4;
        if self.history.len() <= KEEP + 1 {
            self.emit(
                |session_id, seq| Event::ContextCompacted {
                    session_id,
                    seq,
                    omitted: 0,
                    note: "History is short; no compaction needed.".to_string(),
                    automatic,
                },
                tx,
            );
            return true;
        }
        let changed: Vec<String> = self.tracker.tracked_paths();
        // The summary is a blocking request with no other events in between; notify the UI to enter the "compacting" state first
        self.emit(
            |session_id, seq| Event::CompactStarted {
                session_id,
                seq,
                automatic,
            },
            tx,
        );

        let summary = match config {
            Some(config) => {
                let messages = build_summary_messages(&self.history, config, instruction);
                let tools = self.root_schemas();
                match provider::complete_messages(config, &messages, &tools, cancel).await {
                    Ok(summary) => Some(summary),
                    Err(_) if cancel.is_cancelled() => {
                        self.emit(|session_id, seq| Event::TurnAborted { session_id, seq }, tx);
                        return false;
                    }
                    Err(error) => {
                        eprintln!(
                            "[pig-core] summary request failed, falling back to truncation: {error:?}"
                        );
                        None
                    }
                }
            }
            None => None,
        };

        let changed_note = if changed.is_empty() {
            "No files were changed in the meantime.".to_string()
        } else {
            format!("Files changed in the meantime: {}", changed.join(", "))
        };
        let tail = select_tail(&self.history, KEEP);
        let omitted = self.history.len() - 1 - tail.len();
        let note = match &summary {
            Some(summary) => format!(
                "[Earlier context compacted{} — model summary] {omitted} messages omitted.

{summary}

How to use this summary: it is a faithful record of the earlier conversation — do not redo work it marks as done, and do not re-ask the user for information it already contains. Live state such as background tasks and file contents may have changed since; re-verify with tools when needed instead of trusting the summary blindly. {changed_note}",
                if automatic { " (automatic)" } else { "" },
            ),
            None => format!(
                "[Earlier context compacted{}] {omitted} messages were omitted (summary generation failed, \
                 the history was truncated directly). The omitted content is no longer in context — \
                 re-verify details with Read/Grep when needed instead of guessing from memory. {changed_note}",
                if automatic { " (automatic)" } else { "" },
            ),
        };

        self.history = vec![self.history[0].clone(), ChatMsg::system(note.clone())];
        self.history.extend(tail);
        // Reset the usage watermark to an estimate of the compacted history (corrected by the next real sample):
        // without the reset the watermark check would see the pre-compaction high value and immediately trigger
        // another automatic compaction at the start of the next turn, compacting the freshly generated summary again
        let used_after = estimate_history_tokens(&self.history);
        self.last_total_tokens = Some(used_after);
        self.record(&RolloutRecord::Compact {
            note: note.clone(),
            omitted,
            automatic,
            used_after: Some(used_after),
        });
        // Refresh the capacity chip immediately (skipped when no model is configured; no window to report)
        if let Some(config) = config {
            let (cache_read_total, input_total) = (self.cache_read_total, self.input_total);
            let total = config.context_window;
            self.emit(
                |session_id, seq| Event::ContextUsage {
                    session_id,
                    seq,
                    used: used_after,
                    total,
                    cache_read_total,
                    input_total,
                },
                tx,
            );
        }
        self.emit(
            |session_id, seq| Event::ContextCompacted {
                session_id,
                seq,
                omitted,
                note,
                automatic,
            },
            tx,
        );
        self.touch_index();
        true
    }
}

/// The tail kept by compaction: at most `keep` messages, and it must be cut at a user boundary.
/// - Leading assistant (with tool_calls): the Anthropic endpoint requires the first non-system
///   message to be user (400 otherwise), and OpenAI-compatible endpoints would also get a semantically broken head;
/// - Leading tool: an orphan tool_result (no preceding tool_use); both providers reject it;
/// - No user inside the window (automatic compaction triggered mid-tool-chain): degrade to keeping only
///   the last user message — the current turn's user request must stay in the verbatim context, not rely entirely on the summary.
fn select_tail(history: &[ChatMsg], keep: usize) -> Vec<ChatMsg> {
    let mut tail: Vec<ChatMsg> = history[history.len().saturating_sub(keep)..].to_vec();
    while !tail.is_empty() && tail[0].role != "user" {
        tail.remove(0);
    }
    if tail.is_empty()
        && let Some(ix) = history.iter().rposition(|m| m.role == "user")
    {
        tail.push(history[ix].clone());
    }
    tail
}

/// Roughly estimate the history's token count (~4 chars/token + 4 tokens of overhead per message):
/// only a transitional value for the post-compaction watermark/capacity display; underestimating is
/// the safe direction (no false automatic-compaction triggers), and the next real Usage sample corrects it.
fn estimate_history_tokens(history: &[ChatMsg]) -> u64 {
    let mut chars = 0usize;
    for msg in history {
        chars += msg.content.as_deref().unwrap_or("").chars().count();
        chars += msg.reasoning.as_deref().unwrap_or("").chars().count();
        if let Some(calls) = &msg.tool_calls {
            for call in calls {
                chars += call.function.arguments.chars().count() + 20;
            }
        }
    }
    (chars / 4 + history.len() * 4) as u64
}

pub const COMPACTION_MARKER: &str = "[COMPACTION]";
// ---- Session auto-naming (aligned with ZCode's title-generation sidecar) ----

/// Summary request messages: frozen system + verbatim history + one trailing instruction user message.
/// The same system/tools/history bytes as the normal conversation request -> the provider's prefix cache
/// hits the cache written by the last turn (ZCode routes its summary through the same projection pipeline /
/// kimi-code reuses the same history array — the same trade-off; the old "concatenate everything into one
/// big user message + truncate each entry" shape was a cache killer: every compaction paid full-price input
/// and long tool output was lossy). The tools field is carried along to align the cache prefix.
/// When history exceeds the budget, drop whole messages from the head (same as kimi-code preShrink) and trim
/// to a user boundary — must not start with tool (orphan tool_result) and Anthropic requires the first
/// message to be user; automatic compaction by construction never exceeds the window (the trigger point is
/// below window minus output reserve), so this path mainly guards the manual case.
fn build_summary_messages(
    history: &[ChatMsg],
    config: &ResolvedModel,
    instruction: Option<&str>,
) -> Vec<ChatMsg> {
    let budget = config
        .context_window
        .saturating_sub(config.max_output_tokens + 13_000);
    let mut head = 1usize; // history[0] = frozen system, always kept
    while history.len() - head > 1 && estimate_history_tokens(&history[head..]) > budget {
        head += 1;
    }
    let mut middle: Vec<ChatMsg> = history[head..].to_vec();
    // Trim to a user boundary (after the head drop the first message may be assistant/tool; both endpoints reject that)
    while !middle.is_empty() && middle[0].role != "user" {
        middle.remove(0);
    }
    let mut messages = vec![history[0].clone()];
    messages.extend(middle);
    messages.push(ChatMsg::user(compaction_instruction(instruction)));
    messages
}

fn compaction_instruction(instruction: Option<&str>) -> String {
    let custom = instruction
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(|text| {
            format!("\nThe user added a special request for this summary — emphasize it: {text}\n")
        })
        .unwrap_or_default();
    format!(
        "{COMPACTION_MARKER} Compress the coding-assistant conversation history above into a handoff summary for the same assistant to continue working with after compaction. Write it in the language the conversation has been using, with the following sections (skip sections that have no content):
1. User goals: what the user wants, including explicitly stated requirements and preferences.
2. Completed: work that is done and verified conclusions.
3. File changes: files touched (path + one-line description of the change).
4. Key decisions: technical choices and their rationale; directions the user rejected or corrected.
5. In progress and pending: unfinished steps, next-step plans, known blockers; if an implementation plan has been approved or is being drafted, its steps and conclusions must be preserved in full (execution continues from it after compaction).
6. Important context: running commands/background tasks, verbatim key error messages, environment essentials.
Keep only what is needed to continue the work; preserve hard facts such as file paths, commands, and verbatim error messages — do not invent details. Keep the whole summary within 1200 characters.{custom}
Do not call any tools; output the summary text directly.
"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> ChatMsg {
        ChatMsg::user(text.to_string())
    }

    fn assistant_text(text: &str) -> ChatMsg {
        ChatMsg::assistant(text.to_string(), vec![], None)
    }

    fn assistant_call(id: &str) -> ChatMsg {
        ChatMsg::assistant(
            String::new(),
            vec![crate::provider::ToolCall {
                id: id.into(),
                name: "Read".into(),
                arguments: "{}".into(),
            }],
            None,
        )
    }

    fn tool(id: &str) -> ChatMsg {
        ChatMsg::tool_result(id, "output".to_string())
    }

    fn roles(msgs: &[ChatMsg]) -> Vec<&str> {
        msgs.iter().map(|m| m.role.as_str()).collect()
    }

    /// Window starts at user: kept as-is (the most common plain-text tail shape)
    #[test]
    fn tail_starting_at_user_kept_as_is() {
        let history = vec![
            ChatMsg::system("s".into()),
            user("u1"),
            assistant_text("a1"),
            user("u2"),
            assistant_text("a2"),
            user("u3"),
            assistant_text("a3"),
        ];
        let tail = select_tail(&history, 4);
        assert_eq!(roles(&tail), ["user", "assistant", "user", "assistant"]);
    }

    /// Window starts at tool/assistant (after a tool-chain round): trim to a user boundary —
    /// Anthropic requires the first message to be user, and both providers reject orphan tool_results
    #[test]
    fn tail_trims_to_user_boundary() {
        let history = vec![
            ChatMsg::system("s".into()),
            user("u1"),
            assistant_call("c1"),
            tool("c1"),
            assistant_text("a1"),
            user("u2"),
            assistant_text("a2"),
        ];
        let tail = select_tail(&history, 4);
        assert_eq!(roles(&tail), ["user", "assistant"]);
        assert_eq!(tail[0].content.as_deref(), Some("u2"));
    }

    /// No user inside the window (automatic compaction triggered mid-tool-chain): degrade to keeping only
    /// the last user message — the current turn's user request must be preserved verbatim, not rely entirely on the summary
    #[test]
    fn tail_without_user_falls_back_to_last_user_message() {
        let history = vec![
            ChatMsg::system("s".into()),
            user("u1"),
            assistant_call("c1"),
            tool("c1"),
            assistant_call("c2"),
            tool("c2"),
        ];
        let tail = select_tail(&history, 4);
        assert_eq!(roles(&tail), ["user"]);
        assert_eq!(tail[0].content.as_deref(), Some("u1"));
    }

    /// Not a single user in history (defensive): return empty, never fabricate an orphan segment
    #[test]
    fn tail_without_any_user_returns_empty() {
        let history = vec![
            ChatMsg::system("s".into()),
            assistant_call("c1"),
            tool("c1"),
        ];
        assert!(select_tail(&history, 4).is_empty());
    }

    /// Estimate: covers content/reasoning/tool arguments and per-message overhead, monotonically increasing
    #[test]
    fn estimate_counts_content_reasoning_and_arguments() {
        let base = estimate_history_tokens(&[user("1234")]);
        // 4 chars of content / 4 = 1 + 4 per message = 5
        assert_eq!(base, 5);
        let with_reasoning = estimate_history_tokens(&[ChatMsg::assistant(
            "1234".into(),
            vec![],
            Some("12345678".into()),
        )]);
        // (4+8)/4 = 3 + 4 = 7
        assert_eq!(with_reasoning, 7);
        let with_call = estimate_history_tokens(&[assistant_call("c1")]);
        // arguments "{}" 2 chars + call constant 20 = 22/4 = 5 + 4 = 9
        assert_eq!(with_call, 9);
    }

    fn test_model(context_window: u64) -> crate::provider::ResolvedModel {
        crate::provider::ResolvedModel {
            base_url: "http://x".into(),
            api_key: "k".into(),
            model: "m".into(),
            context_window,
            max_output_tokens: 1000,
            api_format: pig_protocol::ApiFormat::OpenAiChat,
            reasoning_params: None,
            cap_web_search: false,
            web_search_tool: None,
            input_image: false,
            provider_name: "t".into(),
        }
    }

    /// Summary request shape: frozen system first, verbatim history in the middle, instruction user last —
    /// byte-for-byte prefix equality with the conversation request is the precondition for cache hits; no reassembly/truncation
    #[test]
    fn summary_messages_wrap_verbatim_history() {
        let history = vec![
            ChatMsg::system("s".into()),
            user("u1"),
            assistant_call("c1"),
            tool("c1"),
            assistant_text("a1"),
            user("u2"),
            assistant_text("a2"),
        ];
        let messages = build_summary_messages(&history, &test_model(128_000), None);
        assert_eq!(
            roles(&messages),
            [
                "system",
                "user",
                "assistant",
                "tool",
                "assistant",
                "user",
                "assistant",
                "user"
            ]
        );
        // Middle segment matches history entry by entry (verbatim, no truncation or reassembly)
        for (a, b) in messages[1..messages.len() - 1].iter().zip(&history[1..]) {
            assert_eq!(a.content, b.content);
            assert_eq!(a.tool_call_id, b.tool_call_id);
        }
        let instruction = &messages[messages.len() - 1];
        assert_eq!(instruction.role, "user");
        assert!(
            instruction.content.as_deref().is_some_and(
                |c| c.contains(COMPACTION_MARKER) && c.contains("Do not call any tools")
            ),
            "instruction should close the request: {instruction:?}"
        );
    }

    /// History over budget: drop whole messages from the head until it fits, and trim to a user boundary (must not start with tool/assistant)
    #[test]
    fn summary_messages_pre_shrink_drops_from_head_at_user_boundary() {
        let history = vec![
            ChatMsg::system("s".into()),
            user("u1"),
            assistant_call("c1"),
            tool("c1"),
            assistant_text("a1"),
            user("u2"),
            assistant_text("a2"),
        ];
        // Tiny budget: only the last one or two messages fit
        let messages = build_summary_messages(&history, &test_model(1_100), None);
        assert_eq!(messages[0].role, "system");
        assert_eq!(
            messages[1].role,
            "user",
            "first message after head-drop must be user: {}",
            roles(&messages).join(",")
        );
        assert!(
            messages.len() < history.len() + 1,
            "prefix should have been dropped: {messages:?}"
        );
    }

    /// Head drop + boundary trim empties the middle: only system + instruction remain (the summary can still be produced; the request is valid)
    #[test]
    fn summary_messages_fallback_to_instruction_only() {
        let history = vec![
            ChatMsg::system("s".into()),
            assistant_call("c1"),
            tool("c1"),
        ];
        let messages = build_summary_messages(&history, &test_model(1_100), None);
        assert_eq!(roles(&messages), ["system", "user"]);
    }

    /// Custom instruction: the focus note appended by /compact enters the summary request's trailing instruction; blank is ignored
    #[test]
    fn summary_instruction_includes_custom_focus() {
        let history = vec![
            ChatMsg::system("s".into()),
            user("u1"),
            assistant_text("a1"),
        ];
        let messages =
            build_summary_messages(&history, &test_model(128_000), Some("focus on the README"));
        let instruction = messages[messages.len() - 1]
            .content
            .as_deref()
            .unwrap_or_default();
        assert!(
            instruction.contains("special request") && instruction.contains("focus on the README"),
            "custom focus should enter the instruction: {instruction}"
        );
        // Blank = no custom block
        let messages = build_summary_messages(&history, &test_model(128_000), Some("  "));
        let instruction = messages[messages.len() - 1]
            .content
            .as_deref()
            .unwrap_or_default();
        assert!(
            !instruction.contains("special request"),
            "blank instruction should be ignored: {instruction}"
        );
    }
}
