use super::*;

/// A compact marker is the child's canonical replay authority: `[system,
/// note, preserved active user?, tail]`. A second read (restart) and a
/// later resume append produce the identical retained prefix.
#[test]
fn compact_record_replays_identically_and_preserves_the_active_user() {
    let dir = std::env::temp_dir().join(format!("pig-agent-compact-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test dir");
    let path = dir.join("a1.jsonl");
    write_meta(&path);
    for mut msg in [
        ChatMsg::system("child system".into()),
        ChatMsg::user("active task".into()),
        ChatMsg::assistant("old work".into(), vec![], None),
        ChatMsg::assistant("retained tail".into(), vec![], None),
    ] {
        append_test_message(&path, &mut msg).expect("seed message");
    }
    append_test_compact(
        &path,
        "[Earlier context compacted (automatic)] 1 messages omitted.",
        1,
        true,
        Some("old work summary"),
        Some(1),
        Some(1),
        Some("watermark"),
        None,
        None,
    )
    .expect("compact record");
    let (_, first) = read_agent(&path).expect("read compacted context");
    assert_eq!(
        first
            .iter()
            .map(|message| message.role.as_str())
            .collect::<Vec<_>>(),
        vec!["system", "system", "user", "assistant"]
    );
    assert!(
        first[1]
            .content
            .as_deref()
            .is_some_and(|note| note.contains("1 messages omitted")),
        "{:?}",
        first[1].content
    );
    assert_eq!(first[2].content.as_deref(), Some("active task"));
    assert_eq!(first[3].content.as_deref(), Some("retained tail"));

    // Repeated restart replays the same context.
    let (_, second) = read_agent(&path).expect("second read");
    assert_eq!(
        second.len(),
        first.len(),
        "a restart must not grow or shrink the compacted context"
    );
    for (index, message) in first.iter().enumerate() {
        assert_eq!(
            (message.role.as_str(), message.content.as_deref()),
            (
                second[index].role.as_str(),
                second[index].content.as_deref()
            )
        );
    }

    // Resume appends only the new task prompt after the compacted prefix.
    let mut resume = ChatMsg::user("resume task".into());
    append_test_message(&path, &mut resume).expect("resume prompt");
    let (_, resumed) = read_agent(&path).expect("resumed read");
    assert_eq!(resumed.len(), first.len() + 1);
    for (index, message) in first.iter().enumerate() {
        assert_eq!(
            (message.role.as_str(), message.content.as_deref()),
            (
                resumed[index].role.as_str(),
                resumed[index].content.as_deref()
            ),
            "the compacted prefix must not change across resume"
        );
    }
    assert_eq!(
        resumed.last().unwrap().content.as_deref(),
        Some("resume task")
    );
    let raw = std::fs::read_to_string(&path).expect("raw context");
    let compact_line = raw
        .lines()
        .find(|line| line.contains("\"type\":\"compact\""))
        .expect("compact line");
    assert!(
        !compact_line.contains("data_base64") && !compact_line.contains("active task"),
        "the marker carries no message content or media: {compact_line}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// New marked handoffs replay as user messages; a later cut may start at the
/// promoted checkpoint's next request instead of splitting a user run
/// without keeping its request.
#[test]
fn marked_user_handoff_and_checkpoint_cut_keep_the_active_request() {
    let dir = std::env::temp_dir().join(format!("pig-agent-handoff-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test dir");
    let path = dir.join("a1.jsonl");
    write_meta(&path);
    for mut msg in [
        ChatMsg::system("child system".into()),
        ChatMsg::user("previous request".into()),
        ChatMsg::assistant("old answer".into(), vec![], None),
    ] {
        append_test_message(&path, &mut msg).expect("seed");
    }
    let note = |omitted: usize| {
        format!(
            "{} [Earlier context compacted (automatic)] {omitted} messages omitted.",
            crate::rollout::COMPACTION_HANDOFF_MARKER
        )
    };
    append_test_compact(
        &path,
        note(1),
        1,
        true,
        None,
        Some(0),
        Some(1),
        None,
        None,
        None,
    )
    .expect("first compact");
    let (_, first) = read_agent(&path).expect("read first handoff");
    assert_eq!(
        first
            .iter()
            .map(|message| message.role.as_str())
            .collect::<Vec<_>>(),
        vec!["system", "user", "user"]
    );
    assert!(crate::rollout::is_compaction_handoff(&first[1]));

    for mut msg in [
        ChatMsg::user("next request".into()),
        ChatMsg::assistant("next answer".into(), vec![], None),
    ] {
        append_test_message(&path, &mut msg).expect("grow");
    }
    append_test_compact(
        &path,
        note(2),
        2,
        true,
        None,
        Some(2),
        None,
        None,
        None,
        None,
    )
    .expect("checkpoint cut");
    let (_, history) = read_agent(&path).expect("read checkpoint cut");
    assert_eq!(
        history
            .iter()
            .map(|message| message.role.as_str())
            .collect::<Vec<_>>(),
        vec!["system", "user", "user", "assistant"],
        "{history:#?}"
    );
    assert_eq!(history[2].content.as_deref(), Some("next request"));
    assert_eq!(history[3].content.as_deref(), Some("next answer"));

    // Preserving the active request separately from inside the checkpoint run
    // is canonical too; replay validates it as the active request, not merely
    // as the first message of its user run.
    let second = dir.join("a2.jsonl");
    write_meta(&second);
    for mut msg in [
        ChatMsg::system("child system".into()),
        ChatMsg::user("previous request".into()),
        ChatMsg::assistant("old answer".into(), vec![], None),
    ] {
        append_test_message(&second, &mut msg).expect("seed second");
    }
    append_test_compact(
        &second,
        note(1),
        1,
        true,
        None,
        Some(0),
        Some(1),
        None,
        None,
        None,
    )
    .expect("first compact second");
    for mut msg in [
        ChatMsg::user("next request".into()),
        ChatMsg::assistant("next answer".into(), vec![], None),
    ] {
        append_test_message(&second, &mut msg).expect("grow second");
    }
    append_test_compact(
        &second,
        note(2),
        2,
        true,
        None,
        Some(1),
        Some(3),
        None,
        None,
        None,
    )
    .expect("preserved checkpoint cut");
    let (_, history) = read_agent(&second).expect("read preserved checkpoint cut");
    assert_eq!(
        history
            .iter()
            .map(|message| message.role.as_str())
            .collect::<Vec<_>>(),
        vec!["system", "user", "user", "assistant"],
        "{history:#?}"
    );
    assert_eq!(history[2].content.as_deref(), Some("next request"));
    assert_eq!(history[3].content.as_deref(), Some("next answer"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Consecutive compaction markers replay in file order: the second cut is
/// applied to the first marker's rewritten history (including replacing
/// the earlier note), so repeated compactions stay deterministic.
#[test]
fn repeated_compaction_markers_replay_sequentially() {
    let dir = std::env::temp_dir().join(format!("pig-agent-compact-twice-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test dir");
    let path = dir.join("a1.jsonl");
    write_meta(&path);
    for mut msg in [
        ChatMsg::system("child system".into()),
        ChatMsg::user("first task".into()),
        ChatMsg::assistant("old first work".into(), vec![], None),
        ChatMsg::assistant("first tail".into(), vec![], None),
    ] {
        append_test_message(&path, &mut msg).expect("seed first");
    }
    append_test_compact(
        &path,
        "first note",
        1,
        true,
        None,
        Some(1),
        Some(1),
        None,
        None,
        None,
    )
    .expect("first compact");
    for mut msg in [
        ChatMsg::user("second task".into()),
        ChatMsg::assistant("old second work".into(), vec![], None),
        ChatMsg::assistant("second tail".into(), vec![], None),
    ] {
        append_test_message(&path, &mut msg).expect("seed second");
    }
    // At this point the rebuilt history is:
    // [system, first note, first task, first tail, second task, old
    //  second work, second tail]; cut=6 preserves the second task.
    append_test_compact(
        &path,
        "second note",
        4,
        true,
        None,
        Some(1),
        Some(4),
        None,
        None,
        None,
    )
    .expect("second compact");
    let (_, history) = read_agent(&path).expect("read twice-compacted context");
    assert_eq!(
        history
            .iter()
            .map(|message| message.role.as_str())
            .collect::<Vec<_>>(),
        vec!["system", "system", "user", "assistant"]
    );
    assert_eq!(history[1].content.as_deref(), Some("second note"));
    assert_eq!(history[2].content.as_deref(), Some("second task"));
    assert_eq!(history[3].content.as_deref(), Some("second tail"));
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

/// Media in a separately preserved active user is rehydrated through the
/// owning child media dir; corrupt bytes degrade to the deterministic
/// missing note, and the durable ref/bytes can restore it on the next read.
#[test]
fn compact_replay_rehydrates_preserved_media_and_degrades_corrupt_bytes() {
    let dir = std::env::temp_dir().join(format!("pig-agent-compact-media-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test dir");
    let path = dir.join("a1.jsonl");
    write_meta(&path);
    let payload = pig_utils::image::base64_encode(&png_bytes());
    let mut user = ChatMsg::user("look at this".into());
    user.images = vec![pig_provider::ChatImage {
        media_type: "image/png".into(),
        data_base64: payload.clone(),
        label: Some("pic.png".into()),
    }];
    let mut system = ChatMsg::system("child system".into());
    append_test_message(&path, &mut system).expect("system");
    append_test_message(&path, &mut user).expect("user image");
    for mut msg in [
        ChatMsg::assistant("omitted work".into(), vec![], None),
        ChatMsg::assistant("retained tail".into(), vec![], None),
    ] {
        append_test_message(&path, &mut msg).expect("seed tail");
    }
    append_test_compact(
        &path,
        "[Earlier context compacted (automatic)] 1 messages omitted.",
        1,
        true,
        None,
        Some(1),
        Some(1),
        None,
        None,
        None,
    )
    .expect("compact record");
    let raw = std::fs::read_to_string(&path).expect("raw jsonl");
    assert!(
        !raw.contains(&payload) && !raw.contains("data_base64"),
        "child JSONL must not store image base64: {raw}"
    );
    let media = path.with_extension("media").join("1.png");
    assert!(media.is_file(), "the ref's media file exists");

    let (_, rehydrated) = read_agent(&path).expect("read preserved image");
    assert_eq!(rehydrated[2].images.len(), 1);
    assert_eq!(rehydrated[2].images[0].data_base64, payload);

    let valid = std::fs::read(&media).expect("valid image bytes");
    std::fs::write(&media, b"not an image").expect("corrupt media");
    let (_, corrupt) = read_agent(&path).expect("read corrupt media");
    assert!(corrupt[2].images.is_empty());
    assert!(
        corrupt[2]
            .content
            .as_deref()
            .is_some_and(|text| text.contains("[1 image(s) no longer available]")),
        "{:?}",
        corrupt[2].content
    );

    std::fs::write(&media, &valid).expect("restore media");
    let (_, restored) = read_agent(&path).expect("read restored media");
    assert_eq!(restored[2].images.len(), 1);

    // Capability switch: the projection is in-memory only; the durable
    // ref still rehydrates for a later capable resume.
    let mut projected = restored.clone();
    assert_eq!(
        project_history_media(
            &mut projected,
            crate::tool::MediaCapabilities::new(false, false)
        ),
        (1, 0)
    );
    assert!(projected[2].images.is_empty());
    assert!(
        projected[2]
            .content
            .as_deref()
            .is_some_and(|text| text.contains("image(s) were not sent")),
        "{:?}",
        projected[2].content
    );
    let (_, capable_again) = read_agent(&path).expect("read after capability switch");
    assert_eq!(capable_again[2].images.len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A ref pointing at a valid image outside this child's media directory is
/// refused: replay may only read media the child itself owns.
#[test]
fn compact_replay_refuses_media_outside_the_child_root() {
    let dir = std::env::temp_dir().join(format!(
        "pig-agent-compact-containment-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test dir");
    let outside = dir.join("outside.png");
    std::fs::write(&outside, png_bytes()).expect("outside image");
    let path = dir.join("a1.jsonl");
    write_meta(&path);
    // The owning media root exists, so the refusal is specifically the
    // resolved-path containment check (not a missing root).
    std::fs::create_dir_all(path.with_extension("media")).expect("media root");
    append_agent_conversation_record(
        &path,
        &crate::rollout::RolloutRecord::User {
            text: "outside ref".into(),
            files: Vec::new(),
            images: vec![
                serde_json::from_value(image_ref(&outside, "outside.png"))
                    .expect("outside image ref"),
            ],
            sent_at_ms: 1,
            sent_text: None,
            sent_images: Vec::new(),
            model_text: None,
            sent_input_image: None,
        },
    )
    .expect("outside image ref");
    let (_, history) = read_agent(&path).expect("read outside ref");
    assert!(history[0].images.is_empty());
    assert!(
        history[0]
            .content
            .as_deref()
            .is_some_and(|text| text.contains("[1 image(s) no longer available]")),
        "{:?}",
        history[0].content
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A torn/tampered compact marker is ignored (with the previous history
/// intact) instead of failing the whole resume or discarding context.
#[test]
fn an_invalid_compact_record_keeps_the_previous_context() {
    let dir =
        std::env::temp_dir().join(format!("pig-agent-compact-invalid-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test dir");
    let path = dir.join("a1.jsonl");
    write_meta(&path);
    for mut msg in [
        ChatMsg::system("system".into()),
        ChatMsg::user("task".into()),
        ChatMsg::assistant("work".into(), vec![], None),
    ] {
        append_test_message(&path, &mut msg).expect("seed");
    }
    append_test_compact(
        &path,
        "bad marker",
        1,
        false,
        None,
        Some(99),
        Some(1),
        None,
        None,
        None,
    )
    .expect("invalid compact record");
    let mut resume = ChatMsg::user("continue".into());
    append_test_message(&path, &mut resume).expect("resume");
    let (_, history) = read_agent(&path).expect("read invalid marker");
    assert_eq!(
        history
            .iter()
            .map(|message| message.role.as_str())
            .collect::<Vec<_>>(),
        vec!["system", "user", "assistant", "user"],
        "the invalid marker must not rewrite history"
    );
    assert_eq!(history[1].content.as_deref(), Some("task"));
    let _ = std::fs::remove_dir_all(&dir);
}
