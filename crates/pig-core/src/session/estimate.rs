//! UTF-8 byte-density text and resolution-aware media token estimates shared by
//! the compaction budget, the post-compaction watermark and the
//! context-composition panel.
//!
//! Counting UTF-8 bytes (`4 bytes / token`, matching Codex) naturally prices
//! ASCII at 4 chars/token, 3-byte CJK ideographs at ~1.33 chars/token, and
//! 4-byte emoji at 1 char/token without a heavy local BPE dependency.

use pig_protocol::ContextBreakdown;
use pig_provider::{ChatImage, ChatMsg};

/// Fixed UTF-8 byte density: 4 bytes per token (4 ASCII chars or ~1.33 CJK chars).
const BYTES_PER_TOKEN: u64 = 4;

/// Per-message framing overhead (role fields and separators).
const MESSAGE_OVERHEAD: u64 = 4;

/// Extra UTF-8 bytes charged per tool call on top of its argument JSON.
const TOOL_CALL_OVERHEAD: usize = 20;

/// Fallback token price of one image when dimensions cannot be probed from its
/// header (e.g. test placeholders or opaque/corrupt bytes).
const DEFAULT_IMAGE_TOKENS: u64 = 1_000;
/// Maximum long edge used for vision token scaling (Anthropic/modern VLM cap).
const IMAGE_MAX_LONG_EDGE: u64 = 1_568;
/// Pixel area per vision token after long-edge scaling (`w * h / 750`).
const IMAGE_PIXELS_PER_TOKEN: u64 = 750;
/// Lower bound for tiny icons so framing/patch overhead is never undercounted.
const MIN_IMAGE_TOKENS: u64 = 85;
/// Upper bound per single image to guard pathological dimension headers.
const MAX_IMAGE_TOKENS: u64 = 4_096;

/// Estimate vision tokens from decoded image pixel dimensions.
pub(crate) fn image_tokens_for_dimensions(width: u32, height: u32) -> u64 {
    if width == 0 || height == 0 {
        return DEFAULT_IMAGE_TOKENS;
    }
    let mut w = width as u64;
    let mut h = height as u64;
    let long_edge = w.max(h);
    if long_edge > IMAGE_MAX_LONG_EDGE {
        w = (w * IMAGE_MAX_LONG_EDGE).div_ceil(long_edge);
        h = (h * IMAGE_MAX_LONG_EDGE).div_ceil(long_edge);
    }
    (w.saturating_mul(h) / IMAGE_PIXELS_PER_TOKEN).clamp(MIN_IMAGE_TOKENS, MAX_IMAGE_TOKENS)
}

/// Probe image dimensions from its encoded payload header to price vision
/// tokens; falls back to [`DEFAULT_IMAGE_TOKENS`] when the payload is not a
/// decodable image header. Never charges raw base64 length.
fn estimate_image_tokens(image: &ChatImage) -> u64 {
    let Ok(bytes) = pig_utils::image::base64_decode(&image.data_base64) else {
        return DEFAULT_IMAGE_TOKENS;
    };
    match pig_utils::image::image_dimensions(&bytes) {
        Some((width, height)) => image_tokens_for_dimensions(width, height),
        None => DEFAULT_IMAGE_TOKENS,
    }
}

/// Estimated tokens of one history entry: visible text (or the replacement
/// sent for a stopped tool receipt), reasoning, tool-call arguments, native
/// replay items (including encrypted credentials/citations), and media
/// reservations, plus per-message framing. PDF documents are priced from the
/// cost cached when the document was loaded or persisted; estimation never
/// decodes or scans the bytes.
pub(crate) fn estimate_message_tokens(msg: &ChatMsg) -> u64 {
    let mut bytes = 0usize;
    if msg.stopped {
        bytes += pig_provider::STOPPED_TOOL_OUTPUT.len();
    } else {
        bytes += msg.content.as_deref().unwrap_or("").len();
    }
    bytes += msg.reasoning.as_deref().unwrap_or("").len();
    if let Some(calls) = &msg.tool_calls {
        for call in calls {
            bytes += call.function.arguments.len() + TOOL_CALL_OVERHEAD;
        }
    }
    if let Some(replay) = &msg.replay {
        bytes += replay_estimate_bytes(msg, replay);
    }
    bytes as u64 / BYTES_PER_TOKEN
        + MESSAGE_OVERHEAD
        + msg.images.iter().map(estimate_image_tokens).sum::<u64>()
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

/// UTF-8 bytes contributed by native replay beyond the plaintext fields already
/// counted on the message. Text content and reasoning are counted once from
/// `content`/`reasoning`; tool calls matched by id are counted once from
/// `tool_calls`. Only opaque metadata (citations, encrypted/reasoning
/// credentials, hosted-search items) is added here.
fn replay_estimate_bytes(msg: &ChatMsg, replay: &pig_provider::AssistantReplay) -> usize {
    use pig_provider::AssistantReplayItem;
    let mut bytes = 0usize;
    for item in &replay.items {
        match item {
            // Text is already counted from the message content; only citation
            // metadata (including encrypted indexes) adds to the budget.
            AssistantReplayItem::Text { citations, .. } => {
                if !citations.is_empty() {
                    bytes += serde_json::to_string(citations)
                        .map(|json| json.len())
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
                let mut item_bytes = serde_json::to_string(item)
                    .map(|json| json.len())
                    .unwrap_or(0);
                // The streamed reasoning text is recorded in `reasoning`; do not
                // charge the same plaintext twice, but keep encrypted fields.
                let reasoning = msg.reasoning.as_deref().unwrap_or("");
                let plaintext = summary
                    .iter()
                    .chain(content.iter())
                    .map(String::len)
                    .sum::<usize>();
                if !reasoning.is_empty() && plaintext > 0 {
                    let opaque = encrypted_content.as_ref().map(String::len).unwrap_or(0);
                    item_bytes = item_bytes
                        .saturating_sub(reasoning.len().min(plaintext))
                        .max(opaque);
                }
                let _ = id;
                bytes += item_bytes;
            }
            AssistantReplayItem::AnthropicThinking { thinking, .. } => {
                let item_bytes = serde_json::to_string(item)
                    .map(|json| json.len())
                    .unwrap_or(0);
                let already = if msg.reasoning.as_deref() == Some(thinking.as_str()) {
                    thinking.len()
                } else {
                    0
                };
                bytes += item_bytes.saturating_sub(already);
            }
            other => {
                bytes += serde_json::to_string(other)
                    .map(|json| json.len())
                    .unwrap_or(0);
            }
        }
    }
    bytes
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
    prompt.len() as u64 / BYTES_PER_TOKEN + MESSAGE_OVERHEAD
}

/// Estimated tokens of the request's tool schemas (their JSON serialization).
pub(crate) fn estimate_schemas_tokens(schemas: &[serde_json::Value]) -> u64 {
    if schemas.is_empty() {
        return 0;
    }
    let bytes = serde_json::to_string(schemas)
        .map(|json| json.len())
        .unwrap_or(0);
    bytes as u64 / BYTES_PER_TOKEN + MESSAGE_OVERHEAD
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

    #[test]
    fn cjk_text_is_priced_by_utf8_bytes_not_char_count() {
        // 400 Chinese characters = 1,200 UTF-8 bytes -> 300 tokens + 4 framing
        // (vs only 100 + 4 under naive chars/4).
        let cjk = "中".repeat(400);
        let msg = ChatMsg::user(cjk);
        assert_eq!(estimate_message_tokens(&msg), 300 + 4);
        assert_eq!(estimate_system_tokens(&"汉".repeat(40)), 30 + 4);
    }

    #[test]
    fn decodable_images_are_priced_by_pixel_dimensions() {
        fn valid_png_base64(width: u32, height: u32) -> String {
            let mut cursor = std::io::Cursor::new(Vec::new());
            image::DynamicImage::ImageRgb8(image::RgbImage::new(width, height))
                .write_to(&mut cursor, image::ImageFormat::Png)
                .expect("encode png");
            pig_utils::image::base64_encode(&cursor.into_inner())
        }

        assert_eq!(image_tokens_for_dimensions(128, 128), MIN_IMAGE_TOKENS);
        assert_eq!(image_tokens_for_dimensions(1000, 1000), 1_333);
        let hd = image_tokens_for_dimensions(1920, 1080);
        assert!(hd > 1_500 && hd < 2_200, "1920x1080 scaled tokens: {hd}");

        let mut small = ChatMsg::tool_result("c1", "ok".into());
        let plain = estimate_message_tokens(&small);
        small.images = vec![ChatImage {
            media_type: "image/png".into(),
            data_base64: valid_png_base64(128, 128),
            label: None,
        }];
        let mut large = ChatMsg::tool_result("c1", "ok".into());
        large.images = vec![ChatImage {
            media_type: "image/png".into(),
            data_base64: valid_png_base64(1920, 1080),
            label: None,
        }];
        assert_eq!(estimate_message_tokens(&small), plain + MIN_IMAGE_TOKENS);
        assert_eq!(estimate_message_tokens(&large), plain + hd);
        assert!(estimate_message_tokens(&large) > estimate_message_tokens(&small));
    }
}
