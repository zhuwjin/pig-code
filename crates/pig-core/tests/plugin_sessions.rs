//! P1 #5: a plugin's narrow session actions against a real agent loop.
//!
//! The fixture package is resource-only: the core runner's authorization sees
//! it enabled in the registry, then the actions create a session, send a
//! message, wait for the turn, interrupt it, and run a controlled model call.

mod common;

use std::path::Path;
use std::time::Duration;

use common::{recv_until, setup};
use pig_protocol::{
    CoreError, Event, ExecMode, MessageDelivery, Op, PluginPackageSource, PluginSessionAction,
    PluginSource,
};

const PACKAGE_ID: &str = "test.sessions";

fn write_package(root: &Path) {
    std::fs::create_dir_all(root).expect("package directory");
    std::fs::write(
        root.join("pig-plugin.json"),
        r#"{
          "apiVersion": 1,
          "id": "test.sessions",
          "name": "Session Test",
          "version": "1.0.0",
          "build": 1,
          "minHostVersion": "1.0.0",
          "protocols": {"ui": 1, "services": 1},
          "capabilities": [
            {"name": "session.create", "targets": ["*"]},
            {"name": "session.control", "targets": ["*"]},
            {"name": "model.request", "targets": ["review"]},
            {"name": "agent.delegate", "targets": ["*"]}
          ],
          "contributions": {
            "actions": ["provide_agents"],
            "agents": [{
              "name": "reviewer",
              "description": "Reviews a change",
              "tools": [],
              "max_turns": 3,
              "system_prompt": "You are reviewer, a code review subagent."
            }]
          }
        }"#,
    )
    .expect("manifest");
}

async fn install(agent: &pig_core::AgentHandle, root: &Path) -> (String, String) {
    agent
        .ops
        .send(Op::SyncPlugins {
            packages: vec![PluginPackageSource {
                root: root.to_path_buf(),
                source: PluginSource::User,
            }],
        })
        .await
        .expect("sync plugin");
    let events = recv_until(&agent.events, Duration::from_secs(10), |event| {
        matches!(
            event,
            Event::PluginSnapshot { snapshot }
                if snapshot.packages.iter().any(|package| package.id == PACKAGE_ID)
        )
    })
    .await;
    let snapshot = events
        .iter()
        .find_map(|event| match event {
            Event::PluginSnapshot { snapshot } => Some(snapshot),
            _ => None,
        })
        .expect("plugin snapshot");
    let package = snapshot
        .packages
        .iter()
        .find(|package| package.id == PACKAGE_ID)
        .expect("fixture package");
    let (id, digest) = (package.id.clone(), package.digest.clone());
    agent
        .ops
        .send(Op::SetPluginEnabled {
            package: id.clone(),
            digest: digest.clone(),
            enabled: true,
        })
        .await
        .expect("enable plugin");
    recv_until(&agent.events, Duration::from_secs(10), |event| {
        matches!(
            event,
            Event::PluginSnapshot { snapshot }
                if snapshot.packages.iter().any(|package| package.id == PACKAGE_ID && package.enabled)
        )
    })
    .await;
    (id, digest)
}

async fn action(
    agent: &pig_core::AgentHandle,
    request_id: &str,
    (package, digest): (&str, &str),
    action: PluginSessionAction,
) {
    agent
        .ops
        .send(Op::PluginSessionAction {
            request_id: request_id.to_owned(),
            package: package.to_owned(),
            digest: digest.to_owned(),
            action,
        })
        .await
        .expect("session action");
}

async fn result(
    agent: &pig_core::AgentHandle,
    request_id: &str,
) -> Result<serde_json::Value, CoreError> {
    let events = recv_until(&agent.events, Duration::from_secs(30), |event| {
        matches!(
            event,
            Event::PluginSessionResult { request_id: current, .. } if current == request_id
        )
    })
    .await;
    events
        .into_iter()
        .find_map(|event| match event {
            Event::PluginSessionResult {
                request_id: current,
                result,
                ..
            } if current == request_id => Some(result),
            _ => None,
        })
        .expect("session result")
}

#[tokio::test]
async fn a_plugin_creates_sends_waits_and_interrupts_a_session() {
    let (config_path, dir, data_dir) = setup("plugin-sessions");
    let package_root = dir.join("pkg");
    write_package(&package_root);
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let (package, digest) = install(&agent, &package_root).await;

    action(
        &agent,
        "create-1",
        (&package, &digest),
        PluginSessionAction::Create {
            cwd: dir.clone(),
            provider_id: None,
            model_id: None,
            reasoning_level: None,
            permissions: Some(ExecMode::Auto.permissions()),
        },
    )
    .await;
    let created = result(&agent, "create-1").await.expect("create result");
    let session_id = created["sessionId"]
        .as_str()
        .expect("session id")
        .to_owned();
    assert!(!session_id.is_empty());

    action(
        &agent,
        "send-1",
        (&package, &digest),
        PluginSessionAction::Send {
            session_id: session_id.clone(),
            content: "SLOW_BASH_SCENARIO".into(),
            delivery: MessageDelivery::Queue,
        },
    )
    .await;
    assert_eq!(
        result(&agent, "send-1").await.expect("send result")["accepted"],
        true
    );
    recv_until(&agent.events, Duration::from_secs(20), |event| {
        matches!(event, Event::TurnStarted { session_id: current, .. } if current == &session_id)
    })
    .await;

    // The turn is still running (the scenario sleeps); a wait is parked and
    // settles when the interrupt aborts the turn.
    action(
        &agent,
        "wait-1",
        (&package, &digest),
        PluginSessionAction::Wait {
            session_id: session_id.clone(),
            timeout_ms: 30_000,
        },
    )
    .await;
    action(
        &agent,
        "interrupt-1",
        (&package, &digest),
        PluginSessionAction::Interrupt {
            session_id: session_id.clone(),
        },
    )
    .await;
    assert!(
        result(&agent, "interrupt-1")
            .await
            .expect("interrupt")
            .is_null()
    );
    let waited = result(&agent, "wait-1").await.expect("wait result");
    assert_eq!(waited["status"], "aborted");
}

#[tokio::test]
async fn a_plugin_model_request_is_bounded_and_recorded() {
    let (config_path, dir, data_dir) = setup("plugin-model");
    let package_root = dir.join("pkg");
    write_package(&package_root);
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let (package, digest) = install(&agent, &package_root).await;

    action(
        &agent,
        "model-1",
        (&package, &digest),
        PluginSessionAction::Model {
            session_id: None,
            purpose: "review".into(),
            system: Some("You review code.".into()),
            prompt: "Review the pending change.".into(),
            max_output_tokens: Some(128),
        },
    )
    .await;
    let response = result(&agent, "model-1").await.expect("model result");
    let text = response["text"].as_str().expect("model text");
    assert!(!text.is_empty());

    action(
        &agent,
        "model-2",
        (&package, &digest),
        PluginSessionAction::Model {
            session_id: None,
            purpose: "not-a-purpose".into(),
            system: None,
            prompt: "Hello".into(),
            max_output_tokens: None,
        },
    )
    .await;
    let error = result(&agent, "model-2")
        .await
        .expect_err("invalid purpose");
    assert!(matches!(error, CoreError::PluginPermission { .. }));
}

#[tokio::test]
async fn a_plugin_lists_starts_waits_reads_and_stops_contributed_agents() {
    let (config_path, dir, data_dir) = setup("plugin-agents");
    let package_root = dir.join("pkg");
    write_package(&package_root);
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let (package, digest) = install(&agent, &package_root).await;

    action(
        &agent,
        "create-agent-session",
        (&package, &digest),
        PluginSessionAction::Create {
            cwd: dir.clone(),
            provider_id: None,
            model_id: None,
            reasoning_level: None,
            permissions: Some(ExecMode::Auto.permissions()),
        },
    )
    .await;
    let created = result(&agent, "create-agent-session")
        .await
        .expect("create result");
    let session_id = created["sessionId"]
        .as_str()
        .expect("session id")
        .to_owned();

    action(
        &agent,
        "list-agents",
        (&package, &digest),
        PluginSessionAction::AgentList {
            session_id: Some(session_id.clone()),
        },
    )
    .await;
    let listed = result(&agent, "list-agents").await.expect("agent list");
    let profiles = listed["profiles"].as_array().expect("profiles");
    let reviewer = profiles
        .iter()
        .find(|profile| profile["name"] == "reviewer")
        .expect("contributed profile");
    assert_eq!(reviewer["source"], "plugin");

    action(
        &agent,
        "start-agent",
        (&package, &digest),
        PluginSessionAction::AgentStart {
            session_id: session_id.clone(),
            profile: "reviewer".into(),
            prompt: "Review the pending change.".into(),
            description: "Review the change".into(),
            background: true,
        },
    )
    .await;
    let started_events = recv_until(&agent.events, Duration::from_secs(20), |event| {
        matches!(
            event,
            Event::PluginSessionResult { request_id: current, .. } if current == "start-agent"
        )
    })
    .await;
    let started = started_events
        .iter()
        .find_map(|event| match event {
            Event::PluginSessionResult {
                request_id: current,
                result,
                ..
            } if current == "start-agent" => result.clone().ok(),
            _ => None,
        })
        .expect("agent start result");
    let agent_id = started["agentId"].as_str().expect("agent id").to_owned();
    assert_eq!(started["status"], "running");
    // The existing Agent card/tool UI is driven for the delegated child.
    assert!(
        started_events.iter().any(|event| matches!(
            event,
            Event::SubagentCard { agent_id: current, .. } if current == &agent_id
        )),
        "the delegated child emits the ordinary Agent card"
    );
    assert!(started_events.iter().any(|event| matches!(
        event,
        Event::ToolCallBegin { item_id, tool, .. }
            if item_id == &format!("plugin-agent-{agent_id}") && tool == "Agent"
    )));

    action(
        &agent,
        "wait-agent",
        (&package, &digest),
        PluginSessionAction::AgentWait {
            session_id: session_id.clone(),
            agent_id: agent_id.clone(),
            timeout_ms: 30_000,
        },
    )
    .await;
    let waited = result(&agent, "wait-agent").await.expect("agent wait");
    assert_eq!(waited["status"], "completed", "{waited}");
    assert!(
        waited["summary"]
            .as_str()
            .is_some_and(|summary| !summary.is_empty())
    );

    action(
        &agent,
        "read-agent",
        (&package, &digest),
        PluginSessionAction::AgentRead {
            session_id: session_id.clone(),
            agent_id: agent_id.clone(),
        },
    )
    .await;
    let read = result(&agent, "read-agent").await.expect("agent read");
    assert_eq!(read["agentId"], agent_id);
    assert!(
        read["items"]
            .as_array()
            .is_some_and(|items| !items.is_empty()),
        "{read}"
    );
}
