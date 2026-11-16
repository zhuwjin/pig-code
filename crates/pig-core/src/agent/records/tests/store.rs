use super::*;

#[test]
fn conversation_store_loads_and_syncs_child_records() {
    let dir = std::env::temp_dir().join(format!(
        "pig-agent-conversation-store-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test dir");
    let path = dir.join("a1.jsonl");
    write_meta(&path);
    let mut store = AgentConversationStore::new(&path);
    let records = crate::rollout::ConversationStore::load(&store).expect("load raw records");
    assert!(matches!(
        records.as_slice(),
        [crate::rollout::RolloutRecord::Meta { .. }]
    ));
    crate::rollout::ConversationStore::sync(&mut store).expect("sync child store");
    assert_eq!(
        crate::rollout::ConversationStore::media_root(&store),
        path.with_extension("media")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The read-only subagent view carries the real tool status instead of
/// pretending every row succeeded.
#[test]
fn display_items_keep_tool_error_status() {
    let call = pig_provider::ToolCall {
        id: "call-err".into(),
        name: "Read".into(),
        arguments: "{}".into(),
    };
    let msgs = vec![
        pig_provider::ChatMsg::user("do it".into()),
        pig_provider::ChatMsg::assistant(String::new(), vec![call.clone()], None),
        pig_provider::ChatMsg::tool_result("call-err", "failed".into()).with_error(true),
    ];
    let items = project_display_items(&msgs);
    let (output, status) = items
        .iter()
        .find_map(|item| match item {
            pig_protocol::ConversationItem::Tool { output, status, .. } => Some((output, status)),
            _ => None,
        })
        .expect("tool row");
    assert_eq!(output, "failed");
    assert_eq!(*status, pig_protocol::ConversationToolStatus::Error);
}

/// The subagent panel renders reasoning before the assistant body, like the
/// main conversation.
#[test]
fn display_items_keep_reasoning_before_the_assistant_body() {
    let assistant =
        pig_provider::ChatMsg::assistant("answer".into(), vec![], Some("thinking".into()));
    let msgs = vec![pig_provider::ChatMsg::user("do it".into()), assistant];
    let items = project_display_items(&msgs);
    let kinds: Vec<&str> = items
        .iter()
        .map(|item| match item {
            pig_protocol::ConversationItem::User { .. } => "user",
            pig_protocol::ConversationItem::Reasoning { .. } => "reasoning",
            pig_protocol::ConversationItem::Markdown { .. } => "assistant",
            _ => "other",
        })
        .collect();
    assert_eq!(kinds, vec!["user", "reasoning", "assistant"]);
    assert!(
        matches!(&items[1], pig_protocol::ConversationItem::Reasoning { text, .. } if text == "thinking")
    );
    assert!(
        matches!(&items[2], pig_protocol::ConversationItem::Markdown { text, .. } if text == "answer")
    );
}

#[test]
fn display_items_keep_recorded_timing_and_tool_edit() {
    let dir = std::env::temp_dir().join(format!(
        "pig-agent-display-meta-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test dir");
    let path = dir.join("a1.jsonl");
    write_meta(&path);
    let call = pig_provider::ToolCall {
        id: "call-edit".into(),
        name: "Edit".into(),
        arguments: "{}".into(),
    };
    let mut assistant = pig_provider::ChatMsg::assistant(
        "answer".into(),
        vec![call.clone()],
        Some("thinking".into()),
    );
    append_agent_message_meta(
        &path,
        &mut assistant,
        TEST_CONTEXT_WINDOW,
        AgentMsgMeta {
            reasoning_ms: Some(1234),
            edit: None,
        },
    )
    .expect("append assistant");
    let edit = pig_protocol::EditDiff {
        path: "a.ts".into(),
        unified_diff: "@@".into(),
        additions: 1,
        deletions: 0,
    };
    let mut tool = pig_provider::ChatMsg::tool_result("call-edit", "edited".into());
    append_agent_tool_result(
        &path,
        &call,
        &mut tool,
        TEST_CONTEXT_WINDOW,
        AgentMsgMeta {
            reasoning_ms: None,
            edit: Some(edit.clone()),
        },
    )
    .expect("append tool");
    let (meta, stored) = read_agent_stored(&path).expect("read stored records");
    let (_, _, items) = display_stored_items(&meta, &stored);
    let reasoning = items
        .iter()
        .find_map(|item| match item {
            pig_protocol::ConversationItem::Reasoning {
                text, duration_ms, ..
            } => Some((text, duration_ms)),
            _ => None,
        })
        .expect("reasoning row");
    assert_eq!(reasoning.0, "thinking");
    assert_eq!(*reasoning.1, Some(1234));
    let tool_edit = items
        .iter()
        .find_map(|item| match item {
            pig_protocol::ConversationItem::Tool { edit, .. } => Some(edit.clone()),
            _ => None,
        })
        .expect("tool row");
    assert_eq!(tool_edit, Some(edit));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn stopped_tool_status_survives_jsonl_and_display() {
    let dir = std::env::temp_dir().join(format!("pig-agent-stopped-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test dir");
    let path = dir.join("a1.jsonl");
    write_meta(&path);
    let call = pig_provider::ToolCall {
        id: "call-stop".into(),
        name: "ExecCommand".into(),
        arguments: "{}".into(),
    };
    let mut assistant = pig_provider::ChatMsg::assistant(String::new(), vec![call.clone()], None);
    append_test_message(&path, &mut assistant).expect("append assistant");
    let mut stopped =
        pig_provider::ChatMsg::tool_result("call-stop", "Stopped".into()).with_error(true);
    stopped.stopped = true;
    append_test_tool_result(&path, &call, &mut stopped).expect("append stopped result");

    let (_, history) = read_agent(&path).expect("read stopped result");
    assert!(history[1].is_error);
    assert!(history[1].stopped);
    let items = project_display_items(&history);
    let status = items
        .iter()
        .find_map(|item| match item {
            pig_protocol::ConversationItem::Tool { status, .. } => Some(*status),
            _ => None,
        })
        .expect("tool row");
    assert_eq!(status, pig_protocol::ConversationToolStatus::Stopped);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn canonical_assistant_without_reasoning_or_image_refs_stays_readable() {
    let dir = std::env::temp_dir().join(format!("pig-agent-legacy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test dir");
    let path = dir.join("a1.jsonl");
    write_meta(&path);
    let mut msg = ChatMsg::assistant("plain".into(), Vec::new(), None);
    append_test_message(&path, &mut msg).expect("append assistant");
    let (_, history) = read_agent(&path).expect("read assistant");
    assert_eq!(history[0].content.as_deref(), Some("plain"));
    assert!(history[0].reasoning.is_none());
    assert!(history[0].images.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn append_repairs_a_torn_final_line() {
    let dir = std::env::temp_dir().join(format!("pig-agent-append-repair-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test dir");
    let path = dir.join("a1.jsonl");
    write_meta(&path);
    for mut msg in [
        ChatMsg::system("system".into()),
        ChatMsg::user("task".into()),
        ChatMsg::assistant("answer".into(), vec![], None),
    ] {
        append_test_message(&path, &mut msg).expect("seed");
    }
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open context");
    file.write_all(br#"{"type":"msg","msg":{"role":"torn_marker"#)
        .expect("torn tail");
    drop(file);

    let mut next = ChatMsg::user("next task".into());
    append_test_message(&path, &mut next).expect("append after torn tail");
    let raw = std::fs::read_to_string(&path).expect("raw context");
    assert!(
        !raw.contains("torn_marker"),
        "the torn partial record is gone: {raw}"
    );
    let (_, history) = read_agent(&path).expect("read repaired context");
    assert_eq!(
        history
            .iter()
            .map(|message| message.role.as_str())
            .collect::<Vec<_>>(),
        vec!["system", "user", "assistant", "user"]
    );
    assert_eq!(
        history.last().unwrap().content.as_deref(),
        Some("next task")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A valid final record without its trailing newline is preserved (the
/// separator is added) rather than truncated as if it were torn.
#[test]
fn append_preserves_a_valid_final_record_without_newline() {
    let dir = std::env::temp_dir().join(format!(
        "pig-agent-append-no-newline-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test dir");
    let path = dir.join("a1.jsonl");
    write_meta(&path);
    let mut system = ChatMsg::system("system".into());
    append_test_message(&path, &mut system).expect("seed system");
    let bytes = std::fs::read(&path).expect("read context");
    assert_eq!(bytes.last(), Some(&b'\n'));
    std::fs::write(&path, &bytes[..bytes.len() - 1]).expect("strip final newline");

    let mut user = ChatMsg::user("task".into());
    append_test_message(&path, &mut user).expect("append after complete final line");
    let (_, history) = read_agent(&path).expect("read context");
    assert_eq!(
        history
            .iter()
            .map(|message| message.role.as_str())
            .collect::<Vec<_>>(),
        vec!["system", "user"]
    );
    let raw = std::fs::read_to_string(&path).expect("raw context");
    assert!(raw.ends_with('\n'), "the repaired file ends with a newline");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A complete compact marker followed by a torn (later) message line still
/// installs the compaction: a crash after the marker must not roll the
/// child back to the pre-compaction context.
#[test]
fn a_torn_line_after_a_complete_compact_marker_keeps_the_compaction() {
    let dir = std::env::temp_dir().join(format!(
        "pig-agent-compact-torn-after-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test dir");
    let path = dir.join("a1.jsonl");
    write_meta(&path);
    for mut msg in [
        ChatMsg::system("system".into()),
        ChatMsg::user("task".into()),
        ChatMsg::assistant("omitted work".into(), vec![], None),
        ChatMsg::assistant("retained tail".into(), vec![], None),
    ] {
        append_test_message(&path, &mut msg).expect("seed");
    }
    append_agent_record(
        &path,
        &serde_json::json!({
            "type": "compact",
            "note": "persisted note",
            "omitted": 1,
            "automatic": true,
            "tail_len": 1,
            "preserved_user": 1,
        }),
    )
    .expect("complete compact marker");
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open context");
    file.write_all(br#"{"type":"msg","msg":{"role":"assistant""#)
        .expect("write torn message");
    drop(file);

    let (_, history) = read_agent(&path).expect("torn later line is tolerated");
    assert_eq!(
        history
            .iter()
            .map(|message| message.role.as_str())
            .collect::<Vec<_>>(),
        vec!["system", "system", "user", "assistant"]
    );
    assert_eq!(history[1].content.as_deref(), Some("persisted note"));
    assert_eq!(history[2].content.as_deref(), Some("task"));
    assert_eq!(history[3].content.as_deref(), Some("retained tail"));
    let (_, again) = read_agent(&path).expect("repeat read");
    assert_eq!(
        again
            .iter()
            .map(|message| message.content.as_deref())
            .collect::<Vec<_>>(),
        history
            .iter()
            .map(|message| message.content.as_deref())
            .collect::<Vec<_>>()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A crash during the append can leave a partial final line: replay keeps
/// the last valid prefix (including a complete compact marker) instead of
/// discarding the whole child context.
#[test]
fn a_torn_final_child_line_is_ignored_on_replay() {
    let dir = std::env::temp_dir().join(format!("pig-agent-compact-torn-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test dir");
    let path = dir.join("a1.jsonl");
    write_meta(&path);
    for mut msg in [
        ChatMsg::system("system".into()),
        ChatMsg::user("task".into()),
        ChatMsg::assistant("answer".into(), vec![], None),
    ] {
        append_test_message(&path, &mut msg).expect("seed");
    }
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open context");
    file.write_all(br#"{"type":"compact","note":"torn"#)
        .expect("write torn line");
    drop(file);

    let (_, history) = read_agent(&path).expect("torn final line is tolerated");
    assert_eq!(
        history
            .iter()
            .map(|message| message.role.as_str())
            .collect::<Vec<_>>(),
        vec!["system", "user", "assistant"]
    );
    // A complete malformed line before EOF (with its newline) remains a
    // hard error rather than silently losing later records.
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open context");
    file.write_all(b"{\"type\":\"msg\"}\n{\"type\":\"msg\"}\n")
        .expect("write malformed line");
    drop(file);
    let error = read_agent(&path).expect_err("complete malformed line must error");
    assert!(
        error.contains("line 5") || error.contains("line 6"),
        "the error names the malformed line: {error}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
