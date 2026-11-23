//! Fixed-density token estimates shared by the compaction budget, the
//! post-compaction watermark and the context-composition panel. One density
//! everywhere (4 characters per token) keeps the heuristic figures of one
//! session mutually consistent; they are not provider counts.

use pig_protocol::ContextBreakdown;
use pig_provider::ChatMsg;

/// Fixed text density: 4 characters per token.
const CHARS_PER_TOKEN: u64 = 4;

/// Per-message framing overhead (role fields and separators).
const MESSAGE_OVERHEAD: u64 = 4;

/// Extra characters charged per tool call on top of its argument JSON.
const TOOL_CALL_OVERHEAD: usize = 20;

/// Fixed token price of one image. Media is never estimated from its base64
/// length (a compressed screenshot would look enormous); a conservative
/// constant keeps the budget stable across encodings.
const IMAGE_TOKENS: u64 = 1_000;

/// Estimated tokens of one history entry: visible text (or the replacement
/// sent for a stopped tool receipt), reasoning, tool-call arguments, native
/// replay items (including encrypted credentials/citations), and media
/// reservations, plus per-message framing. PDF documents are priced from the
/// cost cached when the document was loaded or persisted; estimation never
/// decodes or scans the bytes.
pub(crate) fn estimate_message_tokens(msg: &ChatMsg) -> u64 {
    let mut chars = 0usize;
    if msg.stopped {
        chars += pig_provider::STOPPED_TOOL_OUTPUT.chars().count();
    } else {
        chars += msg.content.as_deref().unwrap_or("").chars().count();
    }
    chars += msg.reasoning.as_deref().unwrap_or("").chars().count();
    if let Some(calls) = &msg.tool_calls {
        for call in calls {
            chars += call.function.arguments.chars().count() + TOOL_CALL_OVERHEAD;
        }
    }
    if let Some(replay) = &msg.replay {
        chars += replay_estimate_chars(msg, replay);
    }
    chars as u64 / CHARS_PER_TOKEN
        + MESSAGE_OVERHEAD
        + msg.images.len() as u64 * IMAGE_TOKENS
        + msg.documents.iter().map(document_tokens).sum::<u64>()
}

/// PDF cost cached by the media pipeline. `None` only occurs for a message
/// built outside that pipeline (tests, hand-written JSONL), which has no
/// trusted window to price an opaque page tree against; it contributes zero
/// rather than resurrecting a fixed per-document constant. A window fallback
/// is repriced when a model window is resolved, never here.
fn document_tokens(document: &pig_provider::ChatDocument) -> u64 {
    document
        .token_estimate
        .map(|estimate| estimate.tokens)
        .unwrap_or(0)
}

/// Chars contributed by native replay beyond the plaintext fields already
/// counted on the message. Text content and reasoning are counted once from
/// `content`/`reasoning`; tool calls matched by id are counted once from
/// `tool_calls`. Only opaque metadata (citations, encrypted/reasoning
/// credentials, hosted-search items) is added here.
fn replay_estimate_chars(msg: &ChatMsg, replay: &pig_provider::AssistantReplay) -> usize {
    use pig_provider::AssistantReplayItem;
    let mut chars = 0usize;
    for item in &replay.items {
        match item {
            // Text is already counted from the message content; only citation
            // metadata (including encrypted indexes) adds to the budget.
            AssistantReplayItem::Text { citations, .. } => {
                if !citations.is_empty() {
                    chars += serde_json::to_string(citations)
                        .map(|json| json.chars().count())
                        .unwrap_or(0);
                }
            }
            // A tool call represented in `tool_calls` has its arguments and
            // name counted already; opaque ids do not need another charge.
            AssistantReplayItem::ToolCall { call }
                if msg
                    .tool_calls
                    .as_ref()
                    .is_some_and(|calls| calls.iter().any(|known| known.id == call.id)) => {}
            AssistantReplayItem::ResponsesReasoning {
                summary,
                content,
                encrypted_content,
                id,
            } => {
                let mut item_chars = serde_json::to_string(item)
                    .map(|json| json.chars().count())
                    .unwrap_or(0);
                // The streamed reasoning text is recorded in `reasoning`; do not
                // charge the same plaintext twice, but keep encrypted fields.
                let reasoning = msg.reasoning.as_deref().unwrap_or("");
                let plaintext = summary
                    .iter()
                    .chain(content.iter())
                    .map(|text| text.chars().count())
                    .sum::<usize>();
                if !reasoning.is_empty() && plaintext > 0 {
                    let opaque = encrypted_content
                        .as_ref()
                        .map(|value| value.chars().count())
                        .unwrap_or(0);
                    item_chars = item_chars
                        .saturating_sub(reasoning.chars().count().min(plaintext))
                        .max(opaque);
                }
                let _ = id;
                chars += item_chars;
            }
            AssistantReplayItem::AnthropicThinking { thinking, .. } => {
                let item_chars = serde_json::to_string(item)
                    .map(|json| json.chars().count())
                    .unwrap_or(0);
                let already = if msg.reasoning.as_deref() == Some(thinking.as_str()) {
                    thinking.chars().count()
                } else {
                    0
                };
                chars += item_chars.saturating_sub(already);
            }
            other => {
                chars += serde_json::to_string(other)
                    .map(|json| json.chars().count())
                    .unwrap_or(0);
            }
        }
    }
    chars
}

/// Estimated tokens of a whole history: text, reasoning and tool-call
/// arguments over the fixed density, plus per-message framing.
pub(crate) fn estimate_history_tokens(history: &[ChatMsg]) -> u64 {
    history.iter().map(estimate_message_tokens).sum()
}

/// Estimated tokens of the frozen system prompt plus every earlier compaction
/// handoff at the head of the history. These survive the next rewrite (the new
/// note summarizes their content), so the tail budget reserves them.
pub(crate) fn estimate_leading_system_tokens(history: &[ChatMsg]) -> u64 {
    let fixed = crate::rollout::fixed_prefix_len(history);
    history[..fixed].iter().map(estimate_message_tokens).sum()
}

/// Estimated tokens of the system prompt: the same accounting
/// [`estimate_history_tokens`] applies to a single system message.
pub(crate) fn estimate_system_tokens(prompt: &str) -> u64 {
    prompt.chars().count() as u64 / CHARS_PER_TOKEN + MESSAGE_OVERHEAD
}

/// Estimated tokens of the request's tool schemas (their JSON serialization).
pub(crate) fn estimate_schemas_tokens(schemas: &[serde_json::Value]) -> u64 {
    if schemas.is_empty() {
        return 0;
    }
    let chars = serde_json::to_string(schemas)
        .map(|json| json.chars().count())
        .unwrap_or(0);
    chars as u64 / CHARS_PER_TOKEN + MESSAGE_OVERHEAD
}

/// Composition of the current surface with a precomputed tool estimate.
/// `messages` is derived by subtraction so the three figures always add up to
/// [`estimate_history_tokens`] for the same history.
pub(crate) fn context_breakdown_with_tools(history: &[ChatMsg], tools: u64) -> ContextBreakdown {
    let system = history
        .first()
        .filter(|msg| msg.role == "system")
        .map(|msg| estimate_system_tokens(msg.content.as_deref().unwrap_or("")))
        .unwrap_or(0);
    let surface = estimate_history_tokens(history);
    ContextBreakdown {
        system,
        tools,
        messages: surface.saturating_sub(system),
    }
}

/// Composition of the current surface for a request that would send `schemas`.
pub(crate) fn context_breakdown(
    history: &[ChatMsg],
    schemas: &[serde_json::Value],
) -> ContextBreakdown {
    context_breakdown_with_tools(history, estimate_schemas_tokens(schemas))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pig_provider::{ChatMsg, ToolCall};

    fn schema(name: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": { "name": name, "description": "d", "parameters": { "type": "object" } }
        })
    }

    #[test]
    fn density_and_overhead_match_the_history_fold() {
        // "1234" is exactly 4 chars -> 1 token + 4 framing
        assert_eq!(estimate_system_tokens("1234"), 5);
        assert_eq!(estimate_system_tokens(""), 4);
        let history = vec![
            ChatMsg::system("1234".into()),
            ChatMsg::user("12345678".into()),
        ];
        // 12 chars / 4 + 2 messages * 4
        assert_eq!(estimate_history_tokens(&history), 3 + 8);
    }

    #[test]
    fn breakdown_parts_add_up_to_the_history_fold() {
        let history = vec![
            ChatMsg::system("1234".into()),
            ChatMsg::user("12345678".into()),
        ];
        let breakdown = context_breakdown_with_tools(&history, 7);
        assert_eq!(breakdown.system, 5);
        assert_eq!(breakdown.tools, 7);
        assert_eq!(
            breakdown.system + breakdown.messages,
            estimate_history_tokens(&history)
        );
    }

    #[test]
    fn schemas_are_priced_from_their_json() {
        assert_eq!(estimate_schemas_tokens(&[]), 0);
        let schemas = vec![schema("Read")];
        let chars = serde_json::to_string(&schemas).unwrap().chars().count() as u64;
        assert_eq!(estimate_schemas_tokens(&schemas), chars / 4 + 4);
    }

    #[test]
    fn media_is_priced_by_kind_not_base64_length() {
        let mut message = ChatMsg::tool_result("c1", "ok".into());
        let plain = estimate_message_tokens(&message);
        message.images = vec![pig_provider::ChatImage {
            media_type: "image/png".into(),
            data_base64: "A".repeat(100_000),
            label: None,
        }];
        assert_eq!(estimate_message_tokens(&message), plain + 1_000);
        message.documents = vec![pig_provider::ChatDocument {
            media_type: "application/pdf".into(),
            // The payload is irrelevant here on purpose: the cached cost is
            // the only input the estimator may use.
            data_base64: "not base64 at all".into(),
            filename: "a.pdf".into(),
            label: None,
            token_estimate: Some(pig_provider::DocumentTokenEstimate::measured(
                2 * pig_utils::pdf::PDF_TOKENS_PER_PAGE,
            )),
        }];
        assert_eq!(
            estimate_message_tokens(&message),
            plain + 1_000 + 2 * pig_utils::pdf::PDF_TOKENS_PER_PAGE,
            "multi-page PDFs are priced by the cached page-count cost"
        );
        // Unknown/incomplete structures carry the window-aware fallback that
        // was computed once at load; the estimator does not re-scan.
        let window = 128_000;
        message.documents[0].token_estimate =
            Some(pig_provider::DocumentTokenEstimate::window_fallback(
                pig_utils::pdf::unknown_document_token_reserve(window),
                window,
            ));
        assert_eq!(
            estimate_message_tokens(&message),
            plain + 1_000 + pig_utils::pdf::unknown_document_token_reserve(window)
        );
    }

    #[test]
    fn stopped_receipts_are_priced_by_the_replacement_text() {
        let mut message = ChatMsg::tool_result("c1", "x".repeat(10_000));
        let full = estimate_message_tokens(&message);
        message.stopped = true;
        message.is_error = true;
        let stopped = estimate_message_tokens(&message);
        assert!(
            stopped < full,
            "the request sends the fixed replacement, not the stored output"
        );
        assert_eq!(
            stopped,
            (pig_provider::STOPPED_TOOL_OUTPUT.chars().count() as u64 / 4) + 4
        );
    }

    #[test]
    fn native_replay_does_not_double_count_plaintext_or_tool_calls() {
        let thinking = "reasoning ".repeat(40);
        let answer = "answer ".repeat(50);
        let call = ToolCall {
            id: "c1".into(),
            name: "Read".into(),
            arguments: "{\"path\":\"a\"}".into(),
        };
        let mut message = ChatMsg::assistant(answer, vec![call], Some(thinking.clone()));
        let without_replay = estimate_message_tokens(&message);
        message.replay = Some(pig_provider::AssistantReplay {
            origin: pig_provider::ReplayOrigin::new(
                pig_protocol::ApiFormat::OpenAiResponses,
                "https://api.openai.com/v1",
                "gpt-5",
                "test-key",
            ),
            items: vec![
                pig_provider::AssistantReplayItem::ResponsesReasoning {
                    id: Some("rs_1".into()),
                    summary: vec![thinking],
                    content: vec![],
                    encrypted_content: Some("opaque".into()),
                },
                pig_provider::AssistantReplayItem::Text {
                    text: "answer ".repeat(50),
                    citations: vec![],
                },
                pig_provider::AssistantReplayItem::ToolCall {
                    call: pig_provider::ToolCall {
                        id: "c1".into(),
                        name: "Read".into(),
                        arguments: "{\"path\":\"a\"}".into(),
                    }
                    .to_wire(),
                },
            ],
        });
        let with_replay = estimate_message_tokens(&message);
        let delta = with_replay - without_replay;
        let duplicated = ((message.content.as_deref().unwrap_or("").chars().count()
            + message.reasoning.as_deref().unwrap_or("").chars().count()
            + "{\"path\":\"a\"}".chars().count())
            / 4) as u64;
        assert!(
            delta < duplicated,
            "replayed plaintext/tool arguments must not be charged twice (delta {delta}, duplication would be ~{duplicated})"
        );
        assert!(
            delta > 0,
            "opaque replay metadata (the encrypted reasoning field) still counts"
        );
    }

    #[test]
    fn native_replay_and_citations_are_counted_including_opaque_fields() {
        let mut message = ChatMsg::assistant("answer".into(), vec![], None);
        let plain = estimate_message_tokens(&message);
        message.replay = Some(pig_provider::AssistantReplay {
            origin: pig_provider::ReplayOrigin::new(
                pig_protocol::ApiFormat::AnthropicMessages,
                "https://api.anthropic.com",
                "claude-sonnet-4-5",
                "test-key",
            ),
            items: vec![
                pig_provider::AssistantReplayItem::Text {
                    text: "answer".into(),
                    citations: vec![pig_provider::TextCitation {
                        url: "https://example.com".into(),
                        title: "Example".into(),
                        snippet: None,
                        start_index: Some(0),
                        end_index: Some(6),
                        encrypted_index: Some("opaque".repeat(100)),
                        cited_text: None,
                    }],
                },
                pig_provider::AssistantReplayItem::AnthropicRedactedThinking {
                    data: "opaque".repeat(100),
                },
            ],
        });
        assert!(
            estimate_message_tokens(&message) > plain,
            "native replay credentials and citations must enter the budget"
        );
    }

    #[test]
    fn leading_system_tokens_include_earlier_compact_notes() {
        let history = vec![
            ChatMsg::system("frozen".into()),
            ChatMsg::system("earlier note".into()),
            ChatMsg::user("u".into()),
        ];
        let leading = estimate_leading_system_tokens(&history);
        assert_eq!(
            leading,
            estimate_message_tokens(&history[0]) + estimate_message_tokens(&history[1])
        );
        assert!(leading < estimate_history_tokens(&history));

        let handoff = crate::session::compact::compaction_note(1, true, Some(""), None);
        let history = vec![
            ChatMsg::system("frozen".into()),
            ChatMsg::user(handoff),
            ChatMsg::user("u".into()),
        ];
        assert_eq!(
            estimate_leading_system_tokens(&history),
            estimate_message_tokens(&history[0]) + estimate_message_tokens(&history[1]),
            "a user-role handoff stays in the fixed prefix"
        );
    }

    #[test]
    fn tool_call_arguments_and_reasoning_are_counted_in_messages() {
        let call = ToolCall {
            id: "c1".into(),
            name: "Read".into(),
            arguments: "{\"path\":\"a\"}".into(),
        };
        let history = vec![
            ChatMsg::system("sys".into()),
            ChatMsg::assistant("ok".into(), vec![call], Some("thinking".into())),
        ];
        let breakdown = context_breakdown(&history, &[schema("Read")]);
        assert!(breakdown.messages > breakdown.system);
        assert_eq!(
            breakdown.system + breakdown.messages,
            estimate_history_tokens(&history)
        );
    }
}
