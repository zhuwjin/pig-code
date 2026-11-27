mod common;

use std::path::PathBuf;
use std::time::Duration;

use pig_core::bots::check_tool_capability;
use pig_core::memory;
use pig_core::store::Store;
use pig_protocol::{BotCapabilities, BotProfile, Event, ExecMode, MessageDelivery, Op};

async fn create_bot_session(
    agent: &pig_core::AgentHandle,
    cwd: PathBuf,
    bot_id: Option<&str>,
    global: bool,
) -> (String, Option<String>, bool, PathBuf) {
    agent
        .ops
        .send(Op::NewSession {
            cwd,
            provider_id: None,
            model_id: None,
            reasoning_level: None,
            permissions: Some(ExecMode::Auto.permissions()),
            plan_enabled: Some(false),
            bot_id: bot_id.map(str::to_string),
            global,
        })
        .await
        .expect("send NewSession");
    let events = common::recv_until(&agent.events, Duration::from_secs(10), |event| {
        matches!(event, Event::SessionConfigured { .. })
    })
    .await;
    events
        .into_iter()
        .find_map(|event| match event {
            Event::SessionConfigured {
                session_id,
                bot_id,
                global,
                cwd,
                ..
            } => Some((session_id, bot_id, global, cwd)),
            _ => None,
        })
        .expect("SessionConfigured event")
}

#[test]
fn bot_crud_builtins_and_sqlite_persistence() {
    let dir = std::env::temp_dir().join(format!(
        "pig-bots-crud-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");

    {
        let store = Store::open(&dir).expect("open store");
        let bots = store.list_bots();
        assert!(bots.len() >= 3, "expected built-in bots seeded: {bots:?}");
        let coder = store.get_bot("pig-coder").expect("pig-coder seeded");
        let arch = store.get_bot("pig-architect").expect("pig-architect seeded");
        let orch = store
            .get_bot("pig-orchestrator")
            .expect("pig-orchestrator seeded");
        assert!(!coder.capabilities.control_plane);
        assert!(!arch.capabilities.control_plane);
        assert!(orch.capabilities.control_plane);
        assert!(
            store.delete_bot("pig-coder").is_err(),
            "built-in bots must not be deletable"
        );

        let custom = BotProfile {
            bot_id: "sec-reviewer".to_string(),
            name: "Security Reviewer".to_string(),
            avatar: None,
            description: "Audits auth and boundaries".to_string(),
            system_prompt: "Always verify trust boundaries.".to_string(),
            provider_id: None,
            model_id: None,
            reasoning_level: None,
            capabilities: BotCapabilities::coding(),
            builtin: false,
            created_at_ms: 1000,
            updated_at_ms: 1000,
        };
        let saved = store.upsert_bot(&custom).expect("upsert custom bot");
        assert_eq!(saved.bot_id, "sec-reviewer");
        assert!(!saved.capabilities.control_plane);
    }

    // Reopen SQLite and verify persistence + deletion tombstone.
    {
        let store = Store::open(&dir).expect("reopen store");
        let loaded = store.get_bot("sec-reviewer").expect("custom bot persisted");
        assert_eq!(loaded.name, "Security Reviewer");
        assert!(store.delete_bot("sec-reviewer").expect("delete custom bot"));
        assert!(store.get_bot("sec-reviewer").is_none());
        assert!(
            store
                .bot_deletions()
                .iter()
                .any(|t| t.bot_id == "sec-reviewer"),
            "deleting a custom bot must write a sync tombstone"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

fn last_logged_json(log: &std::sync::Arc<std::sync::Mutex<Vec<String>>>) -> serde_json::Value {
    let requests = log.lock().expect("log lock");
    let raw = requests.last().expect("logged request");
    serde_json::from_str(raw).expect("parse logged request json")
}

#[tokio::test]
async fn same_bot_across_workspaces_keeps_personal_memory_and_survives_compaction() {
    let (config_path, root_dir, data_dir, log) = common::setup_with_log("bot-cross-workspace");
    let ws_a = root_dir.join("workspace-a");
    let ws_b = root_dir.join("workspace-b");
    std::fs::create_dir_all(&ws_a).expect("ws_a");
    std::fs::create_dir_all(&ws_b).expect("ws_b");

    // Seed workspace memories for A and B separately.
    memory::ensure_initialized(&data_dir, &ws_a);
    memory::ensure_initialized(&data_dir, &ws_b);
    std::fs::write(
        memory::workspace_memory_dir(&data_dir, &ws_a).join("MEMORY.md"),
        "# Workspace A\n- WS_A_UNIQUE_FACT\n",
    )
    .expect("write ws_a memory");
    std::fs::write(
        memory::workspace_memory_dir(&data_dir, &ws_b).join("MEMORY.md"),
        "# Workspace B\n- WS_B_UNIQUE_FACT\n",
    )
    .expect("write ws_b memory");

    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), ws_a.clone(), data_dir.clone());

    let mentor = BotProfile {
        bot_id: "mentor-bot".to_string(),
        name: "Mentor Bot".to_string(),
        avatar: None,
        description: "Cross-workspace coding mentor".to_string(),
        system_prompt: "MENTOR_PERSONA_DIRECTIVE_XYZ".to_string(),
        provider_id: None,
        model_id: None,
        reasoning_level: None,
        capabilities: BotCapabilities::coding(),
        builtin: false,
        created_at_ms: 1000,
        updated_at_ms: 1000,
    };
    agent
        .ops
        .send(Op::UpsertBot { bot: mentor })
        .await
        .expect("send UpsertBot");
    common::recv_until(&agent.events, Duration::from_secs(5), |event| {
        matches!(event, Event::BotUpdated { bot } if bot.bot_id == "mentor-bot")
    })
    .await;

    // Write Bot personal long-term memory.
    let bot_mem_file = memory::bot_memory_dir(&data_dir, "mentor-bot").join("MEMORY.md");
    std::fs::write(
        &bot_mem_file,
        "# Bot Personal Memory\n- PERSONAL_LONG_TERM_TOKEN_777\n",
    )
    .expect("write bot personal memory");

    // Session 1 in Workspace A.
    let (sid_a, bot_a, global_a, _) =
        create_bot_session(&agent, ws_a.clone(), Some("mentor-bot"), false).await;
    assert_eq!(bot_a.as_deref(), Some("mentor-bot"));
    assert!(!global_a);

    agent
        .ops
        .send(Op::SendMessage {
            session_id: sid_a.clone(),
            content: "Hello in workspace A".to_string(),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: MessageDelivery::Queue,
        })
        .await
        .expect("send msg A");
    common::recv_until(&agent.events, Duration::from_secs(15), |event| {
        matches!(event, Event::TurnComplete { session_id, .. } if session_id == &sid_a)
    })
    .await;

    let req_a_system = last_logged_json(&log)["messages"][0]["content"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(req_a_system.contains("MENTOR_PERSONA_DIRECTIVE_XYZ"));
    assert!(req_a_system.contains("PERSONAL_LONG_TERM_TOKEN_777"));
    assert!(req_a_system.contains("WS_A_UNIQUE_FACT"));
    assert!(!req_a_system.contains("WS_B_UNIQUE_FACT"));

    // Session 2: same Bot in Workspace B.
    let (sid_b, bot_b, global_b, _) =
        create_bot_session(&agent, ws_b.clone(), Some("mentor-bot"), false).await;
    assert_eq!(bot_b.as_deref(), Some("mentor-bot"));
    assert!(!global_b);

    agent
        .ops
        .send(Op::SendMessage {
            session_id: sid_b.clone(),
            content: "Hello in workspace B".to_string(),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: MessageDelivery::Queue,
        })
        .await
        .expect("send msg B");
    common::recv_until(&agent.events, Duration::from_secs(15), |event| {
        matches!(event, Event::TurnComplete { session_id, .. } if session_id == &sid_b)
    })
    .await;

    let req_b_system = last_logged_json(&log)["messages"][0]["content"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(req_b_system.contains("MENTOR_PERSONA_DIRECTIVE_XYZ"));
    assert!(req_b_system.contains("PERSONAL_LONG_TERM_TOKEN_777"));
    assert!(req_b_system.contains("WS_B_UNIQUE_FACT"));
    assert!(!req_b_system.contains("WS_A_UNIQUE_FACT"));

    // Compact Session A and confirm Bot personal memory on disk AND in post-compaction prompt is preserved.
    agent
        .ops
        .send(Op::Compact {
            session_id: sid_a.clone(),
            instruction: None,
        })
        .await
        .expect("send Compact");
    common::recv_until(&agent.events, Duration::from_secs(15), |event| {
        matches!(event, Event::ContextCompacted { session_id, .. } if session_id == &sid_a)
    })
    .await;

    let on_disk_after_compact = std::fs::read_to_string(&bot_mem_file).expect("bot memory intact");
    assert!(on_disk_after_compact.contains("PERSONAL_LONG_TERM_TOKEN_777"));

    agent
        .ops
        .send(Op::SendMessage {
            session_id: sid_a.clone(),
            content: "After compaction check".to_string(),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: MessageDelivery::Queue,
        })
        .await
        .expect("send msg after compact");
    common::recv_until(&agent.events, Duration::from_secs(15), |event| {
        matches!(event, Event::TurnComplete { session_id, .. } if session_id == &sid_a)
    })
    .await;

    let req_after_compact = last_logged_json(&log)["messages"][0]["content"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(req_after_compact.contains("PERSONAL_LONG_TERM_TOKEN_777"));
    assert!(req_after_compact.contains("MENTOR_PERSONA_DIRECTIVE_XYZ"));

    agent.shutdown();
    let _ = std::fs::remove_dir_all(&root_dir);
}

#[tokio::test]
async fn global_session_blocks_workspace_tools_and_rebinds_cleanly() {
    let (config_path, root_dir, data_dir, log) = common::setup_with_log("bot-global-session");
    let ws = root_dir.join("bound-project");
    std::fs::create_dir_all(&ws).expect("ws");

    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), root_dir.clone(), data_dir.clone());

    // Create a global-only Bot (no workspace_tools, no network, no subagents, no control_plane).
    let restricted = BotProfile {
        bot_id: "global-notes".to_string(),
        name: "Global Notes".to_string(),
        avatar: None,
        description: "Workspace-free assistant".to_string(),
        system_prompt: "You organize personal notes without touching project files.".to_string(),
        provider_id: None,
        model_id: None,
        reasoning_level: None,
        capabilities: BotCapabilities {
            workspace_tools: false,
            control_plane: false,
            mcp: false,
            skills: false,
            network: false,
            subagents: false,
        },
        builtin: false,
        created_at_ms: 1000,
        updated_at_ms: 1000,
    };
    agent
        .ops
        .send(Op::UpsertBot { bot: restricted })
        .await
        .expect("upsert restricted bot");
    common::recv_until(&agent.events, Duration::from_secs(5), |event| {
        matches!(event, Event::BotUpdated { bot } if bot.bot_id == "global-notes")
    })
    .await;

    let (sid, bot_id, global, cwd) =
        create_bot_session(&agent, PathBuf::new(), Some("global-notes"), true).await;
    assert_eq!(bot_id.as_deref(), Some("global-notes"));
    assert!(global);
    assert!(cwd.as_os_str().is_empty());

    agent
        .ops
        .send(Op::SendMessage {
            session_id: sid.clone(),
            content: "Summarize my day".to_string(),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: MessageDelivery::Queue,
        })
        .await
        .expect("send global message");
    common::recv_until(&agent.events, Duration::from_secs(15), |event| {
        matches!(event, Event::TurnComplete { session_id, .. } if session_id == &sid)
    })
    .await;

    let tool_names: Vec<String> = last_logged_json(&log)["tools"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|t| t["function"]["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        !tool_names.iter().any(|n| n == "ExecCommand"),
        "global session must NOT project ExecCommand: {tool_names:?}"
    );
    assert!(
        !tool_names.iter().any(|n| n == "Glob" || n == "Grep" || n == "ApplyPatch"),
        "global session must NOT project workspace search/patch tools: {tool_names:?}"
    );
    assert!(
        !tool_names.iter().any(|n| n == "FetchURL" || n == "Agent" || n == "AgentSwarm" || n == "Skill"),
        "restricted bot must NOT project disabled capability tools: {tool_names:?}"
    );

    // Execution gate must also reject every disabled capability directly (defense in depth).
    let caps = BotCapabilities {
        workspace_tools: false,
        control_plane: false,
        mcp: false,
        skills: false,
        network: false,
        subagents: false,
    };
    assert!(check_tool_capability("ControlPlaneDispatch", false, Some(&caps), false, false).is_err());
    assert!(check_tool_capability("mcp__demo__ping", true, Some(&caps), false, false).is_err());
    assert!(check_tool_capability("FetchURL", false, Some(&caps), false, false).is_err());
    assert!(check_tool_capability("Skill", false, Some(&caps), false, false).is_err());
    assert!(check_tool_capability("Agent", false, Some(&caps), false, false).is_err());
    assert!(check_tool_capability("ExecCommand", false, Some(&caps), true, false).is_err());
    assert!(check_tool_capability("Write", false, Some(&caps), true, false).is_err());
    // Personal memory root I/O remains explicitly permitted.
    assert!(check_tool_capability("Write", false, Some(&caps), true, true).is_ok());

    // Bind global session to a workspace via Op::BindSessionWorkspace.
    agent
        .ops
        .send(Op::BindSessionWorkspace {
            session_id: sid.clone(),
            cwd: Some(ws.clone()),
        })
        .await
        .expect("bind workspace");
    let bound = common::recv_until(&agent.events, Duration::from_secs(5), |event| {
        matches!(event, Event::SessionConfigured { session_id, global: false, .. } if session_id == &sid)
    })
    .await;
    assert!(bound.iter().any(|event| matches!(
        event,
        Event::SessionConfigured { session_id, global: false, cwd, .. }
            if session_id == &sid && !cwd.as_os_str().is_empty()
    )));

    agent.shutdown();
    let _ = std::fs::remove_dir_all(&root_dir);
}
