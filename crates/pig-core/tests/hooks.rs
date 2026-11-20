//! Lifecycle hooks integration tests: real POSIX hook scripts driven through
//! the full engine (mock provider), pinning the seven-event contract at its
//! integration points — blocking, input rewriting, context injection, Stop
//! continuation cap, project-file trust, and the PermissionRequest pre-dialog
//! shortcut. On Windows the product's auto hook shell is Git Bash, so the same
//! scripts run there; hosts without Git Bash skip with a visible note instead
//! of claiming coverage. The contract's pure logic (matchers, output parsing,
//! config merge) is covered cross-platform by lib unit tests.

#[cfg(any(unix, windows))]
mod shell_hooks {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use pig_protocol::{ApprovalDecision, Event, ExecMode, HookRunState, Op};
    use pig_provider::mock;

    /// The fixtures below are POSIX scripts; Windows runs them through Git Bash
    /// (the built-in auto hook shell). Without it there is no shell to run
    /// them, so the test reports the skip instead of failing or claiming
    /// coverage for cmd.
    macro_rules! require_git_bash {
        () => {
            #[cfg(windows)]
            if !matches!(
                pig_core::task::windows_shell(),
                pig_core::task::WindowsShell::GitBash(_)
            ) {
                eprintln!("[hooks] skipped: Git Bash is not installed (the Windows hook shell)");
                return;
            }
        };
    }

    struct Fixture {
        dir: PathBuf,
        agent: pig_core::AgentHandle,
        session_id: String,
        /// Events seen while the session was being created (the banner events
        /// fire around SessionConfigured, before any turn starts)
        prelim_events: Vec<Event>,
        /// Raw model request bodies (for model-input assertions)
        requests: Arc<std::sync::Mutex<Vec<String>>>,
    }

    /// Write a hook script into the workspace, return its command form
    fn write_script(dir: &Path, name: &str, body: &str) -> String {
        let path = dir.join(format!("{name}.sh"));
        std::fs::write(&path, body).unwrap();
        // Git Bash reads a Windows backslash path as escapes: forward slashes
        // keep the same command working on both hosts.
        let path = path.display().to_string().replace('\\', "/");
        format!("sh '{path}'")
    }

    /// Temp workspace + mock provider config; `prepare` runs on the created
    /// workspace before the config is written (it writes hook scripts and
    /// returns the `[hooks]` event-section TOML referencing them)
    async fn start_with(name: &str, prepare: impl FnOnce(&Path) -> String) -> Fixture {
        start_with_window(name, 128_000, 8_192, prepare).await
    }

    async fn start_with_window(
        name: &str,
        context_window: u64,
        max_output_tokens: u64,
        prepare: impl FnOnce(&Path) -> String,
    ) -> Fixture {
        let (port, requests) = mock::start_mock_server_with_log();
        let dir =
            std::env::temp_dir().join(format!("pig-core-hooks-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(mock::MOCK_FILE_NAME), mock::MOCK_FILE_CONTENT).unwrap();
        let user_hooks_toml = prepare(&dir);
        let config_path = dir.join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                r#"default_provider = "mock"
default_model = "mock-model"

[[providers]]
id = "mock"
name = "Mock Provider"
base_url = "http://127.0.0.1:{port}/v1"
api_key = "mock-key"
api_format = "OpenAiChat"
enabled = true

[[providers.models]]
id = "mock-model"
context_window = {context_window}
max_output_tokens = {max_output_tokens}

[hooks]
enabled = true
{user_hooks_toml}

[sandbox]
deny = []
credential_dirs = []
"#
            ),
        )
        .unwrap();
        let agent =
            pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), dir.join("data"));
        let (session_id, prelim_events) = create_session(&agent, &dir).await;
        Fixture {
            dir,
            agent,
            session_id,
            prelim_events,
            requests,
        }
    }

    /// NewSession + wait for SessionConfigured, keeping every event seen on
    /// the way (banner events fire around SessionConfigured)
    async fn create_session(agent: &pig_core::AgentHandle, dir: &Path) -> (String, Vec<Event>) {
        agent
            .ops
            .send(Op::NewSession {
                cwd: dir.to_path_buf(),
                provider_id: None,
                model_id: None,
                reasoning_level: None,
                permissions: None,
                plan_enabled: None,
            })
            .await
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut prelim_events = Vec::new();
        loop {
            let Ok(Ok(event)) =
                tokio::time::timeout(Duration::from_secs(1), agent.events.recv()).await
            else {
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for SessionConfigured"
                );
                continue;
            };
            let configured = matches!(&event, Event::SessionConfigured { .. });
            let session_id = match &event {
                Event::SessionConfigured { session_id, .. } => Some(session_id.clone()),
                _ => None,
            };
            prelim_events.push(event);
            if configured {
                return (
                    session_id.expect("configured carries the id"),
                    prelim_events,
                );
            }
        }
    }

    impl Fixture {
        /// Send a message and collect everything up to the turn's end (or a
        /// terminal Error, e.g. a blocked prompt); approval popups are
        /// answered with Allow (tests asserting popup absence inspect the
        /// collected events afterwards)
        async fn turn(&self, content: &str, mode: ExecMode) -> Vec<Event> {
            self.agent
                .ops
                .send(Op::SendMessage {
                    session_id: self.session_id.clone(),
                    content: content.to_string(),
                    files: vec![],
                    images: vec![],
                    permissions: mode.permissions(),
                    delivery: pig_protocol::MessageDelivery::Queue,
                })
                .await
                .unwrap();
            let mut collected = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for the turn to end: {collected:#?}"
                );
                let Ok(Ok(event)) =
                    tokio::time::timeout(Duration::from_secs(2), self.agent.events.recv()).await
                else {
                    continue;
                };
                if let Event::ApprovalRequested { request_id, .. } = &event {
                    self.agent
                        .ops
                        .send(Op::ApprovalReply {
                            request_id: request_id.clone(),
                            decision: ApprovalDecision::Allow,
                            feedback: None,
                        })
                        .await
                        .unwrap();
                }
                let done = matches!(
                    event,
                    Event::TurnComplete { .. } | Event::TurnAborted { .. } | Event::Error { .. }
                );
                collected.push(event);
                if done {
                    return collected;
                }
            }
        }
    }

    /// ToolCallEnd results of one tool, paired with the begin's tool name
    fn tool_outputs(events: &[Event], tool: &str) -> Vec<(String, bool)> {
        let mut names: HashMap<String, String> = HashMap::new();
        for event in events {
            if let Event::ToolCallBegin { item_id, tool, .. } = event {
                names.insert(item_id.clone(), tool.clone());
            }
        }
        events
            .iter()
            .filter_map(|event| match event {
                Event::ToolCallEnd {
                    item_id,
                    output,
                    is_error,
                    ..
                } if names.get(item_id).map(String::as_str) == Some(tool) => {
                    Some((output.clone(), *is_error))
                }
                _ => None,
            })
            .collect()
    }

    fn hook_states(events: &[Event], hook_event: &str) -> Vec<HookRunState> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::HookStatus {
                    hook_event: name,
                    state,
                    ..
                } if name == hook_event => Some(*state),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn pre_tool_use_deny_blocks_write() {
        require_git_bash!();
        let fixture = start_with("deny-write", |dir| {
            let script = write_script(
                dir,
                "deny-write",
                "#!/bin/sh\ncat > /dev/null\necho '{\"continue\": false, \"reason\": \"protected path\"}'",
            );
            format!("[[hooks.pre_tool_use]]\ncommand = \"{script}\"\nmatcher = \"Write\"")
        })
        .await;
        let events = fixture
            .turn(
                &format!("{} full modification chain", mock::SCENARIO_B_TRIGGER),
                ExecMode::Auto,
            )
            .await;
        let writes = tool_outputs(&events, "Write");
        assert!(
            writes.iter().any(|(output, is_error)| *is_error
                && output.contains("Blocked by a PreToolUse hook")
                && output.contains("protected path")),
            "Write should be rejected with the hook reason: {writes:?}"
        );
        assert!(
            !fixture.dir.join(mock::SCENARIO_B_FILE).exists(),
            "the blocked Write must not touch the file"
        );
        // The Bash leg still runs (a hook decision never kills the turn)
        assert!(!tool_outputs(&events, "ExecCommand").is_empty());
        let states = hook_states(&events, "PreToolUse");
        // An exit-0 JSON block decision labels the chain Finished (Blocked is
        // the exit-2 label; see user_prompt_submit_block_stops_turn)
        assert_eq!(states, vec![HookRunState::Running, HookRunState::Finished]);
    }

    /// Handling `Op::TrustProjectHooks` refreshes every project-file surface
    /// consistently: project hooks run, the project permission `allow` applies
    /// live, and the next system prompt carries the newly admitted project
    /// skill/profile listing (the frozen prompt is invalidated on purpose).
    #[tokio::test]
    async fn trust_refresh_activates_project_permissions_skills_and_profiles() {
        require_git_bash!();
        const SKILL_MARKER: &str = "PROJECT_SKILL_UNIQUE_DESCRIPTION_7C1";
        const PROFILE_MARKER: &str = "PROJECT_PROFILE_UNIQUE_DESCRIPTION_7C1";
        let fixture = start_with("trust-refresh", |dir| {
            std::fs::create_dir_all(dir.join(".pigcode/skills/proj")).unwrap();
            std::fs::write(
                dir.join(".pigcode/skills/proj/SKILL.md"),
                format!("---\nname: proj-skill\ndescription: {SKILL_MARKER}\n---\n\nProject skill body.\n"),
            )
            .unwrap();
            std::fs::create_dir_all(dir.join(".pigcode/agents")).unwrap();
            std::fs::write(
                dir.join(".pigcode/agents/proj.md"),
                format!("---\nname: proj-agent\ndescription: {PROFILE_MARKER}\ntools: Read\n---\n\nProject profile body.\n"),
            )
            .unwrap();
            std::fs::create_dir_all(dir.join(".pigcode")).unwrap();
            std::fs::write(
                dir.join(".pigcode/permissions.toml"),
                format!("allow = [\"Write({})\"]\n", mock::SCENARIO_B_FILE),
            )
            .unwrap();
            let hook = write_script(dir, "project-noop", "#!/bin/sh\ncat > /dev/null\nexit 0\n");
            std::fs::write(
                dir.join(".pigcode/hooks.toml"),
                format!(
                    "[[hooks.pre_tool_use]]\ncommand = \"{hook}\"\nmatcher = \"Write|Edit\"\n"
                ),
            )
            .unwrap();
            String::new()
        })
        .await;
        assert!(
            fixture
                .prelim_events
                .iter()
                .any(|event| matches!(event, Event::ProjectHooksPending { .. })),
            "the untrusted project file raises the banner"
        );

        fixture
            .agent
            .ops
            .send(Op::TrustProjectHooks {
                session_id: fixture.session_id.clone(),
            })
            .await
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(
                Instant::now() < deadline,
                "ProjectHooksTrusted was not emitted"
            );
            let Ok(Ok(event)) =
                tokio::time::timeout(Duration::from_secs(2), fixture.agent.events.recv()).await
            else {
                continue;
            };
            if matches!(event, Event::ProjectHooksTrusted { .. }) {
                break;
            }
        }

        // Trusted (live session): the next turn runs approval-free for the
        // project-allowed Write, the project hook fires, and the rebuilt prompt
        // carries the project skill/profile listing. (The workspace has no
        // prior tool results in this turn, so the mock still emits the Write
        // chain.)
        let after = fixture
            .turn(
                &format!("{} trusted", mock::SCENARIO_B_TRIGGER),
                ExecMode::ReadOnly,
            )
            .await;
        let write_approvals_after = after
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    Event::ApprovalRequested { tool, .. } if tool == "Write"
                )
            })
            .count();
        assert_eq!(
            write_approvals_after, 0,
            "the trusted project allow applies live: {after:#?}"
        );
        assert!(
            after
                .iter()
                .filter(|event| matches!(
                    event,
                    Event::HookRunFinished { hook_event, .. } if hook_event == "PreToolUse"
                ))
                .count()
                >= 2,
            "the project hook runs for both trusted writes: {after:#?}"
        );
        let bodies = fixture.requests.lock().unwrap().clone();
        let trusted_body = bodies.last().cloned().unwrap_or_default();
        assert!(
            trusted_body.contains(SKILL_MARKER),
            "the next request carries the project skill listing"
        );
        assert!(
            trusted_body.contains(PROFILE_MARKER),
            "the next request carries the project agent profile listing"
        );
    }

    /// Revoking via a config save uses the same trust boundary as a grant:
    /// project skills leave the next prompt, the MCP cache is dropped and the
    /// trust banner comes back without reopening the session.
    #[tokio::test]
    async fn revoking_workspace_trust_refreshes_project_surfaces_and_reopens_the_banner() {
        require_git_bash!();
        const SKILL_MARKER: &str = "PROJECT_SKILL_REVOKE_BOUNDARY_9F2";
        let fixture = start_with("trust-revoke", |dir| {
            std::fs::create_dir_all(dir.join(".pigcode/skills/proj")).unwrap();
            std::fs::write(
                dir.join(".pigcode/skills/proj/SKILL.md"),
                format!(
                    "---\nname: proj-skill\ndescription: {SKILL_MARKER}\n---\n\nProject skill body.\n"
                ),
            )
            .unwrap();
            std::fs::write(
                dir.join(".pigcode/hooks.toml"),
                "[[hooks.pre_tool_use]]\ncommand = \"true\"\nmatcher = \"Write\"\n",
            )
            .unwrap();
            String::new()
        })
        .await;

        fixture
            .agent
            .ops
            .send(Op::TrustProjectHooks {
                session_id: fixture.session_id.clone(),
            })
            .await
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(
                Instant::now() < deadline,
                "ProjectHooksTrusted was not emitted"
            );
            let Ok(Ok(event)) =
                tokio::time::timeout(Duration::from_secs(2), fixture.agent.events.recv()).await
            else {
                continue;
            };
            if matches!(event, Event::ProjectHooksTrusted { .. }) {
                break;
            }
        }
        fixture.turn("trusted hello", ExecMode::Auto).await;
        let trusted_body = fixture.requests.lock().unwrap().last().cloned().unwrap();
        assert!(
            trusted_body.contains(SKILL_MARKER),
            "the trusted prompt carries the project skill"
        );

        fixture.agent.ops.send(Op::GetConfig).await.unwrap();
        let mut config = loop {
            let Ok(Ok(event)) =
                tokio::time::timeout(Duration::from_secs(2), fixture.agent.events.recv()).await
            else {
                continue;
            };
            if let Event::ConfigSnapshot { config } = event {
                break config;
            }
        };
        config.hooks.trusted_projects.clear();
        fixture
            .agent
            .ops
            .send(Op::SaveConfig { config })
            .await
            .unwrap();
        let mut banner = None;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(Instant::now() < deadline, "the revoke save did not finish");
            let Ok(Ok(event)) =
                tokio::time::timeout(Duration::from_secs(2), fixture.agent.events.recv()).await
            else {
                continue;
            };
            match event {
                Event::ProjectHooksPending { path, count, .. } => banner = Some((path, count)),
                Event::ConfigSnapshot { .. } => break,
                _ => {}
            }
        }
        let (_, count) = banner.expect("a revoke re-opens the trust banner");
        assert!(count >= 2, "skill and hooks are counted: {count}");

        fixture.turn("revoked hello", ExecMode::Auto).await;
        let revoked_body = fixture.requests.lock().unwrap().last().cloned().unwrap();
        assert!(
            !revoked_body.contains(SKILL_MARKER),
            "the project skill leaves the prompt after revocation"
        );
    }

    #[tokio::test]
    async fn parallel_readonly_group_observes_pre_tool_use_hooks() {
        require_git_bash!();
        let fixture = start_with("parallel-hook-deny", |dir| {
            let script = write_script(
                dir,
                "deny-read",
                "#!/bin/sh\ncat > /dev/null\necho '{\"continue\": false, \"reason\": \"read hooks denied\"}'",
            );
            format!("[[hooks.pre_tool_use]]\ncommand = \"{script}\"\nmatcher = \"Read\"")
        })
        .await;
        let events = fixture
            .turn(
                &format!("{} parallel", mock::PREFIX_PARALLEL_SCENARIO),
                ExecMode::Auto,
            )
            .await;
        let reads = tool_outputs(&events, "Read");
        assert_eq!(
            reads.len(),
            2,
            "both Read calls must settle through the gate: {reads:?}"
        );
        assert!(
            reads.iter().all(|(output, is_error)| *is_error
                && output.contains("Blocked by a PreToolUse hook")
                && output.contains("read hooks denied")),
            "the hook decision must apply to every group member: {reads:?}"
        );
        let finished = hook_states(&events, "PreToolUse");
        assert_eq!(
            finished
                .iter()
                .filter(|state| matches!(state, HookRunState::Running))
                .count(),
            2,
            "each call runs its own hook chain: {finished:?}"
        );
    }

    #[tokio::test]
    async fn pre_tool_use_updated_input_rewrites_write() {
        require_git_bash!();
        let fixture = start_with("rewrite-write", |dir| {
            let script = write_script(
                dir,
                "rewrite-write",
                "#!/bin/sh\ncat > /dev/null\necho '{\"hookSpecificOutput\": {\"hookEventName\": \"PreToolUse\", \"updatedInput\": {\"path\": \"src/hello.txt\", \"content\": \"HOOK_WROTE_THIS\"}}}'",
            );
            format!("[[hooks.pre_tool_use]]\ncommand = \"{script}\"\nmatcher = \"Write\"")
        })
        .await;
        let events = fixture
            .turn(
                &format!("{} full modification chain", mock::SCENARIO_B_TRIGGER),
                ExecMode::Auto,
            )
            .await;
        let file = fixture.dir.join(mock::SCENARIO_B_FILE);
        let content = std::fs::read_to_string(&file)
            .unwrap_or_else(|_| panic!("the rewritten Write should still run: {events:#?}"));
        assert_eq!(content, "HOOK_WROTE_THIS");
    }

    #[tokio::test]
    async fn user_prompt_submit_block_stops_turn() {
        require_git_bash!();
        let fixture = start_with("block-prompt", |dir| {
            let script = write_script(
                dir,
                "block-prompt",
                "#!/bin/sh\ncat > /dev/null\necho 'not on my watch' >&2\nexit 2",
            );
            format!("[[hooks.user_prompt_submit]]\ncommand = \"{script}\"")
        })
        .await;
        let events = fixture.turn("hello there", ExecMode::Auto).await;
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Event::TurnStarted { .. })),
            "the turn must not start: {events:?}"
        );
        let error = events
            .iter()
            .find_map(|event| match event {
                Event::Error { error, .. } => Some(error.clone()),
                _ => None,
            })
            .expect("a HookBlocked error event");
        assert!(
            matches!(error.semantic_error(), pig_protocol::CoreError::HookBlocked { detail } if detail.contains("not on my watch")),
            "{error:?}"
        );
        assert!(
            fixture.requests.lock().unwrap().is_empty(),
            "zero model requests may happen for a blocked prompt"
        );
    }

    #[tokio::test]
    async fn post_tool_use_context_appended_to_result() {
        require_git_bash!();
        let fixture = start_with("post-context", |dir| {
            let script = write_script(
                dir,
                "post-context",
                "#!/bin/sh\ncat > /dev/null\necho '{\"additionalContext\": \"POST_HOOK_MARKER_7788\"}'",
            );
            format!(
                "[[hooks.post_tool_use]]\ncommand = \"{script}\"\nmatcher = \"Write\""
            )
        })
        .await;
        // Write goes through the gated path (parallel read-only tools like
        // Read bypass the gate entirely — hooks do not fire for them)
        let events = fixture
            .turn(
                &format!("{} full modification chain", mock::SCENARIO_B_TRIGGER),
                ExecMode::Auto,
            )
            .await;
        let writes = tool_outputs(&events, "Write");
        assert!(
            writes
                .iter()
                .any(|(output, _)| output.contains("POST_HOOK_MARKER_7788")),
            "the Write result should carry the hook context: {writes:?}"
        );
    }

    #[tokio::test]
    async fn stop_hook_continuation_capped_at_three() {
        require_git_bash!();
        let fixture = start_with("stop-continue", |dir| {
            let script = write_script(
                dir,
                "stop-continue",
                "#!/bin/sh\ncat > /dev/null\necho '{\"continue\": true, \"additionalContext\": \"KEEP_GOING_MARKER\"}'",
            );
            format!("[[hooks.stop]]\ncommand = \"{script}\"")
        })
        .await;
        let events = fixture.turn("hello there", ExecMode::Auto).await;
        let text_count = events
            .iter()
            .filter(|event| matches!(event, Event::TextDone { .. }))
            .count();
        assert_eq!(
            text_count, 4,
            "the initial reply plus at most 3 continuations (events: {events:#?})"
        );
        // Each extra round carries the hook context to the model
        let requests = fixture.requests.lock().unwrap().clone();
        assert!(
            requests.len() >= 4,
            "expected the initial Read round plus continuation rounds: {}",
            requests.len()
        );
        assert!(
            requests
                .iter()
                .filter(|body| body.contains("KEEP_GOING_MARKER"))
                .count()
                >= 3,
            "each continuation round carries the hook context"
        );
        // The Stop chain runs at every boundary (4 rounds), even though the
        // 4th no longer continues
        let states = hook_states(&events, "Stop");
        assert_eq!(
            states
                .iter()
                .filter(|state| **state == HookRunState::Running)
                .count(),
            4,
            "{states:?}"
        );
    }

    /// A Stop-hook continuation can arrive with the context already over the
    /// watermark. The preflight compaction must keep the hook instruction (the
    /// pending request) verbatim; dropping it would make the model answer the
    /// previous request again.
    #[tokio::test]
    async fn stop_hook_context_survives_preflight_compaction() {
        require_git_bash!();
        let fixture = start_with_window("stop-compact", 15_000, 1_000, |dir| {
            let context = format!("KEEP_GOING_MARKER {}", "x".repeat(8_000));
            let script = write_script(
                dir,
                "stop-compact",
                &format!(
                    "#!/bin/sh\ncat > /dev/null\necho '{{\"continue\": true, \"additionalContext\": \"{context}\"}}'"
                ),
            );
            format!("[[hooks.stop]]\ncommand = \"{script}\"")
        })
        .await;
        let events = fixture.turn("hello there", ExecMode::Auto).await;
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::TurnComplete { .. })),
            "{events:#?}"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Event::TurnStarted { .. }))
                .count(),
            1,
            "compaction must not restart the turn: {events:#?}"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Event::UserMessage { .. }))
                .count(),
            1,
            "compaction must not duplicate the user message: {events:#?}"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::ContextCompacted { .. })),
            "the tiny window forces a preflight compaction: {events:#?}"
        );
        let requests = fixture.requests.lock().unwrap().clone();
        let compacted: Vec<&String> = requests
            .iter()
            .filter(|body| {
                body.contains("\"stream\":true") && body.contains("Earlier context compacted")
            })
            .collect();
        assert!(
            !compacted.is_empty(),
            "a post-compaction request exists: {requests:#?}"
        );
        for body in compacted {
            assert!(
                body.contains("KEEP_GOING_MARKER"),
                "the Stop-hook instruction must survive compaction: {body}"
            );
            assert!(
                body.contains("hello there"),
                "the original request stays preserved: {body}"
            );
        }
        assert!(
            requests.iter().any(|body| body.contains("\"stream\":true")
                && !body.contains("Earlier context compacted")),
            "the first round runs before any compaction: {requests:#?}"
        );
    }

    #[tokio::test]
    async fn session_start_context_reaches_first_request() {
        require_git_bash!();
        let fixture = start_with("session-start", |dir| {
            let script = write_script(
                dir,
                "session-start",
                "#!/bin/sh\ncat > /dev/null\necho '{\"additionalContext\": \"SESSION_START_MARKER_42\"}'",
            );
            format!("[[hooks.session_start]]\ncommand = \"{script}\"")
        })
        .await;
        let events = fixture.turn("hello there", ExecMode::Auto).await;
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::TurnComplete { .. })),
            "the turn should finish normally"
        );
        let requests = fixture.requests.lock().unwrap().clone();
        // The title-generation sidecar races ahead of the turn's requests;
        // assert on the first conversation request
        let first_conversation = requests
            .iter()
            .find(|body| !body.contains("Generate a title for this coding session"))
            .expect("a conversation request");
        assert!(
            first_conversation.contains("SESSION_START_MARKER_42"),
            "the first conversation request must carry the SessionStart context"
        );
        // The user message itself stays clean (the context rides as its own
        // history record)
        assert!(!events.iter().any(|event| matches!(
            event,
            Event::UserMessage { text, .. } if text.contains("SESSION_START_MARKER_42")
        )));
    }

    /// Per-entry shell selection: a command hook can pin Windows PowerShell
    /// instead of the Git Bash auto chain. The SessionStart context proves the
    /// script ran (PowerShell startup is slower than sh, so the default 60s
    /// timeout still applies).
    #[cfg(windows)]
    #[tokio::test]
    async fn a_hook_entry_can_select_powershell() {
        if !pig_core::task::available_hook_shells().contains(&pig_protocol::HookShell::PowerShell) {
            eprintln!("[hooks] skipped: Windows PowerShell is not installed");
            return;
        }
        let fixture = start_with("powershell-shell", |dir| {
            let script = dir.join("pwsh-hook.ps1");
            std::fs::write(
                &script,
                "$null = [Console]::In.ReadToEnd()\nWrite-Output '{\"additionalContext\": \"PWSH_SHELL_MARKER\"}'\n",
            )
            .unwrap();
            let script = script.display().to_string().replace('\\', "/");
            format!(
                "[[hooks.session_start]]\ncommand = \"& '{script}'\"\nshell = \"powershell\""
            )
        })
        .await;
        let events = fixture.turn("hello there", ExecMode::Auto).await;
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::TurnComplete { .. })),
            "the turn should finish normally: {events:?}"
        );
        let requests = fixture.requests.lock().unwrap().clone();
        let first = requests
            .iter()
            .find(|body| !body.contains("Generate a title for this coding session"))
            .expect("a conversation request");
        assert!(
            first.contains("PWSH_SHELL_MARKER"),
            "the PowerShell hook's context must reach the request: {first}"
        );
    }

    /// A real user message plus its UserPromptSubmit hook context are one
    /// consecutive user run; manual compaction must keep the real user, never
    /// degrade the newest segment to the hook context alone.
    #[tokio::test]
    async fn compact_keeps_real_user_with_its_hook_context() {
        require_git_bash!();
        let fixture = start_with("compact-user-hook", |dir| {
            let script = write_script(
                dir,
                "compact-user-hook",
                "#!/bin/sh\ncat > /dev/null\necho '{\"additionalContext\": \"HOOK_CONTEXT_MARKER\"}'",
            );
            format!("[[hooks.user_prompt_submit]]\ncommand = \"{script}\"")
        })
        .await;
        assert!(
            fixture
                .turn("old user turn", ExecMode::Auto)
                .await
                .iter()
                .any(|event| matches!(event, Event::TurnComplete { .. }))
        );
        assert!(
            fixture
                .turn("current real user", ExecMode::Auto)
                .await
                .iter()
                .any(|event| matches!(event, Event::TurnComplete { .. }))
        );

        fixture
            .agent
            .ops
            .send(Op::Compact {
                session_id: fixture.session_id.clone(),
                instruction: None,
            })
            .await
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for ContextCompacted"
            );
            let Ok(Ok(event)) =
                tokio::time::timeout(Duration::from_secs(2), fixture.agent.events.recv()).await
            else {
                continue;
            };
            if matches!(event, Event::ContextCompacted { .. }) {
                break;
            }
            assert!(
                !matches!(event, Event::Error { .. }),
                "compact failed: {event:?}"
            );
        }

        assert!(
            fixture
                .turn("after compact", ExecMode::Auto)
                .await
                .iter()
                .any(|event| matches!(event, Event::TurnComplete { .. }))
        );
        let requests = fixture.requests.lock().unwrap().clone();
        let request = requests
            .iter()
            .rev()
            .find(|body| body.contains("after compact"))
            .expect("post-compact request");
        assert!(
            request.contains("current real user"),
            "the current real user survives compaction: {request}"
        );
        assert!(
            request.contains("HOOK_CONTEXT_MARKER"),
            "its hook context also survives: {request}"
        );
        assert!(
            !request.contains("old user turn"),
            "the older segment is compacted away: {request}"
        );
    }

    #[tokio::test]
    async fn project_hooks_skip_until_trusted() {
        require_git_bash!();
        let fixture = start_with("project-trust", |dir| {
            // The hook snapshot loads at Session::create, so the project file
            // must exist before the session starts
            let script = write_script(dir, "project-trust", "#!/bin/sh\ncat >> hooklog.txt");
            std::fs::create_dir_all(dir.join(".pigcode")).unwrap();
            std::fs::write(
                dir.join(".pigcode").join("hooks.toml"),
                format!("[[hooks.pre_tool_use]]\ncommand = \"{script}\"\nmatcher = \"Write\"\n"),
            )
            .unwrap();
            String::new()
        })
        .await;

        // Untrusted: skipped (fail-closed) + pending banner (fired at
        // session create, before any turn)
        assert!(
            fixture.prelim_events.iter().any(|event| matches!(
                event,
                Event::ProjectHooksPending { count, .. } if *count == 1
            )),
            "the pending banner event should fire once: {:?}",
            fixture.prelim_events
        );
        fixture
            .turn(
                &format!("{} full modification chain", mock::SCENARIO_B_TRIGGER),
                ExecMode::Auto,
            )
            .await;
        assert!(
            !fixture.dir.join("hooklog.txt").exists(),
            "untrusted project hooks must not run"
        );

        // Trust: persisted + active for the live session
        fixture
            .agent
            .ops
            .send(Op::TrustProjectHooks {
                session_id: fixture.session_id.clone(),
            })
            .await
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut trusted = false;
        while Instant::now() < deadline {
            let Ok(Ok(event)) =
                tokio::time::timeout(Duration::from_secs(1), fixture.agent.events.recv()).await
            else {
                continue;
            };
            if matches!(event, Event::ProjectHooksTrusted { .. }) {
                trusted = true;
                break;
            }
        }
        assert!(trusted, "TrustProjectHooks should be acknowledged");

        // The persisted trust applies to a fresh session in the same
        // workspace (a fresh history also lets the mock emit tool calls
        // again); no second banner may appear
        let (second_id, second_prelim) = create_session(&fixture.agent, &fixture.dir).await;
        assert!(
            !second_prelim
                .iter()
                .any(|event| matches!(event, Event::ProjectHooksPending { .. })),
            "a trusted workspace banners no more: {second_prelim:?}"
        );
        let mut fixture = fixture;
        fixture.session_id = second_id;
        let turn_events = fixture
            .turn(
                &format!("{} full modification chain again", mock::SCENARIO_B_TRIGGER),
                ExecMode::Auto,
            )
            .await;
        assert!(
            turn_events.iter().any(|event| matches!(event,
                    Event::HookStatus { hook_event, state: HookRunState::Finished, .. }
                        if hook_event == "PreToolUse")),
            "the trusted project hook should now run: {turn_events:?}"
        );
    }

    #[tokio::test]
    async fn permission_request_hook_allow_skips_dialog() {
        require_git_bash!();
        let fixture = start_with("perm-allow", |dir| {
            let script = write_script(
                dir,
                "perm-allow",
                "#!/bin/sh\ncat > /dev/null\necho '{\"decision\": \"approve\"}'",
            );
            format!("[[hooks.permission_request]]\ncommand = \"{script}\"\nmatcher = \"Write\"")
        })
        .await;
        // ReadOnly: the Write leg would normally pop an approval
        // dialog; the hook answers it instead
        let events = fixture
            .turn(
                &format!("{} full modification chain", mock::SCENARIO_B_TRIGGER),
                ExecMode::ReadOnly,
            )
            .await;
        assert!(
            !events.iter().any(|event| matches!(
                event,
                Event::ApprovalRequested { tool, .. } if tool == "Write"
            )),
            "the hook allow must answer the Write approval without a dialog: {events:?}"
        );
        // Edit still pops normally (the matcher only covers Write). Bash no
        // longer prompts in ReadOnly at all: it runs under the read-only
        // sandbox profile instead.
        assert!(
            events.iter().any(|event| matches!(
                event,
                Event::ApprovalRequested { tool, .. } if tool == "Edit"
            )),
            "unmatched tools keep their normal dialog: {events:?}"
        );
        assert!(
            !events.iter().any(|event| matches!(
                event,
                Event::ApprovalRequested { tool, .. } if tool == "ExecCommand"
            )),
            "ReadOnly Bash runs sandboxed without a routine dialog: {events:?}"
        );
        assert!(
            fixture.dir.join(mock::SCENARIO_B_FILE).exists(),
            "the Write should have executed after the hook approval"
        );
    }

    /// kind=process: the entry runs as argv without a shell; the stdin/stdout
    /// contract is identical to shell hooks
    #[tokio::test]
    async fn process_kind_argv_hook_blocks_write() {
        require_git_bash!();
        let fixture = start_with("argv-deny", |_dir| {
            #[cfg(windows)]
            let program = match pig_core::task::windows_shell() {
                pig_core::task::WindowsShell::GitBash(bash) => {
                    bash.display().to_string().replace('\\', "/")
                }
                // require_git_bash! has already skipped this host.
                pig_core::task::WindowsShell::Cmd => "sh".to_string(),
            };
            #[cfg(unix)]
            let program = "sh".to_string();
            r#"[[hooks.pre_tool_use]]
type = "process"
command = "__PROGRAM__"
args = ["-c", "cat > /dev/null; echo '{\"continue\": false, \"reason\": \"argv blocked\"}'"]
matcher = "Write"
"#
            .replace("__PROGRAM__", &program)
        })
        .await;
        let events = fixture
            .turn(
                &format!("{} full modification chain", mock::SCENARIO_B_TRIGGER),
                ExecMode::Auto,
            )
            .await;
        let writes = tool_outputs(&events, "Write");
        assert!(
            writes.iter().any(|(output, is_error)| *is_error
                && output.contains("Blocked by a PreToolUse hook")
                && output.contains("argv blocked")),
            "the argv hook should reject Write with its reason: {writes:?}"
        );
        assert!(
            !fixture.dir.join(mock::SCENARIO_B_FILE).exists(),
            "the blocked Write must not touch the file"
        );
        let states = hook_states(&events, "PreToolUse");
        assert_eq!(
            states,
            vec![HookRunState::Running, HookRunState::Finished],
            "exit-0 JSON block = Finished chain: {states:?}"
        );
    }

    /// HookRunFinished rows: one per entry with state + sanitized reason
    #[tokio::test]
    async fn hook_run_rows_report_per_entry_and_redact_secrets() {
        require_git_bash!();
        let fixture = start_with("rows", |dir| {
            let ok = write_script(dir, "rows-ok", "#!/bin/sh\ncat > /dev/null");
            let deny = write_script(
                dir,
                "rows-deny",
                "#!/bin/sh\ncat > /dev/null\necho 'denied: token=sekret123' >&2\nexit 2",
            );
            format!(
                "[[hooks.pre_tool_use]]\ncommand = \"{ok}\"\nmatcher = \"Write\"\n\
                 [[hooks.pre_tool_use]]\ncommand = \"{deny}\"\nmatcher = \"Write\"\nstatus_message = \"guard\"\n"
            )
        })
        .await;
        let events = fixture
            .turn(
                &format!("{} full modification chain", mock::SCENARIO_B_TRIGGER),
                ExecMode::Auto,
            )
            .await;
        let rows: Vec<(String, HookRunState, Option<String>)> = events
            .iter()
            .filter_map(|event| match event {
                Event::HookRunFinished {
                    hook_event,
                    state,
                    reason,
                    ..
                } if hook_event == "PreToolUse" => {
                    Some((hook_event.clone(), *state, reason.clone()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(rows.len(), 2, "one row per entry: {rows:?}");
        assert_eq!(rows[0].1, HookRunState::Finished);
        assert_eq!(rows[1].1, HookRunState::Blocked);
        let reason = rows[1].2.as_deref().unwrap_or_default();
        assert!(
            reason.contains("token=***") && !reason.contains("sekret123"),
            "the block reason is sanitized: {reason}"
        );
    }

    /// transcript_path: the payload carries a per-run JSONL snapshot of the
    /// conversation (the pending prompt included), live while the hook runs
    /// and deleted afterwards
    #[tokio::test]
    async fn user_prompt_submit_carries_a_live_transcript() {
        require_git_bash!();
        let fixture = start_with("transcript", |dir| {
            let script = write_script(
                dir,
                "transcript",
                "#!/bin/sh\npayload=$(cat)\npath=$(printf '%s' \"$payload\" | sed -n 's/.*\"transcript_path\":\"\\([^\"]*\\)\".*/\\1/p')\nprintf '%s' \"$path\" > transcript-path.txt\nif [ -n \"$path\" ] && [ -f \"$path\" ] && grep -q \"hello transcript\" \"$path\"; then\n  echo '{\"continue\": true}'\nelse\n  echo 'transcript missing or incomplete' >&2\n  exit 2\nfi",
            );
            format!("[[hooks.user_prompt_submit]]\ncommand = \"{script}\"")
        })
        .await;
        let events = fixture.turn("hello transcript", ExecMode::Auto).await;
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::TurnComplete { .. })),
            "the hook sees the transcript (prompt included) and lets the turn run: {events:?}"
        );
        // The snapshot is deleted when the run ends
        let recorded = std::fs::read_to_string(fixture.dir.join("transcript-path.txt")).unwrap();
        assert!(recorded.contains("pig-hook-transcript-"), "{recorded}");
        assert!(
            !std::path::Path::new(&recorded).exists(),
            "the transcript file is cleaned up after the run: {recorded}"
        );
    }
}
