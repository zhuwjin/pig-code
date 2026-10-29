//! Hook transcript snapshots (Claude's `transcript_path` convention): the
//! session's model-facing history rendered as JSONL — one
//! `{"type": role, "message": {...}}` line per message — so hook scripts can
//! inspect the conversation with plain JSON tools. System messages are
//! skipped (huge and useless to hooks), image payloads are stripped, each
//! message's text is capped, and the whole file is tail-biased under a total
//! cap (recent messages matter most).

use pig_provider::ChatMsg;
use serde_json::json;

/// Per-message text cap in chars (tool results can be enormous; hooks want
/// the conversation flow, not whole files)
const MESSAGE_CAP: usize = 20_000;
/// Total transcript cap in bytes (tail-biased when exceeded)
const TOTAL_CAP: usize = 512 * 1024;

pub(crate) fn transcript_lines(history: &[ChatMsg]) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut total = 0usize;
    for msg in history {
        if msg.role == "system" {
            continue;
        }
        let mut msg = msg.clone();
        // Never inline image bytes into a transcript
        msg.images = vec![];
        if let Some(content) = &mut msg.content
            && content.len() > MESSAGE_CAP
        {
            let mut end = MESSAGE_CAP;
            while !content.is_char_boundary(end) {
                end -= 1;
            }
            content.truncate(end);
            content.push_str("\n…[message truncated]");
        }
        let line = json!({ "type": msg.role, "message": msg }).to_string();
        total += line.len() + 1;
        lines.push(line);
    }
    if total > TOTAL_CAP {
        while total > TOTAL_CAP && lines.len() > 1 {
            total -= lines[0].len() + 1;
            lines.remove(0);
        }
        lines.insert(
            0,
            json!({ "type": "note", "note": "older messages omitted" }).to_string(),
        );
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_skips_system_strips_images_and_caps() {
        let history = vec![
            ChatMsg::system("the system prompt".into()),
            ChatMsg::user("hello".into()),
            ChatMsg::assistant("hi there".into(), vec![], None),
            ChatMsg::tool_result("c1", "x".repeat(MESSAGE_CAP + 100)),
        ];
        let mut with_image = ChatMsg::user("with image".into());
        with_image.images = vec![pig_provider::ChatImage {
            media_type: "image/png".into(),
            data_base64: "AAAA".repeat(1000),
            label: None,
        }];
        let mut history = history;
        history.push(with_image);

        let lines = transcript_lines(&history);
        assert_eq!(lines.len(), 4, "system skipped: {lines:?}");
        assert!(lines[0].contains("\"type\":\"user\""));
        assert!(lines[1].contains("\"type\":\"assistant\""));
        let tool: serde_json::Value = serde_json::from_str(&lines[2]).unwrap();
        let content = tool["message"]["content"].as_str().unwrap();
        assert!(content.len() < MESSAGE_CAP + 100);
        assert!(content.ends_with("[message truncated]"));
        // Image base64 never reaches the transcript
        let image_line = &lines[3];
        assert!(!image_line.contains("AAAA"));
        assert!(image_line.contains("with image"));
    }

    #[test]
    fn transcript_tail_biases_over_the_total_cap() {
        let big = "y".repeat(MESSAGE_CAP);
        let history: Vec<ChatMsg> = (0..40).map(|_| ChatMsg::user(big.clone())).collect();
        let lines = transcript_lines(&history);
        let total: usize = lines.iter().map(|l| l.len() + 1).sum();
        assert!(total <= TOTAL_CAP + MESSAGE_CAP + 100);
        assert!(lines[0].contains("older messages omitted"));
        assert!(lines.len() < 40);
    }
}
