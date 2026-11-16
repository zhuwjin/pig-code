use super::*;

#[test]
fn message_roundtrip_preserves_reasoning_calls_and_images() {
    let dir = std::env::temp_dir().join(format!("pig-agent-message-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test dir");
    let path = dir.join("a1.jsonl");
    write_meta(&path);
    let call = pig_provider::ToolCall {
        id: "call-image".into(),
        name: "ReadMediaFile".into(),
        arguments: "{\"path\":\"pic.png\"}".into(),
    };
    let mut assistant = pig_provider::ChatMsg::assistant(
        "read the image".into(),
        vec![call.clone()],
        Some("plain reasoning".into()),
    );
    append_test_message(&path, &mut assistant).expect("append assistant");
    let payload = pig_utils::image::base64_encode(&png_bytes());
    let mut result = pig_provider::ChatMsg::tool_result_with_images(
        "call-image",
        "image read".into(),
        vec![pig_provider::ChatImage {
            media_type: "image/png".into(),
            data_base64: payload.clone(),
            label: Some("pic.png".into()),
        }],
    );
    append_test_tool_result(&path, &call, &mut result).expect("append tool result");

    let (_, history) = read_agent(&path).expect("read agent");
    assert_eq!(history[0].reasoning.as_deref(), Some("plain reasoning"));
    assert_eq!(history[0].tool_calls.as_ref().expect("call").len(), 1);
    assert_eq!(history[1].images.len(), 1);
    assert_eq!(history[1].images[0].data_base64, payload);
    assert_eq!(history[1].images[0].label.as_deref(), Some("pic.png"));
    assert!(path.with_extension("media").join("1.png").is_file());
    let raw = std::fs::read_to_string(&path).expect("read raw jsonl");
    assert!(!raw.contains(&payload), "JSONL must not contain base64");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn document_roundtrip_keeps_refs_out_of_jsonl() {
    let dir = std::env::temp_dir().join(format!("pig-agent-doc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test dir");
    let path = dir.join("a1.jsonl");
    write_meta(&path);
    let bytes = b"%PDF-1.4\n%%EOF\n";
    let payload = pig_utils::image::base64_encode(bytes);
    let call = pig_provider::ToolCall {
        id: "call-doc".into(),
        name: "ReadMediaFile".into(),
        arguments: "{}".into(),
    };
    let mut assistant = pig_provider::ChatMsg::assistant(String::new(), vec![call.clone()], None);
    append_test_message(&path, &mut assistant).expect("append assistant");
    let mut result = pig_provider::ChatMsg::tool_result("call-doc", "Read PDF doc.pdf".into());
    result.documents = vec![pig_provider::ChatDocument {
        media_type: "application/pdf".into(),
        data_base64: payload.clone(),
        filename: "doc.pdf".into(),
        label: Some("doc.pdf".into()),
        token_estimate: None,
    }];
    append_test_tool_result(&path, &call, &mut result).expect("append tool result");

    let (_, history) = read_agent(&path).expect("read agent");
    assert_eq!(history[1].documents.len(), 1);
    assert_eq!(history[1].documents[0].data_base64, payload);
    assert_eq!(history[1].documents[0].filename, "doc.pdf");
    assert_eq!(
        history[1].documents[0].token_estimate,
        Some(pig_provider::DocumentTokenEstimate::window_fallback(
            pig_utils::pdf::unknown_document_token_reserve(TEST_CONTEXT_WINDOW),
            TEST_CONTEXT_WINDOW
        )),
        "the persisted token cost is reused instead of rescanning on read"
    );
    assert!(path.with_extension("media").join("1.pdf").is_file());
    let raw = std::fs::read_to_string(&path).expect("read raw jsonl");
    assert!(
        !raw.contains(&payload) && !raw.contains("data_base64"),
        "JSONL must not contain document base64: {raw}"
    );
    assert!(raw.contains("\"documents\""));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn image_persistence_failure_still_writes_the_tool_result() {
    let dir = std::env::temp_dir().join(format!("pig-agent-media-error-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test dir");
    let path = dir.join("a1.jsonl");
    write_meta(&path);
    let call = pig_provider::ToolCall {
        id: "call-image".into(),
        name: "ReadMediaFile".into(),
        arguments: "{}".into(),
    };
    let mut assistant = pig_provider::ChatMsg::assistant(String::new(), vec![call.clone()], None);
    append_test_message(&path, &mut assistant).expect("append assistant");
    std::fs::write(path.with_extension("media"), b"not a directory")
        .expect("force media-dir failure");
    let mut result = pig_provider::ChatMsg::tool_result_with_images(
        "call-image",
        "text survives".into(),
        vec![pig_provider::ChatImage {
            media_type: "image/png".into(),
            data_base64: pig_utils::image::base64_encode(b"image"),
            label: Some("pic.png".into()),
        }],
    );
    append_test_tool_result(&path, &call, &mut result).expect("tool result degrades to text");
    assert!(result.images.is_empty(), "the live message was degraded");
    assert!(
        result
            .content
            .as_deref()
            .is_some_and(|text| text.contains("1 image(s) could not be saved")),
        "{:?}",
        result.content
    );
    let (_, history) = read_agent(&path).expect("read degraded context");
    assert_eq!(history.len(), 2);
    assert_eq!(history[1].content, result.content);
    assert!(history[1].images.is_empty());
    assert_eq!(
        history[0].tool_calls.as_ref().expect("paired call").len(),
        1
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A media write failure must degrade the live message before it is
/// pushed/recorded, so live history and the JSONL line stay identical.
#[test]
fn document_persistence_failure_degrades_live_and_jsonl() {
    let dir = std::env::temp_dir().join(format!("pig-agent-doc-error-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test dir");
    let path = dir.join("a1.jsonl");
    write_meta(&path);
    std::fs::write(path.with_extension("media"), b"not a directory")
        .expect("force media-dir failure");
    let call = pig_provider::ToolCall {
        id: "call-doc".into(),
        name: "ReadMediaFile".into(),
        arguments: "{}".into(),
    };
    let mut assistant = ChatMsg::assistant(String::new(), vec![call.clone()], None);
    append_test_message(&path, &mut assistant).expect("append assistant");
    let mut result = ChatMsg::tool_result("call-doc", "Read PDF doc.pdf".into());
    result.documents = vec![pdf_document("doc.pdf")];
    append_test_tool_result(&path, &call, &mut result).expect("tool result degrades to text");
    assert!(result.documents.is_empty(), "the live message was degraded");
    assert!(
        result
            .content
            .as_deref()
            .is_some_and(|text| text.contains("1 PDF document(s) could not be saved")),
        "{:?}",
        result.content
    );
    let (_, history) = read_agent(&path).expect("read degraded context");
    assert_eq!(history[1].content, result.content);
    assert!(history[1].documents.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn missing_or_corrupt_documents_append_the_replay_note() {
    let dir = std::env::temp_dir().join(format!("pig-agent-doc-bad-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test dir");
    let path = dir.join("a1.jsonl");
    write_meta(&path);
    let call = pig_provider::ToolCall {
        id: "call-doc".into(),
        name: "ReadMediaFile".into(),
        arguments: "{}".into(),
    };
    let mut assistant = ChatMsg::assistant(String::new(), vec![call.clone()], None);
    append_test_message(&path, &mut assistant).expect("append assistant");
    let mut result = ChatMsg::tool_result("call-doc", "Read PDF doc.pdf".into());
    result.documents = vec![pdf_document("doc.pdf")];
    append_test_tool_result(&path, &call, &mut result).expect("append document");
    let media = path.with_extension("media").join("1.pdf");
    assert!(media.is_file());

    std::fs::remove_file(&media).expect("remove media");
    let (_, missing) = read_agent(&path).expect("read missing document");
    assert!(missing[1].documents.is_empty());
    assert!(
        missing[1]
            .content
            .as_deref()
            .is_some_and(|text| text.contains("[1 PDF document(s) no longer available]")),
        "{:?}",
        missing[1].content
    );

    std::fs::write(&media, b"%PDF-1.4\ntruncated without eof").expect("corrupt media");
    let (_, corrupt) = read_agent(&path).expect("read corrupt document");
    assert!(corrupt[1].documents.is_empty());
    assert!(
        corrupt[1]
            .content
            .as_deref()
            .is_some_and(|text| text.contains("[1 PDF document(s) no longer available]")),
        "{:?}",
        corrupt[1].content
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resume_projection_clears_pdf_when_child_capability_is_off() {
    let mut disabled = vec![ChatMsg::tool_result("call-doc", "Read PDF doc.pdf".into())];
    disabled[0].documents = vec![pdf_document("doc.pdf")];
    assert_eq!(
        project_history_media(
            &mut disabled,
            crate::tool::MediaCapabilities::new(true, false)
        ),
        (0, 1)
    );
    assert!(disabled[0].documents.is_empty());
    assert!(
        disabled[0]
            .content
            .as_deref()
            .is_some_and(|text| text.contains("were not sent with this message")
                && text.contains("does not support PDF input")),
        "{:?}",
        disabled[0].content
    );

    let mut enabled = vec![ChatMsg::tool_result("call-doc", "Read PDF doc.pdf".into())];
    enabled[0].documents = vec![pdf_document("doc.pdf")];
    assert_eq!(
        project_history_media(
            &mut enabled,
            crate::tool::MediaCapabilities::new(true, true)
        ),
        (0, 0)
    );
    assert_eq!(enabled[0].documents.len(), 1);
}

#[test]
fn resume_projection_clears_images_and_pdf_together() {
    let mut message = ChatMsg::tool_result("call-media", "media".into());
    message.images = vec![pig_provider::ChatImage {
        media_type: "image/png".into(),
        data_base64: "aW1n".into(),
        label: None,
    }];
    message.documents = vec![pdf_document("doc.pdf")];
    let mut history = vec![message];
    assert_eq!(
        project_history_media(
            &mut history,
            crate::tool::MediaCapabilities::new(false, false)
        ),
        (1, 1)
    );
    assert!(history[0].images.is_empty());
    assert!(history[0].documents.is_empty());
    let content = history[0].content.as_deref().unwrap_or_default();
    assert!(content.contains("1 image(s) were not sent"), "{content}");
    assert!(
        content.contains("1 PDF document(s) were not sent"),
        "{content}"
    );
}

/// A resumed child reprices only window-derived fallbacks. The payloads below
/// would price differently if decoded, so this proves that a measured cost and
/// an existing fallback are never re-scanned; only a document without any
/// cache is priced once from its bytes.
#[test]
fn resumed_document_costs_reprice_fallbacks_without_rescanning() {
    let pages = |count: u32| {
        let body = (1..=count)
            .map(|page| format!("{page} 0 obj << /Type /Page >> endobj\n"))
            .collect::<String>();
        pig_utils::image::base64_encode(
            format!("%PDF-1.4\n1 0 obj << /Type /Pages /Count {count} >> endobj\n{body}%%EOF\n")
                .as_bytes(),
        )
    };
    let document = |estimate, label: &str| pig_provider::ChatDocument {
        media_type: "application/pdf".into(),
        data_base64: pages(3),
        filename: label.into(),
        label: None,
        token_estimate: estimate,
    };
    let mut message = ChatMsg::user("documents".into());
    message.documents = vec![
        document(
            Some(pig_provider::DocumentTokenEstimate::measured(6_000)),
            "measured.pdf",
        ),
        document(
            Some(pig_provider::DocumentTokenEstimate::window_fallback(
                32_000, 128_000,
            )),
            "fallback.pdf",
        ),
        pig_provider::ChatDocument {
            data_base64: pages(2),
            ..document(None, "legacy.pdf")
        },
    ];
    let mut history = vec![message];

    fill_document_token_estimates(&mut history, 8_000);

    assert_eq!(
        history[0].documents[0].token_estimate,
        Some(pig_provider::DocumentTokenEstimate::measured(6_000)),
        "a measured page-tree cost is window-independent and not re-scanned"
    );
    assert_eq!(
        history[0].documents[1].token_estimate,
        Some(pig_provider::DocumentTokenEstimate::window_fallback(
            pig_utils::pdf::unknown_document_token_reserve(8_000),
            8_000
        )),
        "a window fallback is repriced from its tag, not re-measured"
    );
    assert_eq!(
        history[0].documents[2].token_estimate,
        Some(pig_provider::DocumentTokenEstimate::measured(
            2 * pig_utils::pdf::PDF_TOKENS_PER_PAGE
        )),
        "a document without a cache is scanned exactly once"
    );
}
