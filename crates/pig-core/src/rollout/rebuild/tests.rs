use super::*;

fn png_bytes() -> Vec<u8> {
    let image = image::DynamicImage::new_rgb8(8, 8);
    let mut buf = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut buf, image::ImageFormat::Png)
        .expect("encode png");
    buf.into_inner()
}

fn user(text: &str) -> RolloutRecord {
    RolloutRecord::User {
        sent_at_ms: 0,
        text: text.into(),
        files: vec![],
        images: vec![],
        sent_text: Some(text.into()),
        sent_images: vec![],
        model_text: None,
        sent_input_image: None,
    }
}

fn assistant(text: &str, tool_calls: Vec<ToolCall>) -> RolloutRecord {
    RolloutRecord::Assistant {
        text: text.into(),
        reasoning: Some("plain reasoning".into()),
        replay: None,
        tool_calls: tool_calls.iter().map(ToolCall::to_wire).collect(),
        tool_execution_ids: tool_calls
            .iter()
            .map(|call| format!("execution-{}", call.id))
            .collect(),
        ts_ms: 0,
        reasoning_ms: None,
    }
}

fn result(tool: &str, arguments: &str, output: &str) -> RolloutRecord {
    RolloutRecord::ToolCall {
        execution_id: "execution-test".into(),
        tool: tool.into(),
        summary: tool.into(),
        arguments: arguments.into(),
        output: output.into(),
        is_error: false,
        stopped: false,
        edit: None,
        agent_card: None,
        agent_cards: vec![],
        images: vec![],
        documents: vec![],
        delivered_notification_ids: vec![],
        recovery: None,
    }
}

fn compact(note: &str, omitted: usize) -> RolloutRecord {
    RolloutRecord::Compact {
        note: note.into(),
        omitted,
        automatic: false,
        used_after: None,
        used_before: None,
        summary: None,
        tail_len: Some(2),
        preserved_user: None,
        trigger: None,
    }
}

fn compact_with(
    note: &str,
    omitted: usize,
    tail_len: usize,
    preserved_user: Option<usize>,
) -> RolloutRecord {
    RolloutRecord::Compact {
        note: note.into(),
        omitted,
        automatic: false,
        used_after: None,
        used_before: None,
        summary: None,
        tail_len: Some(tail_len),
        preserved_user,
        trigger: None,
    }
}

#[test]
fn canonical_compact_does_not_rehydrate_discarded_media() {
    let dir = std::env::temp_dir().join(format!("pig-discarded-media-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create media dir");
    std::fs::write(dir.join("1.png"), png_bytes()).expect("write image 1");
    std::fs::write(dir.join("2.png"), png_bytes()).expect("write image 2");
    let image = |name: &str| ImageRef {
        path: dir.join(name),
        media_type: "image/png".into(),
        width: 8,
        height: 8,
        label: None,
    };
    let image_user = |text: &str, name: &str| RolloutRecord::User {
        sent_at_ms: 0,
        text: text.into(),
        files: vec![],
        images: vec![image(name)],
        sent_text: None,
        sent_images: vec![],
        model_text: None,
        sent_input_image: None,
    };
    let records = vec![
        image_user("old", "1.png"),
        assistant("old answer", vec![]),
        image_user("new", "2.png"),
        compact_with("note", 2, 1, None),
    ];
    reset_media_reads();
    let (history, _) = rebuild_history_with_media_root(&records, "sys".into(), None, &dir, None);
    assert_eq!(history.len(), 3, "{history:#?}");
    assert_eq!(history[2].content.as_deref(), Some("new"));
    assert_eq!(history[2].images.len(), 1);
    assert_eq!(
        media_reads(),
        1,
        "the discarded first user's media is never read"
    );

    reset_media_reads();
    let (uncompacted, _) =
        rebuild_history_with_media_root(&records[..3], "sys".into(), None, &dir, None);
    assert_eq!(uncompacted.len(), 4);
    assert_eq!(media_reads(), 2, "without a compact both images load");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn canonical_assistant_preserves_parallel_calls_and_results() {
    let calls = vec![
        ToolCall {
            id: "call-a".into(),
            name: "Read".into(),
            arguments: "{\"path\":\"a.rs\"}".into(),
        },
        ToolCall {
            id: "call-b".into(),
            name: "Read".into(),
            arguments: "{\"path\":\"b.rs\"}".into(),
        },
    ];
    let records = vec![
        assistant("working", calls),
        result("Read", "{}", "a"),
        result("Read", "{}", "b"),
    ];
    let (history, _) = rebuild_history(&records, "sys".into());
    let calls = history[1].tool_calls.as_ref().expect("parallel calls");
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].id, "call-a");
    assert_eq!(calls[1].id, "call-b");
    assert_eq!(history[1].reasoning.as_deref(), Some("plain reasoning"));
    assert_eq!(history[2].tool_call_id.as_deref(), Some("call-a"));
    assert_eq!(history[3].tool_call_id.as_deref(), Some("call-b"));
}

#[test]
fn canonical_assistant_keeps_raw_arguments_not_enriched_tool_arguments() {
    let call = ToolCall {
        id: "plan-call".into(),
        name: "ExitPlanMode".into(),
        arguments: "{}".into(),
    };
    let records = vec![
        assistant("", vec![call]),
        result("ExitPlanMode", "{\"plan\":\"effective plan\"}", "approved"),
    ];
    let (history, _) = rebuild_history(&records, "sys".into());
    assert_eq!(
        history[1].tool_calls.as_ref().expect("plan call")[0]
            .function
            .arguments,
        "{}"
    );
    assert_eq!(history[2].content.as_deref(), Some("approved"));
}

#[test]
fn sent_user_text_is_not_recomputed_from_files() {
    let record = RolloutRecord::User {
        sent_at_ms: 0,
        text: "clean".into(),
        files: vec!["changed.rs".into()],
        images: vec![],
        sent_text: Some("reminder + original pointer + image note".into()),
        sent_images: vec![],
        model_text: None,
        sent_input_image: None,
    };
    let (history, _) = rebuild_history(&[record], "sys".into());
    assert_eq!(
        history[1].content.as_deref(),
        Some("reminder + original pointer + image note")
    );
}

#[test]
fn repeated_compaction_and_recall_keep_canonical_boundaries() {
    let compacted = vec![
        user("u1"),
        assistant("a1", vec![]),
        user("u2"),
        assistant("a2", vec![]),
        compact("note-1", 2),
        user("u3"),
        assistant("a3", vec![]),
        compact("note-2", 3),
    ];
    let (once, once_indices) = rebuild_history(&compacted[..5], "sys".into());
    assert_eq!(once.len(), 4, "first compact: {once:#?}");
    assert_eq!(once_indices, vec![None, Some(2)]);
    let (before_recall, indices) = rebuild_history(&compacted, "sys".into());
    assert_eq!(before_recall.len(), 4, "second compact: {before_recall:#?}");
    assert_eq!(before_recall[1].content.as_deref(), Some("note-2"));
    assert_eq!(before_recall[2].content.as_deref(), Some("u3"));
    assert_eq!(before_recall[3].content.as_deref(), Some("a3"));
    assert_eq!(indices, vec![None, None, Some(2)]);

    let mut records = compacted;
    records.extend([RolloutRecord::Recall, RolloutRecord::Recall]);
    let (history, user_ixs) = rebuild_history(&records, "sys".into());
    assert_eq!(history.len(), 2);
    assert_eq!(history[1].content.as_deref(), Some("note-2"));
    assert_eq!(user_ixs, vec![None]);
}

#[test]
fn latest_prompt_snapshot_controls_the_rebuilt_system() {
    let snapshot = |text: &str| crate::rollout::PromptSnapshot {
        text: text.into(),
        git_snapshot: None,
        agents_prompt: String::new(),
        skills_prompt: String::new(),
        skills_state: String::new(),
        memory_prompt: String::new(),
        memory_enabled: false,
        profiles_summary: String::new(),
        date_frozen: "2026-01-01".into(),
        reminders: crate::rollout::PromptReminderState {
            date: "2026-01-01".into(),
            agents: String::new(),
            memory: String::new(),
            permissions: None,
        },
    };
    let records = vec![
        RolloutRecord::PromptSnapshot {
            snapshot: snapshot("old system"),
        },
        RolloutRecord::PromptSnapshot {
            snapshot: snapshot("latest system"),
        },
    ];
    let (history, _) = rebuild_history(&records, "fallback".into());
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].content.as_deref(), Some("latest system"));
}

#[test]
fn empty_canonical_assistant_is_not_dropped() {
    let records = vec![RolloutRecord::Assistant {
        text: String::new(),
        reasoning: None,
        replay: None,
        tool_calls: vec![],
        tool_execution_ids: vec![],
        ts_ms: 0,
        reasoning_ms: None,
    }];
    let (history, _) = rebuild_history(&records, "sys".into());
    assert_eq!(history.len(), 2);
    assert_eq!(history[1].role, "assistant");
    assert!(history[1].content.is_none());
}

#[test]
fn legacy_compact_and_canonical_records_can_be_mixed() {
    let legacy_user = RolloutRecord::User {
        sent_at_ms: 0,
        text: "legacy user".into(),
        files: vec![],
        images: vec![],
        sent_text: None,
        sent_images: vec![],
        model_text: None,
        sent_input_image: None,
    };
    let legacy_compact = RolloutRecord::Compact {
        note: "legacy note".into(),
        omitted: 1,
        automatic: false,
        used_after: None,
        used_before: None,
        summary: None,
        tail_len: None,
        preserved_user: None,
        trigger: None,
    };
    let records = vec![
        legacy_user,
        RolloutRecord::Reasoning {
            text: "legacy reasoning".into(),
            replay: None,
        },
        RolloutRecord::Text {
            text: "legacy answer".into(),
        },
        legacy_compact,
        assistant("canonical answer", vec![]),
    ];
    let (history, _) = rebuild_history(&records, "sys".into());
    assert_eq!(history.len(), 5);
    assert_eq!(history[3].role, "system");
    assert_eq!(history[3].content.as_deref(), Some("legacy note"));
    assert_eq!(history[4].content.as_deref(), Some("canonical answer"));
}

#[test]
fn canonical_crash_tail_drops_unanswered_calls_and_native_replay_items() {
    let calls = [
        ToolCall {
            id: "call-a".into(),
            name: "Read".into(),
            arguments: "{}".into(),
        },
        ToolCall {
            id: "call-b".into(),
            name: "Read".into(),
            arguments: "{}".into(),
        },
    ];
    let replay = pig_provider::AssistantReplay {
        origin: pig_provider::ReplayOrigin {
            api_format: pig_protocol::ApiFormat::OpenAiResponses,
            base_url: "http://localhost/v1".into(),
            model: None,
            credential: None,
        },
        items: calls
            .iter()
            .map(|call| pig_provider::AssistantReplayItem::ToolCall {
                call: call.to_wire(),
            })
            .collect(),
    };
    let records = vec![
        RolloutRecord::Assistant {
            text: String::new(),
            reasoning: None,
            replay: Some(replay),
            tool_calls: calls.iter().map(ToolCall::to_wire).collect(),
            tool_execution_ids: calls
                .iter()
                .map(|call| format!("execution-{}", call.id))
                .collect(),
            ts_ms: 0,
            reasoning_ms: None,
        },
        result("Read", "{}", "only first"),
    ];
    let (history, _) = rebuild_history(&records, "sys".into());
    assert_eq!(history.len(), 3);
    assert_eq!(history[1].tool_calls.as_ref().expect("answered").len(), 1);
    assert_eq!(history[2].tool_call_id.as_deref(), Some("call-a"));
    let replay = history[1].replay.as_ref().expect("native replay");
    assert_eq!(replay.items.len(), 1);
    assert!(matches!(
        &replay.items[0],
        pig_provider::AssistantReplayItem::ToolCall { call } if call.id == "call-a"
    ));
}

/// A tool result's execution status is part of the rebuilt context: an
/// Anthropic continuation must keep `tool_result.is_error` after restart.
#[test]
fn canonical_tool_error_marker_survives_rebuild() {
    let call = ToolCall {
        id: "call-err".into(),
        name: "Read".into(),
        arguments: "{}".into(),
    };
    let records = vec![
        assistant("", vec![call]),
        RolloutRecord::ToolCall {
            execution_id: "execution-call-err".into(),
            tool: "Read".into(),
            summary: "Read".into(),
            arguments: "{}".into(),
            output: "not found".into(),
            is_error: true,
            stopped: false,
            edit: None,
            agent_card: None,
            agent_cards: vec![],
            images: vec![],
            documents: vec![],
            delivered_notification_ids: vec![],
            recovery: None,
        },
    ];
    let (history, _) = rebuild_history(&records, "sys".into());
    assert_eq!(history[2].role, "tool");
    assert!(history[2].is_error, "rebuilt tool result keeps is_error");
}

#[test]
fn canonical_user_images_keep_original_labels_and_report_missing() {
    let dir = std::env::temp_dir().join(format!("pig-canonical-labels-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create media dir");
    std::fs::write(dir.join("2.png"), png_bytes()).expect("write image");
    let image = |name: &str, label: &str| ImageRef {
        path: dir.join(name),
        media_type: "image/png".into(),
        width: 1,
        height: 1,
        label: Some(label.into()),
    };
    let records = vec![RolloutRecord::User {
        sent_at_ms: 0,
        text: "clean".into(),
        files: vec![],
        images: vec![],
        sent_text: Some("[Image 1 failed to compress]".into()),
        sent_images: vec![image("2.png", "Image 2"), image("3.png", "Image 3")],
        model_text: None,
        sent_input_image: None,
    }];
    let (history, _) = rebuild_history_with_media_root(&records, "sys".into(), None, &dir, None);
    assert_eq!(history[1].images.len(), 1);
    assert_eq!(history[1].images[0].label.as_deref(), Some("Image 2"));
    assert!(
        history[1]
            .content
            .as_deref()
            .is_some_and(|text| text.contains("[Image 1 failed to compress]")
                && text.contains("1 image(s) no longer available"))
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn capability_switch_reprojects_from_model_text_not_sent_projection() {
    let dir = std::env::temp_dir().join(format!("pig-capability-switch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create media dir");
    std::fs::write(dir.join("1.png"), png_bytes()).expect("write image");
    let image = ImageRef {
        path: dir.join("1.png"),
        media_type: "image/png".into(),
        width: 1,
        height: 1,
        label: Some("Image 1".into()),
    };
    let nonvision = RolloutRecord::User {
        sent_at_ms: 0,
        text: "clean".into(),
        files: vec![],
        images: vec![image.clone()],
        sent_text: Some("base\n[images were not sent]".into()),
        sent_images: vec![],
        model_text: Some("base".into()),
        sent_input_image: Some(false),
    };
    let (enabled, _) = rebuild_history_with_media_root(
        std::slice::from_ref(&nonvision),
        "sys".into(),
        Some(MediaCapabilities::new(true, false)),
        &dir,
        None,
    );
    assert_eq!(enabled[1].content.as_deref(), Some("base"));
    assert_eq!(enabled[1].images.len(), 1);
    let (frozen, _) = rebuild_history(std::slice::from_ref(&nonvision), "sys".into());
    assert_eq!(
        frozen[1].content.as_deref(),
        Some("base\n[images were not sent]")
    );
    assert!(frozen[1].images.is_empty());

    let mut vision = nonvision;
    let RolloutRecord::User {
        sent_text,
        sent_images,
        sent_input_image,
        ..
    } = &mut vision
    else {
        panic!("user record");
    };
    *sent_text = Some("base".into());
    *sent_images = vec![image];
    *sent_input_image = Some(true);
    let (disabled, _) = rebuild_history_with_media_root(
        &[vision],
        "sys".into(),
        Some(MediaCapabilities::new(false, false)),
        &dir,
        None,
    );
    assert!(disabled[1].images.is_empty());
    assert!(
        disabled[1]
            .content
            .as_deref()
            .is_some_and(|text| text.starts_with("base") && text.contains("were not sent"))
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The live split-turn shape rebuilds identically: frozen system + note +
/// the preserved user extracted from its rebuilt User record + the
/// assistant-starting tail with intact parallel call/result pairs.
#[test]
fn assistant_starting_tail_and_preserved_user_replay_like_live() {
    let calls = vec![
        ToolCall {
            id: "call-a".into(),
            name: "Read".into(),
            arguments: "{}".into(),
        },
        ToolCall {
            id: "call-b".into(),
            name: "Read".into(),
            arguments: "{}".into(),
        },
    ];
    let records = vec![
        user("u1"),
        assistant("a1", vec![]),
        user("u2"),
        assistant("", calls),
        result("Read", "{}", "a"),
        result("Read", "{}", "b"),
        assistant("a2", vec![]),
    ];
    // Pre-compact indices: 0 system, 1 u1, 2 a1, 3 u2, 4 call, 5 a, 6 b, 7 a2
    let (live, _) = rebuild_history(&records, "sys".into());
    assert_eq!(live.len(), 8);
    let mut records = records;
    records.push(compact_with("note", 2, 4, Some(3)));
    let (rebuilt, user_ixs) = rebuild_history(&records, "sys".into());
    assert_eq!(rebuilt.len(), 7, "{rebuilt:#?}");
    assert_eq!(rebuilt[0].content.as_deref(), Some("sys"));
    assert_eq!(rebuilt[1].content.as_deref(), Some("note"));
    assert_eq!(rebuilt[2].content.as_deref(), Some("u2"));
    assert_eq!(rebuilt[2].role, "user");
    assert_eq!(rebuilt[3].tool_calls.as_ref().expect("parallel").len(), 2);
    assert_eq!(rebuilt[4].tool_call_id.as_deref(), Some("call-a"));
    assert_eq!(rebuilt[5].tool_call_id.as_deref(), Some("call-b"));
    assert_eq!(rebuilt[5].content.as_deref(), Some("b"));
    assert_eq!(rebuilt[6].content.as_deref(), Some("a2"));
    assert_eq!(
        user_ixs,
        vec![None, None],
        "the preserved user is a tombstone, not a recallable record"
    );
}

/// New marked handoffs replay as user messages; a later cut may start at the
/// promoted checkpoint's next request instead of no-op'ing forever.
#[test]
fn marked_user_handoff_and_checkpoint_cut_replay_the_active_request() {
    let note = |omitted: usize| {
        format!(
            "{} [Earlier context compacted (automatic)] {omitted} messages omitted.",
            crate::rollout::COMPACTION_HANDOFF_MARKER
        )
    };
    let records = vec![
        user("previous request"),
        assistant("old answer", vec![]),
        compact_with(&note(1), 1, 0, Some(1)),
    ];
    let (history, user_ixs) = rebuild_history(&records, "sys".into());
    assert_eq!(history.len(), 3, "{history:#?}");
    assert_eq!(history[1].role, "user", "new handoffs are user-role");
    assert!(
        crate::rollout::is_compaction_handoff(&history[1]),
        "the marker identifies the handoff"
    );
    assert_eq!(user_ixs, vec![None]);

    let mut records = records;
    records.push(user("next request"));
    records.push(assistant("next answer", vec![]));
    records.push(compact_with(&note(2), 2, 2, None));
    let (history, user_ixs) = rebuild_history(&records, "sys".into());
    assert_eq!(
        history
            .iter()
            .map(|message| message.role.as_str())
            .collect::<Vec<_>>(),
        ["system", "user", "user", "assistant"],
        "{history:#?}"
    );
    assert_eq!(
        history[2].content.as_deref(),
        Some("next request"),
        "the active request survives the checkpoint cut"
    );
    assert_eq!(history[3].content.as_deref(), Some("next answer"));
    assert_eq!(
        user_ixs,
        vec![None, Some(2)],
        "the active request is remapped after the new note"
    );

    // Preserving the active request separately from inside the checkpoint
    // run is also canonical: the replay validates it as the active real user,
    // not merely as the first message of a user run.
    let mut records = vec![
        user("previous request"),
        assistant("old answer", vec![]),
        compact_with(&note(1), 1, 0, Some(1)),
    ];
    records.push(user("next request"));
    records.push(assistant("next answer", vec![]));
    records.push(compact_with(&note(2), 2, 1, Some(3)));
    let (history, user_ixs) = rebuild_history(&records, "sys".into());
    assert_eq!(
        history
            .iter()
            .map(|message| message.role.as_str())
            .collect::<Vec<_>>(),
        ["system", "user", "user", "assistant"],
        "{history:#?}"
    );
    assert_eq!(history[2].content.as_deref(), Some("next request"));
    assert_eq!(history[3].content.as_deref(), Some("next answer"));
    assert_eq!(
        user_ixs,
        vec![None, None],
        "the separately preserved request stays a tombstone, not a recallable record"
    );
}

/// Zero-length raw tails reconstruct the active user after the note.
#[test]
fn zero_tail_preserved_user_rebuilds_after_the_note() {
    let records = vec![
        user("u1"),
        assistant(
            "",
            vec![ToolCall {
                id: "c1".into(),
                name: "Read".into(),
                arguments: "{}".into(),
            }],
        ),
        result("Read", "{}", "out"),
        compact_with("note", 2, 0, Some(1)),
    ];
    let (history, user_ixs) = rebuild_history(&records, "sys".into());
    assert_eq!(history.len(), 3, "{history:#?}");
    assert_eq!(history[1].content.as_deref(), Some("note"));
    assert_eq!(history[2].role, "user");
    assert_eq!(history[2].content.as_deref(), Some("u1"));
    assert_eq!(user_ixs, vec![None]);
}

/// A user-starting tail after the active user (a later Stop-hook context
/// run) reconstructs the preserved request before that tail.
#[test]
fn preserved_user_before_a_later_user_tail_rebuilds_the_request() {
    let records = vec![
        user("real"),
        assistant("answer", vec![]),
        RolloutRecord::HookContext {
            text: "stop hook".into(),
        },
        assistant("follow-up", vec![]),
        compact_with("note", 1, 2, Some(1)),
    ];
    let (history, user_ixs) = rebuild_history(&records, "sys".into());
    assert_eq!(history.len(), 5, "{history:#?}");
    assert_eq!(history[1].content.as_deref(), Some("note"));
    assert_eq!(history[2].content.as_deref(), Some("real"));
    assert_eq!(history[3].content.as_deref(), Some("stop hook"));
    assert_eq!(history[4].content.as_deref(), Some("follow-up"));
    assert_eq!(user_ixs, vec![None]);
}

/// Corrupt indices never invent a different boundary: the record falls
/// back to appending the note above the full rebuilt history.
#[test]
fn invalid_preserved_user_indices_fall_back_conservatively() {
    let base = vec![
        user("u1"),
        assistant(
            "",
            vec![ToolCall {
                id: "c1".into(),
                name: "Read".into(),
                arguments: "{}".into(),
            }],
        ),
        result("Read", "{}", "out"),
        assistant("a1", vec![]),
    ];
    // 5-message pre-compact history; cut = 4 with omitted 2 and tail 1.
    for bad in [0usize, 2, 4, 99] {
        let mut records = base.clone();
        records.push(compact_with("note", 2, 1, Some(bad)));
        let (history, _) = rebuild_history(&records, "sys".into());
        assert_eq!(
            history.len(),
            6,
            "invalid index {bad} must not truncate: {history:#?}"
        );
        assert_eq!(history[5].role, "system");
        assert_eq!(history[5].content.as_deref(), Some("note"));
        assert_eq!(history[1].content.as_deref(), Some("u1"));
    }

    // A second message of a user run is a hook context, not the real user.
    let records = vec![
        user("real"),
        user("hook"),
        assistant("a", vec![]),
        compact_with("note", 1, 1, Some(2)),
    ];
    let (history, _) = rebuild_history(&records, "sys".into());
    assert_eq!(history.len(), 5, "{history:#?}");
    assert_eq!(history[4].role, "system");
    assert_eq!(history[2].content.as_deref(), Some("hook"));
}

/// A preserved index that references a *valid but earlier* user is corrupt
/// (the live active user was the last real User); it must not silently drop
/// the later request.
#[test]
fn preserved_user_must_be_the_active_real_user() {
    let records = vec![
        user("u1"),
        assistant("a1", vec![]),
        user("u2"),
        assistant("a2", vec![]),
        compact_with("note", 3, 0, Some(1)),
    ];
    let (history, user_ixs) = rebuild_history(&records, "sys".into());
    assert_eq!(
        history.len(),
        6,
        "the corrupt record must not truncate: {history:#?}"
    );
    assert_eq!(history[5].role, "system");
    assert_eq!(history[5].content.as_deref(), Some("note"));
    assert_eq!(history[1].content.as_deref(), Some("u1"));
    assert_eq!(history[3].content.as_deref(), Some("u2"));
    assert_eq!(user_ixs, vec![Some(1), Some(3)]);
}

/// A corrupt tail length whose cut lands on a tool result or mid-run user
/// must not orphan a call/result pair or split a user run.
#[test]
fn corrupt_tail_boundaries_fall_back_conservatively() {
    let tool_cut = vec![
        user("u1"),
        assistant(
            "",
            vec![ToolCall {
                id: "c1".into(),
                name: "Read".into(),
                arguments: "{}".into(),
            }],
        ),
        result("Read", "{}", "out"),
        assistant("a1", vec![]),
        compact_with("note", 2, 2, None),
    ];
    let (history, _) = rebuild_history(&tool_cut, "sys".into());
    assert_eq!(history.len(), 6, "no truncation: {history:#?}");
    assert_eq!(history[2].role, "assistant");
    assert_eq!(history[3].role, "tool");
    assert_eq!(history[3].tool_call_id.as_deref(), Some("c1"));
    assert_eq!(history[5].content.as_deref(), Some("note"));

    let mid_run_cut = vec![
        user("u1"),
        user("hook"),
        assistant("a1", vec![]),
        compact_with("note", 1, 2, None),
    ];
    let (history, _) = rebuild_history(&mid_run_cut, "sys".into());
    assert_eq!(history.len(), 5, "no truncation: {history:#?}");
    assert_eq!(history[1].content.as_deref(), Some("u1"));
    assert_eq!(history[2].content.as_deref(), Some("hook"));
    assert_eq!(history[4].content.as_deref(), Some("note"));

    // A completed compaction always omits at least one message; omitted=0
    // would install the note before an otherwise complete history.
    let no_omitted = vec![
        user("u1"),
        assistant("a1", vec![]),
        compact_with("note", 0, 1, Some(1)),
    ];
    let (history, _) = rebuild_history(&no_omitted, "sys".into());
    assert_eq!(history.len(), 4, "no rewrite: {history:#?}");
    assert_eq!(history[3].content.as_deref(), Some("note"));

    // An assistant-starting tail may not follow a bare system note: the next
    // request would have no user anchor.
    let assistant_after_a_note = vec![
        user("u1"),
        assistant("a1", vec![]),
        RolloutRecord::Compact {
            note: "legacy note".into(),
            omitted: 1,
            automatic: false,
            used_after: None,
            used_before: None,
            summary: None,
            tail_len: None,
            preserved_user: None,
            trigger: None,
        },
        assistant("a2", vec![]),
        compact_with("note-2", 3, 1, None),
    ];
    let (history, _) = rebuild_history(&assistant_after_a_note, "sys".into());
    assert_eq!(history.len(), 6, "no rewrite: {history:#?}");
    assert_eq!(history[3].role, "system");
    assert_eq!(history[4].role, "assistant");
    assert_eq!(history[5].content.as_deref(), Some("note-2"));
}

/// A compact boundary whose installed history would contain no user message
/// (an empty raw tail or an assistant-starting tail without a preserved
/// checkpoint) must fall back to the legacy note above the full history
/// instead of truncating every request.
#[test]
fn compact_without_a_user_anchor_falls_back_conservatively() {
    let empty_tail = vec![
        user("u1"),
        assistant("a1", vec![]),
        compact_with("note", 2, 0, None),
    ];
    let (history, _) = rebuild_history(&empty_tail, "sys".into());
    assert_eq!(history.len(), 4, "no truncation: {history:#?}");
    assert_eq!(history[1].content.as_deref(), Some("u1"));
    assert_eq!(history[2].content.as_deref(), Some("a1"));
    assert_eq!(history[3].content.as_deref(), Some("note"));

    let assistant_tail = vec![
        user("u1"),
        assistant(
            "",
            vec![ToolCall {
                id: "c1".into(),
                name: "Read".into(),
                arguments: "{}".into(),
            }],
        ),
        result("Read", "{}", "out"),
        assistant("a1", vec![]),
        compact_with("note", 3, 1, None),
    ];
    let (history, _) = rebuild_history(&assistant_tail, "sys".into());
    assert_eq!(history.len(), 6, "no truncation: {history:#?}");
    assert_eq!(history[1].content.as_deref(), Some("u1"));
    assert_eq!(history[2].role, "assistant");
    assert_eq!(history[3].tool_call_id.as_deref(), Some("c1"));
    assert_eq!(history[4].content.as_deref(), Some("a1"));
    assert_eq!(history[5].content.as_deref(), Some("note"));
}

/// A promoted checkpoint stays replayable after its tombstone was consumed by
/// a Recall (the recall stack may be empty even though the history kept the
/// user after the note).
#[test]
fn promoted_checkpoint_replays_after_its_tombstone_was_recalled() {
    let records = vec![
        user("u1"),
        assistant("a1", vec![]),
        compact_with("note-1", 1, 0, Some(1)),
        RolloutRecord::Recall,
        compact_with("note-2", 1, 0, Some(2)),
    ];
    let (history, user_ixs) = rebuild_history(&records, "sys".into());
    assert_eq!(history.len(), 3, "{history:#?}");
    assert_eq!(history[1].content.as_deref(), Some("note-2"));
    assert_eq!(history[2].content.as_deref(), Some("u1"));
    assert!(user_ixs.is_empty(), "{user_ixs:?}");
}

/// Recall after a preserved-user compaction pops its tombstone and keeps
/// the note out of sight; a later user keeps ordinary recall behavior.
#[test]
fn recall_after_a_preserved_user_keeps_the_tombstone() {
    let mut records = vec![
        user("u1"),
        assistant(
            "",
            vec![ToolCall {
                id: "c1".into(),
                name: "Read".into(),
                arguments: "{}".into(),
            }],
        ),
        result("Read", "{}", "out"),
        compact_with("note", 2, 0, Some(1)),
    ];
    let mut with_recall = records.clone();
    with_recall.push(RolloutRecord::Recall);
    let (history, user_ixs) = rebuild_history(&with_recall, "sys".into());
    assert_eq!(history.len(), 3, "nothing beyond the tombstone is dropped");
    assert_eq!(history[1].content.as_deref(), Some("note"));
    assert!(
        user_ixs.is_empty(),
        "the recall consumed the preserved user's tombstone: {user_ixs:?}"
    );

    records.push(user("u2"));
    records.push(assistant("a2", vec![]));
    records.push(RolloutRecord::Recall);
    let (history, user_ixs) = rebuild_history(&records, "sys".into());
    assert_eq!(history.len(), 3, "the later turn is recalled: {history:#?}");
    assert_eq!(history[2].content.as_deref(), Some("u1"));
    assert_eq!(user_ixs, vec![None]);
}

/// The Compact record carries only an index: media stays in the User
/// record and the rebuilt projection follows current capabilities.
#[test]
fn preserved_user_media_uses_the_rebuilt_projection_without_payloads() {
    let dir = std::env::temp_dir().join(format!("pig-preserved-media-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create media dir");
    std::fs::write(dir.join("1.png"), png_bytes()).expect("write image");
    let image = ImageRef {
        path: dir.join("1.png"),
        media_type: "image/png".into(),
        width: 1,
        height: 1,
        label: Some("Image 1".into()),
    };
    let user_record = RolloutRecord::User {
        sent_at_ms: 0,
        text: "clean".into(),
        files: vec![],
        images: vec![image.clone()],
        sent_text: Some("projected".into()),
        sent_images: vec![image],
        model_text: Some("base".into()),
        sent_input_image: Some(true),
    };
    let records = vec![
        user_record,
        assistant(
            "",
            vec![ToolCall {
                id: "c1".into(),
                name: "Read".into(),
                arguments: "{}".into(),
            }],
        ),
        result("Read", "{}", "out"),
        assistant("a1", vec![]),
        compact_with("note", 2, 1, Some(1)),
    ];
    let compact_json = serde_json::to_string(&records[4]).expect("serialize compact");
    assert!(
        compact_json.contains("\"preserved_user\":1"),
        "{compact_json}"
    );
    assert!(!compact_json.contains("data_base64"), "{compact_json}");
    assert!(!compact_json.contains("iVBOR"), "{compact_json}");

    let (frozen, _) = rebuild_history_with_media_root(&records, "sys".into(), None, &dir, None);
    assert_eq!(frozen[2].content.as_deref(), Some("projected"));
    assert_eq!(frozen[2].images.len(), 1);
    assert_eq!(frozen[2].images[0].label.as_deref(), Some("Image 1"));

    let (reprojected, _) = rebuild_history_with_media_root(
        &records,
        "sys".into(),
        Some(MediaCapabilities::new(false, false)),
        &dir,
        None,
    );
    assert!(reprojected[2].images.is_empty());
    assert!(
        reprojected[2]
            .content
            .as_deref()
            .is_some_and(|text| text.starts_with("base") && text.contains("were not sent")),
        "{:?}",
        reprojected[2].content
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Missing media on the preserved user rebuilds through the same degradation
/// path as any other User record (the compact record stores no payload).
#[test]
fn preserved_user_with_missing_media_reports_the_degradation() {
    let dir = std::env::temp_dir().join(format!(
        "pig-preserved-missing-media-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create media dir");
    let image = ImageRef {
        path: dir.join("missing.png"),
        media_type: "image/png".into(),
        width: 1,
        height: 1,
        label: Some("Image 1".into()),
    };
    let records = vec![
        RolloutRecord::User {
            sent_at_ms: 0,
            text: "clean".into(),
            files: vec![],
            images: vec![],
            sent_text: Some("projected".into()),
            sent_images: vec![image],
            model_text: Some("base".into()),
            sent_input_image: Some(true),
        },
        assistant(
            "",
            vec![ToolCall {
                id: "c1".into(),
                name: "Read".into(),
                arguments: "{}".into(),
            }],
        ),
        result("Read", "{}", "out"),
        assistant("a1", vec![]),
        compact_with("note", 2, 1, Some(1)),
    ];
    let (history, _) = rebuild_history_with_media_root(&records, "sys".into(), None, &dir, None);
    assert_eq!(history[2].role, "user");
    assert!(history[2].images.is_empty());
    assert!(
        history[2]
            .content
            .as_deref()
            .is_some_and(|text| text.contains("1 image(s) no longer available")),
        "{:?}",
        history[2].content
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn task_notification(id: &str) -> RolloutRecord {
    RolloutRecord::TaskNotificationBatch {
        notifications: vec![pig_protocol::TaskNotification {
            id: id.into(),
            task_id: "b1".into(),
            kind: pig_protocol::TaskKind::Shell,
            status: pig_protocol::TaskStatus::Completed,
            exit_code: Some(0),
            severity: pig_protocol::NotificationSeverity::Info,
            description: "cargo test".into(),
            title: "Background shell completed".into(),
            body: "cargo test completed".into(),
            duration_ms: Some(10),
            turns: None,
            model: None,
            agent_id: None,
            record_path: None,
            output: None,
        }],
        model_text: format!("<notification id=\"{id}\">done</notification>"),
        sent_at_ms: 1,
    }
}

#[test]
fn notification_batches_are_not_real_user_anchors() {
    let records = vec![
        user("one"),
        task_notification("task:s1:b1:completed"),
        user("two"),
    ];
    let (history, user_ixs) = rebuild_history(&records, "sys".into());
    assert_eq!(history.len(), 4);
    assert_eq!(user_ixs, vec![Some(1), Some(3)]);
    assert_eq!(history[2].role, "user");
}

#[test]
fn receipt_recall_and_compaction_follow_live_history() {
    let id = "task:s1:b1:completed";
    let mut wait = result("TaskWait", "{}", "done");
    let RolloutRecord::ToolCall {
        delivered_notification_ids,
        ..
    } = &mut wait
    else {
        unreachable!()
    };
    *delivered_notification_ids = vec![id.into()];
    let receipts = notification_delivery_receipts(&[user("one"), wait]);
    assert_eq!(
        receipts.get(id),
        Some(&pig_protocol::DeliveryChannel::TaskWait)
    );
    assert!(
        notification_delivery_receipts(&[
            user("one"),
            task_notification(id),
            RolloutRecord::Recall,
        ])
        .is_empty()
    );

    let mut records = vec![
        user("old"),
        task_notification(id),
        user("latest"),
        compact_with("note", 2, 1, None),
    ];
    assert_eq!(
        notification_delivery_receipts(&records).get(id),
        Some(&pig_protocol::DeliveryChannel::Automatic),
        "compaction preserves delivery even when the batch is omitted"
    );
    records.push(user("new"));
    records.push(RolloutRecord::Recall);
    assert_eq!(
        notification_delivery_receipts(&records).get(id),
        Some(&pig_protocol::DeliveryChannel::Automatic),
        "a later recall cannot revoke an already compacted delivery"
    );
}
