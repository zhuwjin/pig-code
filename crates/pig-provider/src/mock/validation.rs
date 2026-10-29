//! Negative-oracle validation of the Anthropic request shape.

/// Test-support validation of an Anthropic request body. The mock rejects a
/// body that violates the invariants the encoder must guarantee, so a test
/// cannot accidentally bless an invalid shape just because the mock accepted
/// it. Only wire-shape rules we can prove are enforced: no empty text blocks,
/// no empty content arrays, and each document inside the tool_result that owns
/// it (never a sibling between parallel results).
pub fn validate_anthropic_request(body: &serde_json::Value) -> Result<(), String> {
    let messages = body
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "messages must be an array".to_string())?;
    for (message_index, message) in messages.iter().enumerate() {
        let role = message["role"].as_str().unwrap_or_default();
        let content = &message["content"];
        match content {
            serde_json::Value::String(_) => {}
            serde_json::Value::Array(blocks) => {
                if blocks.is_empty() {
                    return Err(format!(
                        "message {message_index} has an empty content array"
                    ));
                }
                let has_tool_result = blocks.iter().any(|block| block["type"] == "tool_result");
                for block in blocks {
                    validate_anthropic_block(block, role, has_tool_result, message_index)?;
                }
            }
            _ => {
                return Err(format!(
                    "message {message_index} content must be a string or block array"
                ));
            }
        }
    }
    Ok(())
}

pub(super) fn validate_anthropic_block(
    block: &serde_json::Value,
    role: &str,
    has_tool_result_sibling: bool,
    message_index: usize,
) -> Result<(), String> {
    match block["type"].as_str() {
        Some("text") => match block["text"].as_str() {
            Some(text) if !text.is_empty() => Ok(()),
            _ => Err(format!(
                "message {message_index} contains an empty text block"
            )),
        },
        Some("image") => Ok(()),
        Some("document") if has_tool_result_sibling => Err(format!(
            "message {message_index} places a document outside its owning tool_result"
        )),
        Some("document") if role == "assistant" => Err(format!(
            "message {message_index} places a document in an assistant turn"
        )),
        Some("document") => Ok(()),
        Some("tool_result") => {
            if role != "user" {
                return Err(format!(
                    "message {message_index} places tool_result in a {role} turn"
                ));
            }
            match &block["content"] {
                serde_json::Value::String(_) => Ok(()),
                serde_json::Value::Array(blocks) if !blocks.is_empty() => {
                    for inner in blocks {
                        match inner["type"].as_str() {
                            Some("text") => match inner["text"].as_str() {
                                Some(text) if !text.is_empty() => {}
                                _ => {
                                    return Err(format!(
                                        "message {message_index} tool_result contains an empty text block"
                                    ));
                                }
                            },
                            Some("tool_result") => {
                                return Err(format!(
                                    "message {message_index} nests a tool_result inside tool_result content"
                                ));
                            }
                            // Future media/content blocks are not classified here.
                            _ => {}
                        }
                    }
                    Ok(())
                }
                serde_json::Value::Array(_) => Err(format!(
                    "message {message_index} tool_result has an empty content array"
                )),
                _ => Err(format!(
                    "message {message_index} tool_result content must be a string or block array"
                )),
            }
        }
        Some("tool_use") => {
            if role != "assistant" {
                return Err(format!(
                    "message {message_index} places tool_use in a {role} turn"
                ));
            }
            Ok(())
        }
        // thinking/redacted_thinking/server_tool_use/web_search_tool_result and
        // any future native block are replayed verbatim; only the shape rules
        // above are enforced here.
        _ => Ok(()),
    }
}

#[cfg(test)]
mod request_shape_tests {
    use super::validate_anthropic_request;

    fn document(name: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "document",
            "source": {"type": "base64", "media_type": "application/pdf", "data": "JVBERi0="},
            "title": name,
        })
    }

    #[test]
    fn accepts_multiple_tool_results_with_nested_documents() {
        let body = serde_json::json!({
            "messages": [
                {"role": "user", "content": "read two"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "call-1", "name": "ReadMediaFile", "input": {}},
                    {"type": "tool_use", "id": "call-2", "name": "ReadMediaFile", "input": {}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call-1", "content": [
                        document("first.pdf"),
                        {"type": "text", "text": "ok"},
                    ]},
                    {"type": "tool_result", "tool_use_id": "call-2", "content": [
                        document("second.pdf"),
                        {"type": "text", "text": "ok"},
                    ]},
                ]},
            ],
        });
        validate_anthropic_request(&body).expect("nested documents are valid");
    }

    #[test]
    fn rejects_empty_text_and_media_only_empty_blocks() {
        let empty_text = serde_json::json!({
            "messages": [{"role": "user", "content": [
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "QUJD"}},
                {"type": "text", "text": ""},
            ]}],
        });
        assert!(
            validate_anthropic_request(&empty_text)
                .expect_err("empty text block")
                .contains("empty text block")
        );

        let empty_result_text = serde_json::json!({
            "messages": [{"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "call-1", "content": [
                    {"type": "text", "text": ""},
                ]},
            ]}],
        });
        assert!(
            validate_anthropic_request(&empty_result_text)
                .expect_err("empty tool-result text")
                .contains("empty text block")
        );

        let empty_content = serde_json::json!({
            "messages": [{"role": "assistant", "content": []}],
        });
        assert!(validate_anthropic_request(&empty_content).is_err());

        let empty_result_content = serde_json::json!({
            "messages": [{"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "call-1", "content": []},
            ]}],
        });
        assert!(
            validate_anthropic_request(&empty_result_content)
                .expect_err("empty tool-result content")
                .contains("empty content array")
        );
    }

    #[test]
    fn rejects_documents_between_parallel_results() {
        let sibling = serde_json::json!({
            "messages": [{"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "call-1", "content": "first"},
                document("first.pdf"),
                {"type": "tool_result", "tool_use_id": "call-2", "content": "second"},
            ]}],
        });
        assert!(
            validate_anthropic_request(&sibling)
                .expect_err("sibling document")
                .contains("outside its owning tool_result")
        );
    }
}
