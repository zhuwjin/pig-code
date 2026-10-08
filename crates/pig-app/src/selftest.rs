use super::*;

/// Selftest environment: mock provider + temporary config/work dirs + isolated data dir.
pub(crate) struct SelftestEnv {
    pub(crate) config_path: PathBuf,
    pub(crate) cwd: PathBuf,
}

pub(crate) fn setup_selftest() -> SelftestEnv {
    let port = pig_core::mock::start_mock_server();
    let dir = std::env::temp_dir().join(format!("pig-app-selftest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create selftest dir");
    std::fs::write(
        dir.join(pig_core::mock::MOCK_FILE_NAME),
        pig_core::mock::MOCK_FILE_CONTENT,
    )
    .expect("write mock file");
    // The data dir must live **outside** the workspace (mirroring production
    // ~/.pigcode): inside the workspace it would be found by the Grep/Glob tools —
    // rollout/model-io files persist verbatim user messages from past turns, and a
    // subagent grepping the workspace would carry mock triggers (ECHO_HISTORY
    // etc.) back into the request body, hitting the mock's content-routing branch
    // early (the selftest once failed this way: subagent conclusions got hijacked
    // by echo_history_response)
    let data_dir =
        std::env::temp_dir().join(format!("pig-app-selftest-data-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data_dir);
    std::fs::create_dir_all(&data_dir).expect("create data dir");
    // The agent locates the isolated data dir via PIG_DATA_DIR
    unsafe { std::env::set_var("PIG_DATA_DIR", &data_dir) };
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"default_provider = "mock"
default_model = "mock-model"

[[providers]]
id = "mock"
name = "Mock"
base_url = "http://127.0.0.1:{port}/v1"
api_key = "mock-key"
api_format = "OpenAiChat"
enabled = true

[[providers.models]]
id = "mock-model"
context_window = 128000
max_output_tokens = 8192
reasoning_levels = ["low", "high"]
default_reasoning_level = "low"

[[providers]]
id = "anthropic"
name = "AnthropicMock"
base_url = "http://127.0.0.1:{port}/v1"
api_key = "mock-key"
api_format = "AnthropicMessages"
enabled = true

[[providers.models]]
id = "mock-model"
context_window = 200000
max_output_tokens = 8192
reasoning_levels = ["high", "max"]
"#
        ),
    )
    .expect("write config");
    SelftestEnv {
        config_path,
        cwd: dir,
    }
}

/// PIG_SELFTEST=1: session A full edit chain → session B parallel conversation → switch back to A → simulated restart resume → @search.
pub(crate) async fn run_selftest(
    view: Entity<AppView>,
    window_handle: AnyWindowHandle,
    cx: &mut AsyncApp,
) {
    use std::time::Duration;
    macro_rules! timer {
        ($ms:expr) => {
            cx.background_executor().timer(Duration::from_millis($ms))
        };
    }
    macro_rules! app {
        ($f:expr) => {
            view.update(cx, $f)
        };
    }

    timer!(800).await;

    // Hero state assertion: no sessions, hero shown
    let is_hero = app!(|app: &mut AppView, cx| app.debug_is_hero(cx));
    assert!(is_hero, "startup should land in the hero state");
    println!("[selftest] hero screen OK");

    // Hero-state @ file search: results must appear even with no session created
    // yet (they were once dropped at current=None and the popup never saw results)
    app!(|app: &mut AppView, cx| {
        app.composer.update(cx, |_, cx| {
            cx.emit(crate::composer::ComposerEvent::SearchFiles(
                "README".to_string(),
            ));
        });
    });
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 10_000, "hero @-search timed out");
        let found = app!(|app: &mut AppView, cx| {
            app.composer
                .read(cx)
                .debug_mention_results()
                .iter()
                .any(|r| r.contains(pig_core::mock::MOCK_FILE_NAME))
        });
        if found {
            break;
        }
    }
    println!("[selftest] hero @-search OK");

    // Send the first message from hero → a session is created automatically
    app!(|app: &mut AppView, cx| {
        app.exec_mode = pig_protocol::ExecMode::ConfirmBeforeEdit;
        app.hero_send(
            format!(
                "{} create and modify files, then run a command",
                pig_core::mock::SCENARIO_B_TRIGGER
            ),
            vec![],
            vec![],
            pig_protocol::ExecMode::ConfirmBeforeEdit,
            cx,
        );
    });
    let session_a = loop {
        timer!(200).await;
        let current = app!(|app: &mut AppView, _| app.current.clone());
        if let Some(id) = current {
            break id;
        }
    };
    println!("[selftest] hero send -> session A created: {session_a}");

    // Session A: scenario B (3 approvals)
    let mut approvals = 0u32;
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 60_000, "session A turn timed out");
        let approved = app!(|app: &mut AppView, cx| {
            let views = app.views.get(&session_a)?;
            let pending = views.thread.read(cx).pending_approval();
            pending.map(|_| {
                views.thread.update(cx, |thread, cx| {
                    thread.decide_pending(pig_protocol::ApprovalDecision::Allow, cx);
                });
            })
        });
        if approved == Some(()) {
            approvals += 1;
            continue;
        }
        let done = app!(|app: &mut AppView, cx| {
            let views = app.views.get(&session_a)?;
            let streaming = views.thread.read(cx).is_streaming();
            let (tool_done, text, _, tool_output) = views.thread.read(cx).debug_last_assistant();
            if !streaming && waited > 1000 && tool_done {
                assert!(
                    text.contains(pig_core::mock::SCENARIO_B_MARKER),
                    "A text marker: {text}"
                );
                assert!(tool_output.contains(pig_core::mock::SCENARIO_B_BASH_MARKER));
                return Some(());
            }
            None
        });
        if done == Some(()) {
            break;
        }
    }
    assert_eq!(approvals, 3, "scenario B needs 3 approvals");
    // Hero → session state switch assertion
    let is_hero = app!(|app: &mut AppView, cx| app.debug_is_hero(cx));
    assert!(!is_hero, "should enter the session state after send");
    println!("[selftest] session A scenario B done (3 approvals), composer settled at bottom");

    // Trajectory panel: the mock turn's multi-step calls should already have
    // persisted model-io records; the panel loads and renders both
    // collapsed/expanded states (building the element tree without panicking
    // passes)
    app!(|app: &mut AppView, cx| {
        app.open_right_tab(RightTab::Trajectory, cx);
        let records = app.trajectory.as_ref().map(|s| s.records.len());
        assert!(
            records.is_some_and(|n| n >= 2),
            "scenario B multi-step calls should persist multiple model-io records: {records:?}"
        );
        let _ = app.render_trajectory_panel(cx);
    });
    app!(|app: &mut AppView, cx| {
        // Row-by-row expansion: simulate opening the first row of the first call
        // and re-render the expanded state
        if let Some(state) = &mut app.trajectory {
            let key = format!("{}:0", state.records[0].turn);
            state.expanded.insert(key);
        }
        let _ = app.render_trajectory_panel(cx);
        app.close_right_tab(RightTab::Trajectory, cx);
    });
    println!("[selftest] trajectory panel load/expand rendering OK");

    // File viewer: simulate clicking the Read card path (ThreadEvent::OpenFile
    // shares the click path) → the right "Files" tab opens and loads the full
    // content (README.mock.md is scenario B's Read target)
    app!(|app: &mut AppView, cx| {
        let views = app.views.get(&session_a).expect("session A view");
        views.thread.update(cx, |_, cx| {
            cx.emit(crate::thread_view::ThreadEvent::OpenFile {
                path: pig_core::mock::MOCK_FILE_NAME.to_string(),
                line: Some(1),
            });
        });
    });
    // Loading runs on a background thread (disk read + tree-sitter highlighting);
    // poll until ready
    let mut file_waited = 0u64;
    loop {
        timer!(100).await;
        file_waited += 100;
        assert!(file_waited < 10_000, "file panel load timed out");
        if let Some((path, lines)) = app!(|app: &mut AppView, cx| app.debug_file_tab(cx)) {
            assert!(lines > 0, "file panel should have content lines: {path}");
            break;
        }
    }
    // Close the file tab to restore the right panel's collapsed state (later
    // steps assert "collapsed by default")
    app!(|app: &mut AppView, cx| {
        if let Some(RightTab::File { path }) = app.right_active.clone() {
            app.close_right_tab(RightTab::File { path }, cx);
        }
    });
    println!("[selftest] file view panel (Read path click -> right tab load) OK");

    // Expand the Bash tool card (command card + output card render paths:
    // highlighting + horizontal scroll regions); rendering a few real frames
    // without panicking passes (the Read card is expanded in the session B step,
    // which has the Read call)
    let expanded = app!(|app: &mut AppView, cx| {
        let views = app.views.get(&session_a)?;
        let mut hit = false;
        views.thread.update(cx, |thread, cx| {
            hit = thread.debug_expand_tool("Bash", cx);
        });
        Some(hit)
    });
    assert_eq!(
        expanded,
        Some(true),
        "session A should have a Bash tool card"
    );
    timer!(300).await;
    println!("[selftest] Bash code card expand rendering OK");

    // Workspace view: the session cwd should appear in the workspace list and be
    // grouped correctly
    let cwd_str = app!(|app: &mut AppView, _| app.cwd.display().to_string());
    let (has_cwd, grouped) = app!(|app: &mut AppView, cx| {
        let sidebar = app.sidebar.read(cx);
        (
            sidebar.debug_workspaces().contains(&cwd_str),
            sidebar
                .debug_workspace_sessions(&cwd_str)
                .contains(&session_a),
        )
    });
    assert!(has_cwd, "workspace list should contain the session cwd");
    assert!(grouped, "workspace view should group sessions by cwd");

    // Add/remove workspace
    let extra = std::env::temp_dir().join(format!("pig-app-ws-{}", std::process::id()));
    std::fs::create_dir_all(&extra).unwrap();
    let extra_str = extra.display().to_string();
    app!(|app: &mut AppView, _| app.agent.add_workspace(extra.clone()));
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 10_000, "add-workspace timed out");
        let has = app!(|app: &mut AppView, cx| {
            app.sidebar.read(cx).debug_workspaces().contains(&extra_str)
        });
        if has {
            break;
        }
    }
    app!(|app: &mut AppView, _| app.agent.remove_workspace(extra.clone()));
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 10_000, "remove-workspace timed out");
        let has = app!(|app: &mut AppView, cx| {
            app.sidebar.read(cx).debug_workspaces().contains(&extra_str)
        });
        if !has {
            break;
        }
    }
    println!("[selftest] workspace list OK (session cwd auto-appears + manual add/remove)");

    // Session B: create new + scenario A
    app!(|app: &mut AppView, _| app.agent.new_session(
        app.cwd.clone(),
        None,
        None,
        None,
        None,
        None
    ));
    let session_b = loop {
        timer!(200).await;
        let current = app!(|app: &mut AppView, _| app.current.clone());
        if let Some(id) = current
            && id != session_a
        {
            break id;
        }
    };
    println!("[selftest] session B ready: {session_b}");
    app!(|app: &mut AppView, _| {
        app.agent.send_message(
            session_b.clone(),
            "read README.mock.md and summarize".to_string(),
            vec![],
            vec![],
            pig_protocol::ExecMode::AutoEdit,
        );
    });
    timer!(300).await; // wait for the first turn to start
    app!(|app: &mut AppView, _| {
        app.agent.send_message(
            session_b.clone(),
            "ECHO_HISTORY".to_string(),
            vec![],
            vec![],
            pig_protocol::ExecMode::AutoEdit,
        );
    });
    let mut saw_queued = false;
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 60_000, "queueing flow timed out");
        let (queued_len, streaming, text) = app!(|app: &mut AppView, cx| {
            let Some(views) = app.views.get(&session_b) else {
                return (0, false, String::new());
            };
            let thread = views.thread.read(cx);
            let (_, text, _, _) = thread.debug_last_assistant();
            (thread.debug_queued().len(), thread.is_streaming(), text)
        });
        saw_queued |= queued_len > 0;
        if !streaming && text.contains("HISTORY_COUNT:6") {
            break;
        }
    }
    assert!(saw_queued, "a queued chip should have appeared");
    println!(
        "[selftest] session B done, message queueing OK (auto-continuation, 6 history items on round 2)"
    );

    // Expand session B's Read tool card (code card render path: line numbers +
    // highlighting + horizontal scroll region); rendering a few real frames
    // without panicking passes
    let expanded = app!(|app: &mut AppView, cx| {
        let views = app.views.get(&session_b)?;
        let mut hit = false;
        views.thread.update(cx, |thread, cx| {
            hit = thread.debug_expand_tool("Read", cx);
        });
        Some(hit)
    });
    assert_eq!(
        expanded,
        Some(true),
        "session B should have a Read tool card"
    );
    timer!(300).await;
    println!("[selftest] Read code card expand rendering OK");

    // Turn nav bar: session B has 2 turns of user messages; the panel is drawn
    // (non-zero width) and past the breakpoint. bounds are recorded in prepaint:
    // data readiness does not mean a frame was drawn, so wait for draw loops
    let mut waited = 0u64;
    let (mut nav_turns, mut nav_pane_w) = app!(|app: &mut AppView, cx| {
        let views = app.views.get(&session_b).expect("session B view in memory");
        views.thread.read(cx).debug_nav_state()
    });
    loop {
        if nav_pane_w >= 720. || waited >= 10_000 {
            break;
        }
        timer!(200).await;
        waited += 200;
        (nav_turns, nav_pane_w) = app!(|app: &mut AppView, cx| {
            let views = app.views.get(&session_b).expect("session B view in memory");
            views.thread.read(cx).debug_nav_state()
        });
    }
    assert!(nav_turns >= 2, "session B should have >= 2 user turns");
    assert!(
        nav_pane_w >= 720.,
        "message pane width {nav_pane_w} should be >= 720 (nav bar breakpoint)"
    );
    println!(
        "[selftest] turn nav bar visibility OK ({nav_turns} turns, pane width {nav_pane_w:.0})"
    );

    // At bottom, the active item should be the last turn's user message
    // (regression: it used to highlight an earlier turn while pinned to bottom by
    // "nearest to viewport top" — several user messages are visible at once in
    // the bottom viewport and the top-nearest skews early)
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        let (active, offset_y, max_offset_y, view_h, user_rows) = app!(|app: &mut AppView, cx| {
            let views = app.views.get(&session_b).expect("session B view in memory");
            views.thread.read(cx).debug_nav_active_detail()
        });
        let last_ix = user_rows.last().map(|(ix, _, _)| *ix);
        if last_ix.is_some() && active == last_ix {
            break;
        }
        assert!(
            waited < 10_000,
            "at bottom the active item should be the last turn: active={active:?} last={last_ix:?} \
             offset_y={offset_y:.1} max_offset_y={max_offset_y:.1} view_h={view_h:.1} \
             user_rows={user_rows:?}"
        );
    }
    println!("[selftest] turn nav bar active item OK (follow bottom = last turn)");

    // Switch back to A: in-memory state must be preserved as-is
    app!(|app: &mut AppView, cx| app.switch_session(session_a.clone(), cx));
    let current = app!(|app: &mut AppView, _| app.current.clone());
    assert_eq!(current.as_ref(), Some(&session_a));
    let kept = app!(|app: &mut AppView, cx| {
        let views = app.views.get(&session_a).expect("session A view in memory");
        let (_, text, _, _) = views.thread.read(cx).debug_last_assistant();
        text.contains(pig_core::mock::SCENARIO_B_MARKER)
    });
    assert!(kept, "content should be kept after switching back to A");
    println!("[selftest] session switching OK");

    // Simulated restart: drop the manager, re-spawn + ListSessions/OpenSession replay
    app!(|app: &mut AppView, cx| app.restart_agent(cx));
    // Wait for the most recent session to auto-open (B, latest updated_at), then
    // explicitly switch to A to trigger replay
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 10_000, "auto-open session after restart timed out");
        let ready = app!(|app: &mut AppView, _| app.current.is_some());
        if ready {
            break;
        }
    }
    app!(|app: &mut AppView, cx| app.switch_session(session_a.clone(), cx));
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 30_000, "replay after restart timed out");
        let done = app!(|app: &mut AppView, cx| {
            let views = app.views.get(&session_a)?;
            let (tool_done, text, _, _) = views.thread.read(cx).debug_last_assistant();
            let review_state = views.review.read(cx).debug_state();
            (tool_done
                && text.contains(pig_core::mock::SCENARIO_B_MARKER)
                && review_state.0 == 1
                && review_state.1 == 3)
                .then_some(())
        });
        if done == Some(()) {
            break;
        }
    }
    println!("[selftest] simulated-restart resume OK (messages + tool cards + diffs all restored)");

    // @search: real files
    app!(|app: &mut AppView, _| {
        app.agent
            .search_files(session_a.clone(), "hello".to_string(), None);
    });
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 10_000, "@-search timed out");
        let found = app!(|app: &mut AppView, cx| {
            app.composer
                .read(cx)
                .debug_mention_results()
                .iter()
                .any(|r| r.contains("hello.txt"))
        });
        if found {
            break;
        }
    }
    println!("[selftest] @-search OK");

    // compact (model summary)
    app!(|app: &mut AppView, _| app.agent.compact(session_a.clone(), None));
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 15_000, "compact timed out");
        let compacted = app!(|app: &mut AppView, cx| {
            app.views
                .get(&session_a)
                .map(|views| views.thread.read(cx).debug_system_notes())
                .unwrap_or_default()
                .iter()
                .any(|note| {
                    note.contains("model summary") && note.contains(pig_core::mock::SUMMARY_MARKER)
                })
        });
        if compacted {
            break;
        }
    }
    // After compact settles, the "compacting" flag must already be cleared
    // (ContextCompacted pairs with CompactStarted in the same session)
    let still_compacting = app!(|app: &mut AppView, cx| {
        app.views
            .get(&session_a)
            .map(|views| views.thread.read(cx).debug_compacting())
            .unwrap_or(false)
    });
    assert!(
        !still_compacting,
        "compacting flag should be cleared after compact settles"
    );
    println!("[selftest] model summary compact OK");

    // Compact summary panel: simulate clicking the divider's "view summary" link
    // (ThreadEvent::OpenCompactSummary shares the click path) → the right
    // "compact summary" tab opens with the summary markdown
    app!(|app: &mut AppView, cx| {
        let views = app.views.get(&session_a).expect("session A view");
        views.thread.update(cx, |_, cx| {
            cx.emit(crate::thread_view::ThreadEvent::OpenCompactSummary {
                text: format!("## Summary\n\n{}", pig_core::mock::SUMMARY_MARKER),
            });
        });
    });
    let mut summary_waited = 0u64;
    loop {
        timer!(100).await;
        summary_waited += 100;
        assert!(summary_waited < 5_000, "compact summary tab open timed out");
        let opened = app!(|app: &mut AppView, _| {
            app.right_active == Some(RightTab::CompactSummary) && app.compact_summary.is_some()
        });
        if opened {
            break;
        }
    }
    app!(|app: &mut AppView, cx| {
        app.close_right_tab(RightTab::CompactSummary, cx);
        assert!(
            app.compact_summary.is_none(),
            "closing the tab should release the render state"
        );
    });
    println!("[selftest] compact summary panel open/close OK");

    // Scenario C: plan mode loop
    app!(|app: &mut AppView, _| app.agent.new_session(
        app.cwd.clone(),
        None,
        None,
        None,
        None,
        None
    ));
    let session_c = loop {
        timer!(200).await;
        let current = app!(|app: &mut AppView, _| app.current.clone());
        if let Some(id) = current
            && id != session_a
            && id != session_b
        {
            break id;
        }
    };
    app!(|app: &mut AppView, cx| {
        // Plan mode is orthogonal to the permission level: level pinned to
        // "confirm before edit" (3 approvals during execution), plan mode toggled
        // on separately
        app.apply_exec_mode(pig_protocol::ExecMode::ConfirmBeforeEdit, cx);
        app.apply_plan_mode(true, cx);
        app.agent.send_message(
            session_c.clone(),
            format!(
                "{} give me a refactoring plan",
                pig_core::mock::SCENARIO_C_TRIGGER
            ),
            vec![],
            vec![],
            pig_protocol::ExecMode::ConfirmBeforeEdit,
        );
    });
    // kimi file-semantics loop: the mock first Writes the plan file
    // (pass-through, no approval) → ExitPlanMode raises the approval panel →
    // approve → scenario B tool chain (Write/Edit/Bash, 3 approvals) → wrap up
    let mut approvals = 0u32;
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 60_000, "scenario C execution timed out");
        let approved = app!(|app: &mut AppView, cx| {
            let views = app.views.get(&session_c)?;
            views.thread.read(cx).pending_approval().map(|_| {
                views.thread.update(cx, |thread, cx| {
                    thread.decide_pending(pig_protocol::ApprovalDecision::Allow, cx);
                });
            })
        });
        if approved == Some(()) {
            approvals += 1;
            continue;
        }
        let done = app!(|app: &mut AppView, cx| {
            let views = app.views.get(&session_c)?;
            let thread = views.thread.read(cx);
            let (tool_done, text, _, _) = thread.debug_last_assistant();
            (!thread.is_streaming()
                && waited > 1000
                && tool_done
                && text.contains(pig_core::mock::SCENARIO_B_MARKER))
            .then_some(())
        });
        if done == Some(()) {
            break;
        }
    }
    assert_eq!(
        approvals, 4,
        "1 ExitPlanMode approval + 3 scenario B approvals"
    );
    let mode = app!(|app: &mut AppView, cx| app.composer.read(cx).debug_exec_mode());
    let plan_on = app!(|app: &mut AppView, cx| app.composer.read(cx).plan_enabled());
    assert_eq!(
        mode,
        pig_protocol::ExecMode::ConfirmBeforeEdit,
        "mode tier unchanged after approval (orthogonal)"
    );
    assert!(!plan_on, "plan toggle should be off after approval");
    // kimi file semantics: the plan file is really persisted (the mock writes it
    // via pass-through Write)
    let cwd = app!(|app: &mut AppView, _| app.cwd.clone());
    let plan_file = cwd.join(".pigcode/plans/plan-mock.md");
    let plan_text = std::fs::read_to_string(&plan_file).unwrap_or_else(|e| {
        panic!(
            "plan file should have been persisted at {}: {e}",
            plan_file.display()
        )
    });
    assert!(
        plan_text.contains(pig_core::mock::PLAN_MARKER),
        "plan file should contain the full plan text: {plan_text}"
    );
    println!("[selftest] plan mode loop (Write plan file -> panel approval -> work starts) OK");

    // Usage watermark bar
    let usage = app!(|app: &mut AppView, cx| app.composer.read(cx).debug_context_usage());
    assert_eq!(
        usage,
        Some((142, 128_000)),
        "usage watermark bar data: {usage:?}"
    );
    println!("[selftest] context usage watermark bar OK");

    // Settings page data: ConfigSnapshot received and the composer model list
    // populated
    let (provider_count, model_count) = app!(|app: &mut AppView, cx| {
        (
            app.debug_config().map(|c| c.providers.len()).unwrap_or(0),
            app.composer.read(cx).debug_model_count(),
        )
    });
    assert_eq!(
        provider_count, 2,
        "ConfigSnapshot should contain 2 providers"
    );
    assert_eq!(model_count, 2, "composer should list 2 models");
    println!("[selftest] ConfigSnapshot + model list OK");

    // Anthropic provider end-to-end: session D switches to the anthropic model
    // and runs scenario B
    app!(|app: &mut AppView, _| app.agent.new_session(
        app.cwd.clone(),
        None,
        None,
        None,
        None,
        None
    ));
    let session_d = loop {
        timer!(200).await;
        let current = app!(|app: &mut AppView, _| app.current.clone());
        if let Some(id) = current
            && id != session_a
            && id != session_b
            && id != session_c
        {
            break id;
        }
    };
    app!(|app: &mut AppView, _| {
        app.agent.set_model(
            session_d.clone(),
            "anthropic".to_string(),
            "mock-model".to_string(),
            None,
        );
        app.agent.send_message(
            session_d.clone(),
            format!(
                "{} create and modify files, then run a command",
                pig_core::mock::SCENARIO_B_TRIGGER
            ),
            vec![],
            vec![],
            pig_protocol::ExecMode::AutoEdit,
        );
    });
    let mut approvals = 0u32;
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        if waited >= 60_000 {
            // Flake diagnostics: dump the actual message layout at the stall
            // (role + segment kinds + tool done flags) before failing
            let layout = app!(|app: &mut AppView, cx| {
                let views = app.views.get(&session_d)?;
                let thread = views.thread.read(cx);
                Some((
                    thread.debug_layout(),
                    thread.debug_last_assistant(),
                    thread.is_streaming(),
                ))
            });
            if let Some((lines, last, streaming)) = layout {
                let ids = app!(|app: &mut AppView, _| {
                    Some(format!(
                        "a={} b={} c={} d={} current={:?}",
                        session_a, session_b, session_c, session_d, app.current
                    ))
                });
                println!(
                    "[selftest] scenario B stall dump: streaming={streaming} last_assistant={last:?} ids={ids:?}"
                );
                for line in lines {
                    println!("[selftest]   {line}");
                }
            }
        }
        assert!(waited < 60_000, "Anthropic scenario B timed out");
        let approved = app!(|app: &mut AppView, cx| {
            let views = app.views.get(&session_d)?;
            views.thread.read(cx).pending_approval().map(|_| {
                views.thread.update(cx, |thread, cx| {
                    thread.decide_pending(pig_protocol::ApprovalDecision::Allow, cx);
                });
            })
        });
        if approved == Some(()) {
            approvals += 1;
            continue;
        }
        let done = app!(|app: &mut AppView, cx| {
            let views = app.views.get(&session_d)?;
            let thread = views.thread.read(cx);
            let (tool_done, text, thinking, _) = thread.debug_last_assistant();
            let _ = thinking;
            (!thread.is_streaming()
                && waited > 1000
                && tool_done
                && text.contains(pig_core::mock::SCENARIO_B_MARKER))
            .then_some(())
        });
        if done == Some(()) {
            break;
        }
    }
    assert_eq!(approvals, 1, "under AutoEdit only the Bash approval");
    println!("[selftest] Anthropic provider end-to-end OK");

    // AskUserQuestion: session E runs SCENARIO_Q → question bar appears → pick
    // option → submit → marker + tool card
    app!(|app: &mut AppView, _| app.agent.new_session(
        app.cwd.clone(),
        None,
        None,
        None,
        None,
        None
    ));
    let session_e = loop {
        timer!(200).await;
        let current = app!(|app: &mut AppView, _| app.current.clone());
        if let Some(id) = current
            && id != session_a
            && id != session_b
            && id != session_c
            && id != session_d
        {
            break id;
        }
    };
    app!(|app: &mut AppView, _| {
        app.agent.send_message(
            session_e.clone(),
            format!(
                "{} help me decide the implementation approach",
                pig_core::mock::SCENARIO_Q_TRIGGER
            ),
            vec![],
            vec![],
            pig_protocol::ExecMode::AutoEdit,
        );
    });
    // Wait for the question bar (questions and approvals are mutually exclusive,
    // questions win; under AutoEdit AskUserQuestion needs no approval)
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 30_000, "question bar appearance timed out");
        let has =
            app!(|app: &mut AppView, cx| { app.composer.read(cx).debug_question().is_some() });
        if has {
            break;
        }
    }
    println!("[selftest] AskUserQuestion question bar appears OK");
    // Wizard paging: pick "Option A" on question 1 → next → pick "Yes" on
    // question 2 → submit
    let q1 = app!(|app: &mut AppView, cx| app.composer.read(cx).debug_question());
    assert_eq!(
        q1.as_deref(),
        Some("Choose an implementation approach"),
        "first question text: {q1:?}"
    );
    app!(|app: &mut AppView, cx| {
        app.composer.update(cx, |composer, cx| {
            composer.debug_select_question_option(0, 0, cx);
            composer.debug_next_question_page(cx);
        });
    });
    let q2 = app!(|app: &mut AppView, cx| app.composer.read(cx).debug_question());
    assert_eq!(
        q2.as_deref(),
        Some("Should tests run?"),
        "after paging, question 2 should be shown: {q2:?}"
    );
    println!("[selftest] AskUserQuestion paging OK");
    app!(|app: &mut AppView, cx| {
        app.composer.update(cx, |composer, cx| {
            composer.debug_select_question_option(1, 0, cx);
            composer.debug_submit_question(cx);
        });
    });
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 30_000, "AskUserQuestion turn timed out");
        let done = app!(|app: &mut AppView, cx| {
            let views = app.views.get(&session_e)?;
            let thread = views.thread.read(cx);
            let (tool_done, text, _, tool_output) = thread.debug_last_assistant();
            if !thread.is_streaming() && waited > 1000 && tool_done {
                assert!(
                    text.contains(pig_core::mock::MOCK_Q_MARKER),
                    "E text marker: {text}"
                );
                return Some(tool_output);
            }
            None
        });
        if let Some(tool_output) = done {
            assert!(
                tool_output.contains("Option A"),
                "tool output should contain the question 1 answer: {tool_output}"
            );
            assert!(
                tool_output.contains("Should tests run?: Yes"),
                "tool output should contain the question 2 answer: {tool_output}"
            );
            break;
        }
    }
    let has_card = app!(|app: &mut AppView, cx| {
        let views = app.views.get(&session_e)?;
        Some(views.thread.read(cx).debug_has_tool_call("AskUserQuestion"))
    });
    assert_eq!(
        has_card,
        Some(true),
        "tool card should show AskUserQuestion"
    );
    println!("[selftest] AskUserQuestion scenario OK");

    // Right panel: collapsed by default → the panel button opens it directly
    // (menu page shows when there is no tab) → open the changes tab → the
    // shortcut toggles it back collapsed (tab kept) → after × closes the last tab
    // the panel auto-collapses
    let right_initial = app!(|app: &mut AppView, _| app.right_open);
    assert!(!right_initial, "right panel should be collapsed by default");
    app!(|app: &mut AppView, cx| app.toggle_right_panel(cx));
    let (open, active) = app!(|app: &mut AppView, _| (app.right_open, app.right_active.clone()));
    assert!(
        open && active.is_none(),
        "panel open with no tab should show the menu page"
    );
    app!(|app: &mut AppView, cx| app.open_right_tab(RightTab::Changes, cx));
    let (open, active) = app!(|app: &mut AppView, _| (app.right_open, app.right_active.clone()));
    assert!(
        open && active == Some(RightTab::Changes),
        "the changes tab should be open"
    );
    app!(|app: &mut AppView, cx| app.toggle_right_tab(RightTab::Changes, cx));
    let (open, kept) = app!(|app: &mut AppView, _| {
        (app.right_open, app.right_active == Some(RightTab::Changes))
    });
    assert!(
        !open && kept,
        "toggling again should collapse the panel and keep the tab"
    );
    app!(|app: &mut AppView, cx| app.close_right_tab(RightTab::Changes, cx));
    let (open, active, tabs) = app!(|app: &mut AppView, _| {
        (
            app.right_open,
            app.right_active.clone(),
            app.right_tabs.len(),
        )
    });
    assert!(
        !open && active.is_none() && tabs == 0,
        "closing a tab while collapsed keeps the collapsed state; tabs cleared"
    );
    // Close the last tab while the panel is open: the panel auto-collapses
    app!(|app: &mut AppView, cx| {
        app.open_right_tab(RightTab::Changes, cx);
        app.close_right_tab(RightTab::Changes, cx);
    });
    let (open, active) = app!(|app: &mut AppView, _| (app.right_open, app.right_active.clone()));
    assert!(
        !open && active.is_none(),
        "panel should auto-collapse after the last tab is closed"
    );
    app!(|app: &mut AppView, cx| app.toggle_right_panel(cx));
    println!("[selftest] right panel open/close OK");

    // Bottom terminal panel: collapsed by default → expand (real PTY spawn +
    // TerminalElement render chain) → toggle back collapsed (tab/process kept,
    // panel merely hidden). The toggle needs &mut Window (focus/view creation),
    // so it is driven via update_window back in window context.
    let terminal_initial = app!(|app: &mut AppView, _| app.terminal_open);
    assert!(
        !terminal_initial,
        "terminal panel should be collapsed by default"
    );
    cx.update_window(window_handle, |_, window, cx| {
        view.update(cx, |app, cx| app.toggle_terminal_panel(window, cx));
    })
    .expect("selftest window should be available");
    // Wait for PTY spawn, the zsh prompt to land in the grid, and a few render
    // frames (the full prepaint/paint chain runs)
    timer!(600).await;
    let (open, tabs) = app!(|app: &mut AppView, cx| {
        (
            app.terminal_open,
            app.terminal
                .as_ref()
                .map(|p| p.read(cx).debug_tab_count())
                .unwrap_or(0),
        )
    });
    assert!(
        open && tabs >= 1,
        "terminal panel should be open with at least one tab"
    );
    cx.update_window(window_handle, |_, window, cx| {
        view.update(cx, |app, cx| app.toggle_terminal_panel(window, cx));
    })
    .expect("selftest window should be available");
    let (closed, kept) = app!(|app: &mut AppView, cx| {
        (
            !app.terminal_open,
            app.terminal
                .as_ref()
                .map(|p| p.read(cx).debug_tab_count())
                .unwrap_or(0)
                >= 1,
        )
    });
    assert!(
        closed && kept,
        "toggling again should collapse the terminal panel and keep the tab"
    );
    println!("[selftest] terminal panel open/close OK");

    // Panel menu (the tab bar's "+"): click to open, click again to collapse
    app!(|app: &mut AppView, cx| {
        app.toggle_right_menu(&ClickEvent::default(), cx);
    });
    let menu_open = app!(|app: &mut AppView, _| app.right_menu_open);
    assert!(menu_open, "menu should be open");
    app!(|app: &mut AppView, cx| {
        app.toggle_right_menu(&ClickEvent::default(), cx);
    });
    let menu_closed = app!(|app: &mut AppView, _| !app.right_menu_open);
    assert!(menu_closed, "clicking again should collapse the menu");
    println!("[selftest] right panel menu OK");

    // Changes chip: clicking now opens the right changes panel directly (no more popup)
    app!(|app: &mut AppView, cx| {
        app.composer
            .update(cx, |_, cx| cx.emit(ComposerEvent::OpenChanges));
    });
    let (open, active) = app!(|app: &mut AppView, _| (app.right_open, app.right_active.clone()));
    assert!(
        open && active == Some(RightTab::Changes),
        "changes chip should open the right panel and activate the changes tab"
    );
    println!("[selftest] changes chip -> right review panel OK");

    // Session management: first message auto-titles → manual rename → delete
    app!(|app: &mut AppView, _| app.agent.new_session(
        app.cwd.clone(),
        None,
        None,
        None,
        None,
        None
    ));
    let session_f = loop {
        timer!(200).await;
        let current = app!(|app: &mut AppView, _| app.current.clone());
        if let Some(id) = current
            && id != session_a
            && id != session_b
            && id != session_c
            && id != session_d
            && id != session_e
        {
            break id;
        }
    };
    // First message (≥10 chars) triggers the auto-title sidecar; the mock
    // returns {"title": MOCK_TITLE}
    app!(|app: &mut AppView, _| {
        app.agent.send_message(
            session_f.clone(),
            "help me outline this project's module structure and suggest refactoring".to_string(),
            vec![],
            vec![],
            pig_protocol::ExecMode::AutoEdit,
        );
    });
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 30_000, "auto-titling timed out");
        let done = app!(|app: &mut AppView, cx| {
            let Some(views) = app.views.get(&session_f) else {
                return false;
            };
            let thread = views.thread.read(cx);
            let (_, text, _, _) = thread.debug_last_assistant();
            let title = app
                .metas
                .iter()
                .find(|m| m.id == session_f)
                .map(|m| m.title.clone());
            !thread.is_streaming()
                && waited > 1000
                && !text.is_empty()
                && title.as_deref() == Some(pig_core::mock::MOCK_TITLE)
        });
        if done {
            break;
        }
    }
    println!("[selftest] first-message auto-titling OK (mock title replaces 30-char seed)");

    // Manual rename: only counts as truly persisted when the full SessionList
    // refresh after core persists it (title_custom) still shows the new name
    // (the local patch only covers immediate display)
    app!(|app: &mut AppView, cx| app.rename_session(&session_f, "manual rename F", cx));
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 10_000, "rename persistence timed out");
        let done = app!(|app: &mut AppView, _| {
            app.metas
                .iter()
                .find(|m| m.id == session_f)
                .map(|m| m.title.as_str())
                == Some("manual rename F")
        });
        if done {
            break;
        }
    }
    println!("[selftest] session manual rename OK");

    // Delete session: view/list/rollout file all cleaned up; deleting the
    // current session switches away automatically
    let data_dir =
        std::path::PathBuf::from(std::env::var("PIG_DATA_DIR").expect("selftest data dir"));
    let jsonl = data_dir.join("sessions").join(format!("{session_f}.jsonl"));
    assert!(
        jsonl.exists(),
        "rollout should exist before deletion: {}",
        jsonl.display()
    );
    app!(|app: &mut AppView, cx| app.delete_session(&session_f, cx));
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 10_000, "session deletion timed out");
        let gone = app!(|app: &mut AppView, _| {
            !app.metas.iter().any(|m| m.id == session_f) && !app.views.contains_key(&session_f)
        });
        if gone && !jsonl.exists() {
            break;
        }
    }
    assert!(
        app!(|app: &mut AppView, _| app.current.clone()) != Some(session_f),
        "should switch away after deleting the current session"
    );
    println!("[selftest] session deletion OK (views + list + rollout all cleaned)");

    // ---- New-session model selection must not be clobbered by the workspace
    // seed (regression: "switch model → pick workspace → send" was once
    // overridden by apply_hero_defaults with the workspace's old model) ----
    // Setup: explicitly create a session with mock and finish a turn so it
    // becomes the workspace's latest seed
    app!(|app: &mut AppView, _| app.agent.new_session(
        app.cwd.clone(),
        Some("mock".to_string()),
        Some("mock-model".to_string()),
        None,
        None,
        None,
    ));
    let known: Vec<String> =
        app!(|app: &mut AppView, _| { app.metas.iter().map(|m| m.id.clone()).collect() });
    let seed_id = loop {
        timer!(200).await;
        let found = app!(|app: &mut AppView, _| {
            let Some(sid) = &app.current else { return None };
            (!known.contains(sid)).then(|| sid.clone())
        });
        if let Some(id) = found {
            break id;
        }
    };
    app!(|app: &mut AppView, _| {
        app.agent.send_message(
            seed_id.clone(),
            "seed session check-in".to_string(),
            vec![],
            vec![],
            pig_protocol::ExecMode::AutoEdit,
        );
    });
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 30_000, "model seed session timed out");
        let done = app!(|app: &mut AppView, cx| {
            let Some(views) = app.views.get(&seed_id) else {
                return false;
            };
            !views.thread.read(cx).is_streaming() && waited > 1000
        });
        if done {
            break;
        }
    }

    // Setup assertion: with no explicit model chosen, the hero default comes from
    // the workspace seed (latest = the just-created mock session)
    app!(|app: &mut AppView, cx| app.enter_hero(cx));
    timer!(400).await;
    let seeded = app!(|app: &mut AppView, _| app.current_model.clone());
    assert_eq!(
        seeded,
        Some(("mock".to_string(), "mock-model".to_string())),
        "with no model chosen, the hero default should come from the workspace seed: {seeded:?}"
    );

    // Variant 2: hero → switch to anthropic → pick workspace again (triggers
    // apply_hero_defaults) → send
    app!(|app: &mut AppView, cx| app.enter_hero(cx));
    app!(|app: &mut AppView, cx| {
        app.composer.update(cx, |_, cx| {
            cx.emit(ComposerEvent::SetModel {
                provider_id: "anthropic".to_string(),
                model_id: "mock-model".to_string(),
            });
        });
    });
    timer!(400).await;
    let cwd_str = app!(|app: &mut AppView, _| app.cwd.display().to_string());
    app!(|app: &mut AppView, cx| {
        app.composer.update(cx, |_, cx| {
            cx.emit(ComposerEvent::SelectCwd(cwd_str.clone()));
        });
    });
    timer!(400).await;
    let picked = app!(|app: &mut AppView, _| (app.current_model.clone(), app.hero_cwd.is_some()));
    assert_eq!(
        picked,
        (
            Some(("anthropic".to_string(), "mock-model".to_string())),
            true
        ),
        "after picking a workspace, the user-chosen model should not be overridden by the seed: {picked:?}"
    );
    app!(|app: &mut AppView, cx| {
        app.hero_send(
            "model selection regression v2".to_string(),
            vec![],
            vec![],
            pig_protocol::ExecMode::AutoEdit,
            cx,
        );
    });
    let mut waited = 0u64;
    let v2_id = loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 20_000, "v2 session creation timed out");
        let found = app!(|app: &mut AppView, cx| {
            let Some(sid) = &app.current else { return None };
            if known.contains(sid) {
                return None;
            }
            let streaming = app
                .views
                .get(sid)
                .map(|v| v.thread.read(cx).is_streaming())
                .unwrap_or(true);
            (!streaming).then(|| sid.clone())
        });
        if let Some(id) = found {
            break id;
        }
    };
    let v2 = app!(|app: &mut AppView, _| {
        app.metas
            .iter()
            .find(|m| m.id == v2_id)
            .map(|m| (m.provider_id.clone(), m.model_id.clone()))
    });
    assert_eq!(
        v2,
        Some((
            Some("anthropic".to_string()),
            Some("mock-model".to_string())
        )),
        "switch model -> pick workspace -> send: the new session should use the user-chosen model: {v2:?}"
    );
    println!("[selftest] model kept after hero model switch then workspace pick OK");

    // Variant 1: hero → switch to anthropic → send directly (no workspace selection)
    app!(|app: &mut AppView, cx| app.enter_hero(cx));
    app!(|app: &mut AppView, cx| {
        app.composer.update(cx, |_, cx| {
            cx.emit(ComposerEvent::SetModel {
                provider_id: "anthropic".to_string(),
                model_id: "mock-model".to_string(),
            });
        });
    });
    timer!(400).await;
    app!(|app: &mut AppView, cx| {
        app.hero_send(
            "model selection regression v1".to_string(),
            vec![],
            vec![],
            pig_protocol::ExecMode::AutoEdit,
            cx,
        );
    });
    let mut waited = 0u64;
    let v1_id = loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 20_000, "v1 session creation timed out");
        let found = app!(|app: &mut AppView, cx| {
            let Some(sid) = &app.current else { return None };
            if sid == &v2_id || known.contains(sid) {
                return None;
            }
            let streaming = app
                .views
                .get(sid)
                .map(|v| v.thread.read(cx).is_streaming())
                .unwrap_or(true);
            (!streaming).then(|| sid.clone())
        });
        if let Some(id) = found {
            break id;
        }
    };
    let v1 = app!(|app: &mut AppView, _| {
        app.metas
            .iter()
            .find(|m| m.id == v1_id)
            .map(|m| (m.provider_id.clone(), m.model_id.clone()))
    });
    assert_eq!(
        v1,
        Some((
            Some("anthropic".to_string()),
            Some("mock-model".to_string())
        )),
        "hero model switch then direct send: the new session should use the user-chosen model: {v1:?}"
    );
    println!("[selftest] new-session model pick (user choice wins / seed fallback) OK");

    // Variant 3 (the user's real flow): click + on a workspace row
    // (NewTaskInWorkspace, cwd preloaded into hero) → switch model → send.
    // Setup: explicitly create one more mock session and finish a turn — the
    // v1/v2 anthropic sessions have become the workspace's latest seed and would
    // match the user's anthropic choice, so the assertion could not tell "choice
    // kept" from "restored to seed"; the seed must be flushed back to mock
    app!(|app: &mut AppView, _| app.agent.new_session(
        app.cwd.clone(),
        Some("mock".to_string()),
        Some("mock-model".to_string()),
        None,
        None,
        None,
    ));
    let known3: Vec<String> =
        app!(|app: &mut AppView, _| { app.metas.iter().map(|m| m.id.clone()).collect() });
    let seed3_id = loop {
        timer!(200).await;
        let found = app!(|app: &mut AppView, _| {
            let Some(sid) = &app.current else { return None };
            (!known3.contains(sid)).then(|| sid.clone())
        });
        if let Some(id) = found {
            break id;
        }
    };
    app!(|app: &mut AppView, _| {
        app.agent.send_message(
            seed3_id.clone(),
            "v3 setup seed session".to_string(),
            vec![],
            vec![],
            pig_protocol::ExecMode::AutoEdit,
        );
    });
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 30_000, "v3 seed session timed out");
        let done = app!(|app: &mut AppView, cx| {
            let Some(views) = app.views.get(&seed3_id) else {
                return false;
            };
            !views.thread.read(cx).is_streaming() && waited > 1000
        });
        if done {
            break;
        }
    }
    // Confirm the seed took effect: after entering hero the default model should
    // be mock (workspace latest)
    app!(|app: &mut AppView, cx| {
        // The body of the SidebarEvent::NewTaskInWorkspace handler (direct call
        // needs a window)
        app.hero_cwd = Some(app.cwd.clone());
        app.enter_hero(cx);
    });
    timer!(400).await;
    let seeded3 = app!(|app: &mut AppView, _| app.current_model.clone());
    assert_eq!(
        seeded3,
        Some(("mock".to_string(), "mock-model".to_string())),
        "v3 setup: the workspace seed should be mock: {seeded3:?}"
    );
    // Switch to anthropic → send
    app!(|app: &mut AppView, cx| {
        app.composer.update(cx, |_, cx| {
            cx.emit(ComposerEvent::SetModel {
                provider_id: "anthropic".to_string(),
                model_id: "mock-model".to_string(),
            });
        });
    });
    timer!(400).await;
    app!(|app: &mut AppView, cx| {
        app.hero_send(
            "model selection regression v3".to_string(),
            vec![],
            vec![],
            pig_protocol::ExecMode::AutoEdit,
            cx,
        );
    });
    let mut waited = 0u64;
    let v3_id = loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 20_000, "v3 session creation timed out");
        let found = app!(|app: &mut AppView, cx| {
            let Some(sid) = &app.current else { return None };
            if sid == &v1_id || sid == &v2_id || known.contains(sid) || sid == &seed3_id {
                return None;
            }
            let streaming = app
                .views
                .get(sid)
                .map(|v| v.thread.read(cx).is_streaming())
                .unwrap_or(true);
            (!streaming).then(|| sid.clone())
        });
        if let Some(id) = found {
            break id;
        }
    };
    let v3 = app!(|app: &mut AppView, _| {
        app.metas
            .iter()
            .find(|m| m.id == v3_id)
            .map(|m| (m.provider_id.clone(), m.model_id.clone()))
    });
    assert_eq!(
        v3,
        Some((
            Some("anthropic".to_string()),
            Some("mock-model".to_string())
        )),
        "workspace + click -> switch model -> send: the new session should use the user-chosen model: {v3:?}"
    );
    println!("[selftest] model kept after workspace click + new session then model switch OK");

    // Reasoning level landing spot when switching models (priority): model
    // default > inherit (requires the new model to support it) > heuristic when
    // the inherited level is invalid. Selftest config: mock defaults to low,
    // anthropic has no default.
    // 1) Default wins: currently high (mock also supports high) → switching to
    // mock still lands on the default low
    app!(|app: &mut AppView, cx| {
        app.composer.update(cx, |_, cx| {
            cx.emit(ComposerEvent::SetReasoning(Some("high".to_string())));
        });
    });
    timer!(200).await;
    let had_level = app!(|app: &mut AppView, _| app.reasoning_level.clone());
    assert_eq!(
        had_level,
        Some("high".to_string()),
        "setup: the level should be high"
    );
    app!(|app: &mut AppView, cx| {
        app.composer.update(cx, |_, cx| {
            cx.emit(ComposerEvent::SetModel {
                provider_id: "mock".to_string(),
                model_id: "mock-model".to_string(),
            });
        });
    });
    timer!(200).await;
    let (level, meta_level) = app!(|app: &mut AppView, _| {
        let meta_level = app
            .metas
            .iter()
            .find(|m| m.id == v3_id)
            .and_then(|m| m.reasoning_level.clone());
        (app.reasoning_level.clone(), meta_level)
    });
    assert_eq!(
        level,
        Some("low".to_string()),
        "the model default tier should win over an inheritable level: level={level:?}"
    );
    assert_eq!(
        meta_level,
        Some("low".to_string()),
        "the landed level should be written through to meta: meta_level={meta_level:?}"
    );

    // 2) No default configured → inherit: anthropic has no default and max is in
    // its level list → switching keeps max
    app!(|app: &mut AppView, cx| {
        app.composer.update(cx, |_, cx| {
            cx.emit(ComposerEvent::SetReasoning(Some("max".to_string())));
        });
    });
    app!(|app: &mut AppView, cx| {
        app.composer.update(cx, |_, cx| {
            cx.emit(ComposerEvent::SetModel {
                provider_id: "anthropic".to_string(),
                model_id: "mock-model".to_string(),
            });
        });
    });
    timer!(200).await;
    let level = app!(|app: &mut AppView, _| app.reasoning_level.clone());
    assert_eq!(
        level,
        Some("max".to_string()),
        "with no default configured and the level supported, it should be inherited: level={level:?}"
    );
    println!("[selftest] thinking level on model switch (default tier first / inherited) OK");

    // Subagent scenario (A3): foreground Agent card — progress line appears
    // while running, original summary kept after settling; then the background
    // subagent finishes → a synthetic <task-notification> user message arrives
    // (notification card render path)
    let before_current = app!(|app: &mut AppView, _| app.current.clone());
    app!(|app: &mut AppView, _| app.agent.new_session(
        app.cwd.clone(),
        None,
        None,
        None,
        None,
        None
    ));
    let session_c = loop {
        timer!(200).await;
        let current = app!(|app: &mut AppView, _| app.current.clone());
        if let Some(cur) = current
            && Some(&cur) != before_current.as_ref()
        {
            break cur;
        }
    };
    app!(|app: &mut AppView, _| {
        // Pin to the mock (OpenAI format) provider: the earlier model-switch
        // tests left the current selection on anthropic, but the mock's subagent
        // scenario only has the OpenAI-format branch
        app.agent.set_model(
            session_c.clone(),
            "mock".to_string(),
            "mock-model".to_string(),
            None,
        );
        app.agent.send_message(
            session_c.clone(),
            format!("{} GREP", pig_core::mock::SUBAGENT_TRIGGER),
            vec![],
            vec![],
            pig_protocol::ExecMode::AutoEdit,
        );
    });
    let mut saw_progress = false;
    let mut waited = 0u64;
    loop {
        timer!(100).await;
        waited += 100;
        assert!(waited < 60_000, "foreground subagent scenario timed out");
        let state = app!(|app: &mut AppView, cx| {
            let views = app.views.get(&session_c)?;
            let thread = views.thread.read(cx);
            let (_, _, _, tool_output) = thread.debug_last_assistant();
            Some((
                thread.is_streaming(),
                thread.debug_agent_card(),
                tool_output,
            ))
        });
        let Some((streaming, card, tool_output)) = state else {
            continue;
        };
        let Some((summary, live_note, done)) = card else {
            continue;
        };
        saw_progress |= !done && live_note.is_some();
        if done && !streaming {
            assert!(
                saw_progress,
                "progress line (SubagentProgress) should have appeared while running"
            );
            assert!(
                summary.contains("subagent explore"),
                "after settling, the original summary should be kept (not overwritten by progress): {summary}"
            );
            assert!(
                live_note.is_none(),
                "after settling, the progress line should be cleared: {live_note:?}"
            );
            assert!(
                tool_output.contains(pig_core::mock::SUBAGENT_CHILD_DONE),
                "Agent card output should contain the subagent conclusion: {tool_output}"
            );
            // A3c: the SubagentCard event wrote the agent card meta onto the card (agent_id + subtitle)
            let card_meta = app!(|app: &mut AppView, cx| {
                let views = app.views.get(&session_c)?;
                views.thread.read(cx).debug_agent_card_meta()
            });
            let Some((card_agent_id, card_subtitle)) = card_meta else {
                panic!("Agent card should carry agent card meta (SubagentCard event)");
            };
            assert!(
                !card_agent_id.is_empty(),
                "agent card agent_id should be non-empty"
            );
            assert!(
                card_subtitle.contains("explore"),
                "agent card subtitle should contain the profile: {card_subtitle}"
            );
            break;
        }
    }
    println!(
        "[selftest] foreground subagent card (progress line appears / summary kept / cleared on finish / agent card meta) OK"
    );

    // Send a background subagent in the same session: the running receipt
    // settles → after the subagent finishes, core injects the notification and
    // wakes the wrap-up
    app!(|app: &mut AppView, _| {
        app.agent.send_message(
            session_c.clone(),
            format!("{} BG", pig_core::mock::SUBAGENT_TRIGGER),
            vec![],
            vec![],
            pig_protocol::ExecMode::AutoEdit,
        );
    });
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(
            waited < 60_000,
            "background subagent notification timed out"
        );
        let (streaming, has_notification) = app!(|app: &mut AppView, cx| {
            let Some(views) = app.views.get(&session_c) else {
                return (true, false);
            };
            let thread = views.thread.read(cx);
            (thread.is_streaming(), thread.debug_has_task_notification())
        });
        if has_notification && !streaming {
            break;
        }
    }
    println!(
        "[selftest] background subagent done -> task-notification synthetic message arrives OK"
    );

    // A3b: structured meta parsing of the notification open tag (data source for
    // bubble rendering)
    let (agent_id, title, duration_ms, record, result) = {
        let mut waited = 0u64;
        loop {
            timer!(200).await;
            waited += 200;
            assert!(waited < 10_000, "notification meta parsing timed out");
            let found = app!(|app: &mut AppView, cx| {
                let views = app.views.get(&session_c)?;
                views.thread.read(cx).debug_task_notification_meta()
            });
            if let Some(meta) = found {
                break meta;
            }
        }
    };
    assert!(
        title.contains("subagent selftest delegation"),
        "notification title should be the description: {title}"
    );
    // A3c/A3d: duration, context record path and result file path attributes
    assert!(
        duration_ms.is_some(),
        "notification should carry duration_ms"
    );
    let record = record.expect("notification should carry a record path");
    assert!(
        record.contains(".agents/") || record.contains(".agents\\"),
        "record should be the subagent context JSONL: {record}"
    );
    let result_path = result.expect("notification should carry a result path");
    assert!(
        result_path.ends_with(".result.md"),
        "result should be the full result file: {result_path}"
    );
    println!(
        "[selftest] notification bubble structured meta parsing OK ({agent_id} · {title} · {duration_ms:?}ms)"
    );

    // Open the "Subagents" tab via the same path as clicking the notification
    // card → Op::LoadSubagent → SubagentHistory
    app!(|app: &mut AppView, cx| {
        app.open_subagent_tab(session_c.clone(), agent_id.clone(), title.clone(), cx);
    });
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 15_000, "subagent history load timed out");
        let state = app!(|app: &mut AppView, cx| app.debug_subagent_tab(cx));
        if let Some((tab_title, items)) = state {
            assert!(
                items >= 3,
                "subagent conversation should have at least one user/tool/assistant row each: items={items}"
            );
            assert!(
                tab_title.contains("subagent selftest delegation"),
                "tab title should be meta.description: {tab_title}"
            );
            break;
        }
    }
    println!("[selftest] subagent tab (click opens panel + history loads >= 3 rows) OK");

    // A3d: panel live output — send BG in a new session and open the tab via the
    // same agent-card path while the background subagent runs: running indicator
    // appears → SubagentActivity appends incrementally → running disappears
    // after finished
    let before_current = app!(|app: &mut AppView, _| app.current.clone());
    app!(|app: &mut AppView, _| app.agent.new_session(
        app.cwd.clone(),
        None,
        None,
        None,
        None,
        None
    ));
    let session_d = loop {
        timer!(200).await;
        let current = app!(|app: &mut AppView, _| app.current.clone());
        if let Some(cur) = current
            && Some(&cur) != before_current.as_ref()
        {
            break cur;
        }
    };
    app!(|app: &mut AppView, _| {
        app.agent.set_model(
            session_d.clone(),
            "mock".to_string(),
            "mock-model".to_string(),
            None,
        );
        app.agent.send_message(
            session_d.clone(),
            format!("{} BG", pig_core::mock::SUBAGENT_TRIGGER),
            vec![],
            vec![],
            pig_protocol::ExecMode::AutoEdit,
        );
    });
    // Wait for the bg Agent card's card meta (SubagentCard arrives before
    // subagent execution, carrying agent_id)
    let bg_agent_id = {
        let mut waited = 0u64;
        loop {
            timer!(50).await;
            waited += 50;
            assert!(waited < 15_000, "background agent card meta timed out");
            let meta = app!(|app: &mut AppView, cx| {
                let views = app.views.get(&session_d)?;
                views.thread.read(cx).debug_agent_card_meta()
            });
            if let Some((id, _)) = meta {
                break id;
            }
        }
    };
    // Open the tab immediately (the subagent is most likely still running: the
    // mock child side does ≥2 requests × 50ms/chunk)
    app!(|app: &mut AppView, cx| {
        app.open_subagent_tab(
            session_d.clone(),
            bg_agent_id.clone(),
            "subagent selftest delegation".to_string(),
            cx,
        );
    });
    let mut saw_running = false;
    let mut saw_card_running = false;
    let mut saw_following = false;
    let mut saw_agent_chip = false;
    let mut initial_items = None;
    let mut waited = 0u64;
    loop {
        timer!(100).await;
        waited += 100;
        assert!(waited < 30_000, "panel live output timed out");
        let state = app!(|app: &mut AppView, cx| {
            let live = app.debug_subagent_live(&bg_agent_id, cx);
            let scroll = app.debug_subagent_scroll(&bg_agent_id, cx);
            let card = app
                .views
                .get(&session_d)
                .and_then(|views| views.thread.read(cx).debug_agent_card_state());
            let chips = app.composer.read(cx).debug_task_chips();
            (live, scroll, card, chips)
        });
        let (Some((running, items, appends)), scroll_state, card_state, chips) = state else {
            continue;
        };
        saw_running |= running;
        // The following state must not be lost during appended activity (A3f)
        if let Some((following, _)) = scroll_state {
            saw_following |= following;
        }
        // A3e(1): the tool receipt settled long ago (done) but the subagent is
        // still running (!finished) → the card should spin
        if let Some((_, done, finished)) = &card_state {
            saw_card_running |= *done && !*finished;
        }
        // A3g: while BG is running, the "background Agent" chip should appear
        saw_agent_chip |= chips.1;
        if initial_items.is_none() && items > 0 {
            initial_items = Some(items);
        }
        if !running && items > 0 {
            // Wrap-up (finished → running=false plus a full refetch to close out)
            assert!(
                saw_running,
                "should have seen running=true while the subagent ran (running indicator)"
            );
            assert!(
                appends >= 1,
                "there should be incremental SubagentActivity appends"
            );
            assert!(
                Some(items) >= initial_items,
                "after the wrap-up refetch, items should not decrease: {items} < {initial_items:?}"
            );
            break;
        }
    }
    assert!(
        saw_following,
        "following should stay true during appended activity"
    );
    // The card's terminal state: finished lands (no more spinning)
    let (_, card_done, card_finished) = app!(|app: &mut AppView, cx| {
        app.views
            .get(&session_d)
            .and_then(|views| views.thread.read(cx).debug_agent_card_state())
            .expect("session D should have an agent card")
    });
    assert!(
        card_done && card_finished,
        "the card should reach its terminal state after the subagent finishes"
    );
    assert!(
        saw_card_running,
        "should have seen the spinning window where the tool settled but the subagent still ran (done=true && finished=false)"
    );
    println!(
        "[selftest] subagent panel live output (running indicator / active append / cleared on finish) OK"
    );
    println!(
        "[selftest] background agent card run-state machine (spinner window -> terminal state) OK"
    );

    // A3f: panel follow scrolling — following was not lost during appended
    // activity (asserted above); after wrap-up it sits at bottom (panel content
    // may be shorter than one screen: then max_offset=0, at_bottom is always
    // true, and the assertion degrades to a following-state check; following and
    // the floating button in real overflow scenarios rely on manual
    // verification)
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 10_000, "panel stick-to-bottom timed out");
        let scroll = app!(|app: &mut AppView, cx| app.debug_subagent_scroll(&bg_agent_id, cx));
        let Some((following, at_bottom)) = scroll else {
            continue;
        };
        if at_bottom {
            assert!(
                following,
                "should be in the following state while at bottom"
            );
            break;
        }
    }
    println!("[selftest] subagent panel follow-scroll (following kept + stick to bottom) OK");

    // A3g: chips split by type — the "background Agent" chip was seen while BG
    // ran; session D has no Bash task, so the "background Bash" chip must not
    // appear
    assert!(
        saw_agent_chip,
        "the background Agent chip should appear while BG is running"
    );
    let chips = app!(|app: &mut AppView, cx| app.composer.read(cx).debug_task_chips());
    assert_eq!(
        chips,
        (false, true),
        "session D should have only the Agent chip: {chips:?}"
    );
    // Agent task rows carry agent_id (consistent with the agent cards)
    let task_agent_ids = app!(|app: &mut AppView, cx| app.composer.read(cx).debug_agent_task_ids());
    assert!(
        task_agent_ids.iter().any(|id| id == &bg_agent_id),
        "Agent task rows should carry agent_id: {task_agent_ids:?}"
    );
    // Task row click's event path: ComposerEvent::OpenSubagent →
    // open_subagent_tab focuses the tab
    app!(|app: &mut AppView, cx| {
        app.composer.update(cx, |_, cx| {
            cx.emit(ComposerEvent::OpenSubagent {
                agent_id: bg_agent_id.clone(),
                title: "subagent explore: subagent selftest delegation".to_string(),
            });
        });
    });
    let tab_active = app!(|app: &mut AppView, cx| app.debug_subagent_tab(cx).is_some());
    assert!(
        tab_active,
        "there should be an active subagent tab after OpenSubagent"
    );
    // Regression: opening the "background Agent" popup must survive real render
    // frames — a palette_open that misses AgentTasks falls into render_popup's
    // unreachable (a crash hit on a user's machine)
    let popup_open = app!(|app: &mut AppView, cx| {
        app.composer
            .update(cx, |composer, cx| composer.debug_open_agent_tasks_popup(cx))
    });
    assert!(popup_open, "the Agent popup should be open");
    timer!(200).await;
    app!(|app: &mut AppView, cx| {
        app.composer
            .update(cx, |composer, cx| composer.debug_close_popup(cx));
    });
    println!("[selftest] background Agent popup rendering (palette routing regression) OK");
    println!(
        "[selftest] background task chip split (Agent chip visibility / agent_id / row click opens tab) OK"
    );

    // A3e(2): simulated restart reopens session D — the agent card meta is
    // rebuilt from rollout replay (not degraded to a raw output card), and the
    // background agent lands in its terminal state on replay (core re-emits
    // finished, no spinning)
    app!(|app: &mut AppView, cx| app.restart_agent(cx));
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 10_000, "auto-open session after restart timed out");
        let ready = app!(|app: &mut AppView, _| app.current.is_some());
        if ready {
            break;
        }
    }
    app!(|app: &mut AppView, cx| app.switch_session(session_d.clone(), cx));
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 30_000, "agent card replay after restart timed out");
        let state = app!(|app: &mut AppView, cx| {
            let views = app.views.get(&session_d)?;
            let thread = views.thread.read(cx);
            Some((
                thread.debug_agent_card_meta(),
                thread.debug_agent_card_state(),
            ))
        });
        let Some((meta, card_state)) = state else {
            continue;
        };
        let (Some((card_agent, subtitle)), Some((_, done, finished))) = (meta, card_state) else {
            continue;
        };
        assert_eq!(
            card_agent, bg_agent_id,
            "replay should rebuild the same agent card"
        );
        assert!(
            subtitle.contains("explore"),
            "subtitle should contain the profile: {subtitle}"
        );
        assert!(
            done && finished,
            "the replayed background agent card should land directly in its terminal state: done={done} finished={finished}"
        );
        break;
    }
    println!(
        "[selftest] agent card replay rebuild (meta kept + terminal state without spinner) OK"
    );

    // Three-pane minimum width clamping (pure function): sidebar ≥200, right
    // panel ≥280, and ≥480 reserved for the center area
    assert_eq!(
        clamp_dock_widths(1280., 220., 300., true, true),
        (220., 300.),
        "no change within the allowed range"
    );
    assert_eq!(
        clamp_dock_widths(1280., 100., 50., true, true),
        (200., 280.),
        "clamped back below their respective minimums"
    );
    assert_eq!(
        clamp_dock_widths(1280., 900., 300., true, true),
        (500., 300.),
        "left pane capped to reserve 480 for the center; right pane unaffected"
    );
    assert_eq!(
        clamp_dock_widths(1280., 900., 300., true, false),
        (800., 300.),
        "collapsed panes do not consume the budget"
    );
    assert_eq!(
        clamp_dock_widths(960., 400., 400., true, true),
        (200., 280.),
        "at the minimum window width both sides overflow: left yields first, converging to all minimums in one pass"
    );
    assert_eq!(
        clamp_dock_widths(0., 220., 300., true, true),
        (220., 300.),
        "no action on the unmeasured first frame"
    );
    println!("[selftest] three-pane minimum width clamping OK");

    // Global search popup (Ctrl+K quick switcher): rows cover workspaces +
    // sessions, query filters, Enter on a session row switches to it, and a
    // workspace row enters hero preset to that workspace
    {
        use crate::search_popup::SearchRow;
        cx.update_window(window_handle, |_, window, cx| {
            view.update(cx, |app, cx| app.open_search_popup(window, cx));
        })
        .unwrap();
        let rows = app!(|app: &mut AppView, cx| {
            assert!(
                app.search_open,
                "popup should be open after open_search_popup"
            );
            app.search_rows(cx)
        });
        let ws_rows = rows
            .iter()
            .filter(|r| matches!(r, SearchRow::Workspace { .. }))
            .count();
        let session_rows = rows
            .iter()
            .filter(|r| matches!(r, SearchRow::Session { .. }))
            .count();
        assert!(ws_rows >= 1, "workspace rows should be listed");
        assert!(
            session_rows >= 3,
            "session rows should list the sessions created so far (got {session_rows})"
        );
        // No-match query empties the list; ↑/↓ wrapping on the empty list is a
        // no-op (move_search_selection returns early)
        cx.update_window(window_handle, |_, window, cx| {
            view.update(cx, |app, cx| {
                app.search_input.update(cx, |input, cx| {
                    input.set_value("zzz-no-such-thing", window, cx)
                });
            });
        })
        .unwrap();
        let empty = app!(|app: &mut AppView, cx| app.search_rows(cx).len());
        assert_eq!(empty, 0, "a no-match query should empty the result list");
        // Session-row confirm: pick the first session row (newest session) and
        // confirm — current switches and the popup closes with state reset
        cx.update_window(window_handle, |_, window, cx| {
            view.update(cx, |app, cx| {
                app.search_input
                    .update(cx, |input, cx| input.set_value("", window, cx));
            });
        })
        .unwrap();
        let (session_row_ix, session_id) = app!(|app: &mut AppView, cx| {
            let rows = app.search_rows(cx);
            rows.iter()
                .position(|r| matches!(r, SearchRow::Session { .. }))
                .zip(rows.iter().find_map(|r| match r {
                    SearchRow::Session { id } => Some(id.clone()),
                    _ => None,
                }))
                .expect("session rows should be back with an empty query")
        });
        cx.update_window(window_handle, |_, window, cx| {
            view.update(cx, |app, cx| {
                app.search_selected = session_row_ix;
                app.confirm_search(window, cx);
            });
        })
        .unwrap();
        let after_session = app!(|app: &mut AppView, _| {
            (app.search_open, app.search_selected, app.current.clone())
        });
        assert_eq!(after_session, (false, 0, Some(session_id.clone())));
        // Workspace-row confirm: the first row is a workspace (activity
        // order); confirming enters hero preset to it
        let ws_path = app!(|app: &mut AppView, cx| {
            let rows = app.search_rows(cx);
            app.search_open = true; // reopen without focusing (window-free path)
            match rows.first() {
                Some(SearchRow::Workspace { path }) => Some(path.clone()),
                _ => None,
            }
            .expect("the first row with an empty query should be a workspace")
        });
        cx.update_window(window_handle, |_, window, cx| {
            view.update(cx, |app, cx| {
                app.search_selected = 0;
                app.confirm_search(window, cx);
            });
        })
        .unwrap();
        let after_ws = app!(|app: &mut AppView, _| {
            (app.search_open, app.current.clone(), app.hero_cwd.clone())
        });
        assert_eq!(
            after_ws,
            (false, None, Some(std::path::PathBuf::from(ws_path.clone()))),
            "workspace confirm should close the popup and enter hero preset to it"
        );
    }
    println!("[selftest] global search popup (rows/filter/session+workspace confirm) OK");

    println!("SELFTEST PASS");
    std::process::exit(0);
}
