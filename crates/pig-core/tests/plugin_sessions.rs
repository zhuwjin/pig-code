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
    CoreError, Event, MessageDelivery, Op, PluginAction, PluginGitReadRequest, PluginPackageSource,
    PluginSessionAction, PluginSource, PluginWorkspaceFileReadRequest,
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
            {"name": "agent.delegate", "targets": ["*"]},
            {"name": "git.read", "targets": ["*"]},
            {"name": "files.read", "targets": ["*"]}
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

fn write_project_package(root: &Path) {
    write_package(root);
    let path = root.join("pig-plugin.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("read manifest")).unwrap();
    let contributions = manifest["contributions"].as_object_mut().unwrap();
    contributions.insert(
        "actions".into(),
        serde_json::json!(["provide_agents", "read_config", "write_config"]),
    );
    contributions.insert(
        "config".into(),
        serde_json::json!({
            "items": [{
                "key": "theme",
                "description": {"en": "Theme"},
                "default": "light",
                "schema": {"type": "string", "enum": ["light", "dark"]}
            }]
        }),
    );
    std::fs::write(path, serde_json::to_vec_pretty(&manifest).unwrap()).expect("project manifest");
}

async fn install(agent: &pig_core::AgentHandle, root: &Path) -> (String, String) {
    install_with_source(agent, root, PluginSource::User).await
}

async fn install_with_source(
    agent: &pig_core::AgentHandle,
    root: &Path,
    source: PluginSource,
) -> (String, String) {
    agent
        .ops
        .send(Op::SyncPlugins {
            packages: vec![PluginPackageSource {
                root: root.to_path_buf(),
                source,
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

async fn service_value(
    agent: &pig_core::AgentHandle,
    request_id: &str,
) -> Result<serde_json::Value, CoreError> {
    let events = recv_until(&agent.events, Duration::from_secs(30), |event| {
        matches!(
            event,
            Event::PluginServiceValue { request_id: current, .. } if current == request_id
        )
    })
    .await;
    events
        .into_iter()
        .find_map(|event| match event {
            Event::PluginServiceValue {
                request_id: current,
                result,
                ..
            } if current == request_id => Some(result),
            _ => None,
        })
        .expect("plugin service value")
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
        },
    )
    .await;
    let created = result(&agent, "create-1").await.expect("create result");
    let session_id = created["sessionId"]
        .as_str()
        .expect("session id")
        .to_owned();
    assert!(!session_id.is_empty());
    agent
        .ops
        .send(Op::OpenSession {
            session_id: session_id.clone(),
        })
        .await
        .expect("reopen plugin-created session");
    let configured = recv_until(&agent.events, Duration::from_secs(10), |event| {
        matches!(event, Event::SessionConfigured { session_id: current, .. } if current == &session_id)
    })
    .await;
    assert!(configured.iter().any(|event| matches!(
        event,
        Event::SessionConfigured { permissions, .. } if permissions == &pig_protocol::PermissionSettings::default()
    )));

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
        "start-agent-foreground",
        (&package, &digest),
        PluginSessionAction::AgentStart {
            session_id: session_id.clone(),
            profile: "reviewer".into(),
            prompt: "Review the pending change.".into(),
            description: "Review the change".into(),
            background: false,
        },
    )
    .await;
    assert!(matches!(
        result(&agent, "start-agent-foreground").await,
        Err(CoreError::PluginInvalidInput { .. })
    ));

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

fn git(dir: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Pig Test")
        .env("GIT_AUTHOR_EMAIL", "pig@example.test")
        .env("GIT_COMMITTER_NAME", "Pig Test")
        .env("GIT_COMMITTER_EMAIL", "pig@example.test")
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn a_plugin_reads_git_and_workspace_files_inside_its_session() {
    let (config_path, dir, data_dir) = setup("plugin-workspace-read");
    std::fs::create_dir_all(dir.join("src")).expect("source directory");
    std::fs::write(dir.join("src/lib.rs"), "one\ntwo\nthree\n").expect("source file");
    git(&dir, &["init", "-q"]);
    git(&dir, &["add", "src/lib.rs"]);
    git(&dir, &["commit", "-q", "-m", "initial commit"]);
    std::fs::write(dir.join("src/lib.rs"), "one\nTWO\nthree\n").expect("modify source");

    let package_root = dir.join("pkg");
    write_package(&package_root);
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let (package, digest) = install(&agent, &package_root).await;

    action(
        &agent,
        "create-read-session",
        (&package, &digest),
        PluginSessionAction::Create {
            cwd: dir.clone(),
            provider_id: None,
            model_id: None,
            reasoning_level: None,
        },
    )
    .await;
    let session_id = result(&agent, "create-read-session")
        .await
        .expect("session")["sessionId"]
        .as_str()
        .expect("session id")
        .to_owned();

    let git_read = |request_id: &str, request: PluginGitReadRequest| Op::PluginGitRead {
        request_id: request_id.to_owned(),
        package: package.clone(),
        digest: digest.clone(),
        session_id: session_id.clone(),
        request,
    };
    agent
        .ops
        .send(git_read("status-1", PluginGitReadRequest::Status))
        .await
        .expect("git status");
    let status = service_value(&agent, "status-1").await.expect("status");
    assert!(
        status["unstaged"]
            .as_array()
            .is_some_and(|changes| changes.iter().any(|change| change["path"] == "src/lib.rs")),
        "{status}"
    );

    agent
        .ops
        .send(git_read(
            "diff-1",
            PluginGitReadRequest::Diff {
                path: "src/lib.rs".into(),
                staged: false,
            },
        ))
        .await
        .expect("git diff");
    let diff = service_value(&agent, "diff-1").await.expect("diff");
    assert!(
        diff["diff"]
            .as_str()
            .is_some_and(|text| text.contains("TWO")),
        "{diff}"
    );

    agent
        .ops
        .send(git_read(
            "log-1",
            PluginGitReadRequest::Log {
                limit: Some(5),
                path: None,
            },
        ))
        .await
        .expect("git log");
    let log = service_value(&agent, "log-1").await.expect("log");
    assert_eq!(log["commits"][0]["subject"], "initial commit", "{log}");

    agent
        .ops
        .send(git_read(
            "show-1",
            PluginGitReadRequest::Show {
                rev: "HEAD".into(),
                path: None,
            },
        ))
        .await
        .expect("git show");
    let show = service_value(&agent, "show-1").await.expect("show");
    assert!(
        show["text"]
            .as_str()
            .is_some_and(|text| text.contains("initial commit")),
        "{show}"
    );

    agent
        .ops
        .send(git_read("branches-1", PluginGitReadRequest::Branches))
        .await
        .expect("git branches");
    let branches = service_value(&agent, "branches-1").await.expect("branches");
    assert!(branches["current"].is_string(), "{branches}");

    agent
        .ops
        .send(Op::PluginWorkspaceFileRead {
            request_id: "file-1".into(),
            package: package.clone(),
            digest: digest.clone(),
            session_id: session_id.clone(),
            request: PluginWorkspaceFileReadRequest {
                path: "src/lib.rs".into(),
                start_line: Some(2),
                end_line: Some(3),
                max_bytes: None,
            },
        })
        .await
        .expect("file read");
    let file = service_value(&agent, "file-1").await.expect("file");
    assert_eq!(file["content"], "TWO\nthree", "{file}");

    agent
        .ops
        .send(Op::PluginWorkspaceFileRead {
            request_id: "file-escape".into(),
            package,
            digest,
            session_id,
            request: PluginWorkspaceFileReadRequest {
                path: "../outside.txt".into(),
                start_line: None,
                end_line: None,
                max_bytes: None,
            },
        })
        .await
        .expect("file escape");
    let error = service_value(&agent, "file-escape")
        .await
        .expect_err("traversal must fail");
    assert!(matches!(
        error,
        CoreError::PluginInvalidInput { .. } | CoreError::PluginPermission { .. }
    ));
}

#[tokio::test]
async fn project_plugins_cannot_cross_workspace_boundaries() {
    let (config_path, dir, data_dir) = setup("plugin-project-boundaries");
    let other = dir.with_extension("outside");
    let _ = std::fs::remove_dir_all(&other);
    std::fs::create_dir_all(&other).expect("other workspace");
    let trusted = dir
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    let config = std::fs::read_to_string(&config_path).expect("base config");
    std::fs::write(
        &config_path,
        format!("{config}\n[hooks]\ntrusted_projects=[\"{trusted}\"]\n"),
    )
    .expect("trust project workspace");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let package_root = dir.join(".pigcode/plugins/test.sessions");
    write_project_package(&package_root);
    let (package, digest) = install_with_source(
        &agent,
        &package_root,
        PluginSource::Project {
            workspace: dir.clone(),
        },
    )
    .await;
    let other_session = common::new_session(&agent, other.clone()).await;

    action(
        &agent,
        "list-other-workspace-agents",
        (&package, &digest),
        PluginSessionAction::AgentList {
            session_id: Some(other_session.clone()),
        },
    )
    .await;
    assert!(matches!(
        result(&agent, "list-other-workspace-agents").await,
        Err(CoreError::PluginPermission { .. })
    ));

    let linked = dir.join("linked-outside");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&other, &linked).expect("symlink outside workspace");
    #[cfg(windows)]
    common::junction(&linked, &other);
    action(
        &agent,
        "create-through-symlink",
        (&package, &digest),
        PluginSessionAction::Create {
            cwd: linked,
            provider_id: None,
            model_id: None,
            reasoning_level: None,
        },
    )
    .await;
    assert!(matches!(
        result(&agent, "create-through-symlink").await,
        Err(CoreError::PluginPermission { .. })
    ));

    agent
        .ops
        .send(Op::PluginAction {
            request_id: "write-other-workspace-config".into(),
            package,
            digest,
            action: PluginAction::WriteConfig {
                path: "theme".into(),
                value: serde_json::json!("dark"),
            },
            session_id: Some(other_session),
        })
        .await
        .expect("write other workspace config");
    let events = recv_until(&agent.events, Duration::from_secs(10), |event| {
        matches!(event, Event::PluginActionResult { result } if result.request_id == "write-other-workspace-config")
    })
    .await;
    let outcome = events
        .iter()
        .find_map(|event| match event {
            Event::PluginActionResult { result }
                if result.request_id == "write-other-workspace-config" =>
            {
                Some(result.result.clone())
            }
            _ => None,
        })
        .expect("config action result");
    assert!(matches!(outcome, Err(CoreError::PluginPermission { .. })));
    assert!(!other.join(".pigcode/plugins.json").exists());

    agent.ops.send(Op::Shutdown).await.expect("shutdown agent");
    let _ = std::fs::remove_dir_all(other);
}
