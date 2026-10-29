use super::*;

#[test]
fn agent_id_validation_accepts_safe_path_components_only() {
    for valid in [
        "a1-1",
        "a1699999999-42",
        "a000-000",
        "a1",
        "A1-1",
        "a1_1",
        "a1-xx",
        "a1-1.jsonl",
        "550e8400-e29b-41d4-a716-446655440000",
        ".pending",
    ] {
        validate_agent_id(valid).unwrap_or_else(|error| panic!("{valid}: {error}"));
    }
    let too_long = "a".repeat(65);
    for invalid in [
        "",
        ".",
        "..",
        too_long.as_str(),
        "/tmp/a1-1",
        "../a1-1",
        "a1-1/../escape",
        "a1-1\\..\\escape",
        "a1-1\n",
        "a1-1 ",
        "a 1",
        "a1\u{00e9}",
    ] {
        let error =
            validate_agent_id(invalid).expect_err(&format!("{invalid:?} must not be accepted"));
        assert!(
            error.contains("Invalid resume agent id"),
            "unexpected error for {invalid:?}: {error}"
        );
    }
}

#[test]
fn read_agent_by_id_enforces_containment_and_metadata_equality() {
    let tmp = identity_temp_dir("containment");
    let sessions = tmp.join("sessions");
    let agents = agents_dir(&sessions, "s1");
    seed_context(&agents, "a1-1", "a1-1");
    seed_context(&agents, "a2-2", "a3-3");

    let (meta, history) = read_agent_by_id(&sessions, "s1", "a1-1").expect("valid context");
    assert_eq!(meta.agent_id, "a1-1");
    assert_eq!(history.len(), 1);

    // Future UUID-like ids are safe path components and read like generated
    // ids; canonical containment and metadata equality still apply.
    let uuid = "550e8400-e29b-41d4-a716-446655440000";
    seed_context(&agents, uuid, uuid);
    let (meta, _) = read_agent_by_id(&sessions, "s1", uuid).expect("UUID-like context");
    assert_eq!(meta.agent_id, uuid);

    let mismatch = read_agent_by_id(&sessions, "s1", "a2-2").unwrap_err();
    assert!(
        mismatch.contains("belongs to agent_id \"a3-3\""),
        "metadata id must equal the requested id: {mismatch}"
    );
    let missing = read_agent_by_id(&sessions, "s1", "a9-9").unwrap_err();
    assert!(missing.contains("does not exist"), "{missing}");
    assert!(
        missing.contains("a1-1") && missing.contains("a2-2"),
        "the available listing names every generated context file: {missing}"
    );
    let escaped = read_agent_by_id(&sessions, "s1", "../../etc/passwd").unwrap_err();
    assert!(escaped.contains("Invalid resume agent id"), "{escaped}");
    let absolute = read_agent_by_id(&sessions, "s1", "/etc/passwd").unwrap_err();
    assert!(absolute.contains("Invalid resume agent id"), "{absolute}");
    let _ = std::fs::remove_dir_all(&tmp);
}

#[cfg(unix)]
#[test]
fn read_agent_by_id_rejects_symlinked_contexts() {
    use std::os::unix::fs::symlink;
    let tmp = identity_temp_dir("symlink");
    let sessions = tmp.join("sessions");
    let agents = agents_dir(&sessions, "s1");
    std::fs::create_dir_all(&agents).expect("agents dir");
    let outside = tmp.join("outside.jsonl");
    seed_context(&tmp, "outside", "a4-4");
    symlink(&outside, agents.join("a4-4.jsonl")).expect("plant symlink");
    let error = read_agent_by_id(&sessions, "s1", "a4-4").unwrap_err();
    assert!(
        error.contains("symbolic link"),
        "a linked context must not be resumed: {error}"
    );

    // A linked agent directory itself escapes the session directory.
    let outside_dir = tmp.join("outside-agents");
    std::fs::create_dir_all(&outside_dir).expect("outside dir");
    seed_context(&outside_dir, "a5-5", "a5-5");
    let linked_session = sessions.join("s2");
    std::fs::create_dir_all(&linked_session).expect("session dir");
    symlink(&outside_dir, agents_dir(&sessions, "s2")).expect("plant dir symlink");
    let error = read_agent_by_id(&sessions, "s2", "a5-5").unwrap_err();
    assert!(
        error.contains("symbolic link") || error.contains("escapes this session's directory"),
        "a linked agent directory must not be resumed: {error}"
    );
    let error = prepared_agent_dir(&sessions, "s2").unwrap_err();
    assert!(
        error.contains("symbolic link"),
        "fresh writes must not follow a linked agent directory: {error}"
    );

    // A link that stays *inside* the sessions directory still crosses into
    // another session's contexts and media: only a real directory named for
    // this session is the owning directory.
    seed_context(&agents_dir(&sessions, "s3"), "a6-6", "a6-6");
    symlink(agents_dir(&sessions, "s3"), agents_dir(&sessions, "s4")).expect("cross-session link");
    let error = read_agent_by_id(&sessions, "s4", "a6-6").unwrap_err();
    assert!(
        error.contains("symbolic link"),
        "another session's agents directory is not this session's owning directory: {error}"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn writer_claim_blocks_resume_until_the_driver_terminates() {
    let state = SessionToolState::for_test();
    let writer = state.try_claim_agent_writer("a1-1").expect("driver claim");
    let error = claim_resume_writer(&state, "a1-1").unwrap_err();
    assert!(
        error.contains("still shutting down"),
        "a closing driver rejects resume: {error}"
    );
    drop(writer);
    let guard = claim_resume_writer(&state, "a1-1").expect("resume after termination");
    drop(guard);

    let task_id = crate::task::register_agent_task(
        &state,
        "subagent explore: test".to_string(),
        tokio_util::sync::CancellationToken::new(),
        "a2-2".to_string(),
    );
    let error = claim_resume_writer(&state, "a2-2").unwrap_err();
    assert!(
        error.contains("still running") && error.contains(&task_id),
        "a running task rejects resume with its task id: {error}"
    );
}

#[test]
fn allocate_agent_id_returns_unique_claimed_ids() {
    let tmp = identity_temp_dir("allocate");
    let state = SessionToolState::for_test();
    let agents = agents_dir(&tmp.join("sessions"), "s1");
    let mut seq = 0u64;
    let (first, first_guard) = allocate_agent_id(&state, &agents, &mut seq).expect("first");
    let (second, second_guard) = allocate_agent_id(&state, &agents, &mut seq).expect("second");
    assert_ne!(first, second);
    assert!(validate_agent_id(&first).is_ok());
    assert!(validate_agent_id(&second).is_ok());
    assert!(state.agent_writer_active(&first));
    assert!(state.agent_writer_active(&second));
    drop(first_guard);
    drop(second_guard);
    let _ = std::fs::remove_dir_all(&tmp);
}
