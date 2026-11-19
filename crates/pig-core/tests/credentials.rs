mod common;

use common::{new_session, outside_dir, setup};
use pig_protocol::{ApprovalDecision, Event, ExecMode, Op};
use pig_provider::mock;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn tool_ends(events: &[Event]) -> Vec<(&str, &str, bool)> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::ToolCallEnd {
                item_id,
                output,
                is_error,
                ..
            } => Some((item_id.as_str(), output.as_str(), *is_error)),
            _ => None,
        })
        .collect()
}

/// Every test in this dedicated binary redirects `USERPROFILE`; the async
/// semaphore keeps those redirects and their live agents from interleaving.
async fn credential_env_lock() -> tokio::sync::OwnedSemaphorePermit {
    static LOCK: std::sync::OnceLock<std::sync::Arc<tokio::sync::Semaphore>> =
        std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Arc::new(tokio::sync::Semaphore::new(1)))
        .clone()
        .acquire_owned()
        .await
        .expect("credential env lock")
}

/// A credential profile outside the workspace **and** outside the system tmp.
/// tmp is exempt from the outside-write gate, so a profile under it would not
/// exercise the production outside-write dialog.
fn outside_credential_profile(tag: &str) -> PathBuf {
    let profile = outside_dir(&format!("credential-{tag}"));
    let _ = std::fs::remove_dir_all(&profile);
    std::fs::create_dir_all(profile.join(".ssh")).expect("create outside profile");
    profile
}

/// A single file inside a protected credential directory is approvable, but
/// the dialog offers no "remember for the session" option: a standing grant
/// there is a standing capability the user approved for one file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_credential_file_grant_is_offered_once_per_command() {
    let _env = credential_env_lock().await;
    let (config_path, dir, data_dir) = setup("credential-file-approval");
    // A stand-in profile so the gate classifies the target as a credential file.
    let profile = dir.join("profile");
    std::fs::create_dir_all(profile.join(".ssh")).expect("create fake profile");
    std::fs::write(profile.join(".ssh").join("known_hosts"), "OLD").expect("write fixture");
    // SAFETY: this test runs alone in its own process instance; the gate reads
    // the variable on the same thread pool.
    let previous = std::env::var_os("USERPROFILE");
    let target = profile.join(".ssh").join("known_hosts");
    unsafe { std::env::set_var("USERPROFILE", &profile) };

    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let events = agent.events.clone();
    let session_id = new_session(&agent, dir.clone()).await;
    let target_token = target
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/");
    agent
        .ops
        .send(Op::SetPermissions {
            session_id: session_id.clone(),
            permissions: ExecMode::Auto.permissions(),
        })
        .await
        .unwrap();
    agent
        .ops
        .send(Op::SendMessage {
            session_id,
            content: format!("{} {target_token}", mock::CREDENTIAL_FILE_TRIGGER),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();

    let mut saw_dialog = false;
    let mut decisions_ok = false;
    let mut detail_ok = false;
    let mut saw_native_refusal = false;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "timed out");
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await
        else {
            continue;
        };
        if let Event::ApprovalRequested {
            request_id,
            detail,
            available_decisions,
            ..
        } = &event
        {
            saw_dialog = true;
            detail_ok = detail.contains("protected credential directory");
            decisions_ok = !available_decisions.contains(&ApprovalDecision::ApprovedForSession)
                && !available_decisions.contains(&ApprovalDecision::ApprovedForProject);
            agent
                .ops
                .send(Op::ApprovalReply {
                    request_id: request_id.clone(),
                    decision: ApprovalDecision::Allow,
                    feedback: None,
                })
                .await
                .unwrap();
        }
        if let Event::ToolCallEnd { output, .. } = &event {
            saw_native_refusal |= common::native_sandbox_refusal(output);
        }
        let done = matches!(event, Event::TurnComplete { .. });
        if done {
            break;
        }
    }
    if let Some(previous) = previous {
        unsafe { std::env::set_var("USERPROFILE", previous) };
    } else {
        unsafe { std::env::remove_var("USERPROFILE") };
    }

    assert!(saw_dialog, "a credential file grant still prompts");
    assert!(
        decisions_ok,
        "no session-scoped option for a credential file"
    );
    assert!(detail_ok, "the dialog explains the credential boundary");
    if common::native_sandbox_usable() {
        assert_eq!(
            std::fs::read_to_string(&target).unwrap_or_default().trim(),
            "x"
        );
    } else {
        assert_eq!(
            std::fs::read_to_string(&target).unwrap_or_default().trim(),
            "OLD"
        );
        assert!(
            saw_native_refusal,
            "the approved command still fails closed"
        );
    }
    agent.shutdown();
}

/// A directory inside a protected credential store is refused **without a
/// dialog**: approving a directory would grant every key and token next to the
/// file the model actually named.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_credential_directory_grant_is_refused_outright() {
    let _env = credential_env_lock().await;
    let (config_path, dir, data_dir) = setup("credential-dir-refusal");
    let profile = dir.join("profile");
    std::fs::create_dir_all(profile.join(".ssh")).expect("create fake profile");
    std::fs::write(profile.join(".ssh").join("known_hosts"), "OLD").expect("write fixture");
    let target = profile.join(".ssh").join("known_hosts");
    // SAFETY: see `a_credential_file_grant_is_offered_once_per_command`.
    let previous = std::env::var_os("USERPROFILE");
    unsafe { std::env::set_var("USERPROFILE", &profile) };

    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let events = agent.events.clone();
    let session_id = new_session(&agent, dir.clone()).await;
    let target_token = target
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/");
    agent
        .ops
        .send(Op::SetPermissions {
            session_id: session_id.clone(),
            permissions: ExecMode::Auto.permissions(),
        })
        .await
        .unwrap();
    agent
        .ops
        .send(Op::SendMessage {
            session_id,
            content: format!("{} {target_token}", mock::CREDENTIAL_DIR_TRIGGER),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();

    let mut saw_dialog = false;
    let mut refusal: Option<String> = None;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "timed out");
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await
        else {
            continue;
        };
        if matches!(event, Event::ApprovalRequested { .. }) {
            saw_dialog = true;
        }
        if let Event::ToolCallEnd {
            output, is_error, ..
        } = &event
            && *is_error
        {
            refusal = Some(output.clone());
        }
        let done = matches!(event, Event::TurnComplete { .. });
        if done {
            break;
        }
    }
    if let Some(previous) = previous {
        unsafe { std::env::set_var("USERPROFILE", previous) };
    } else {
        unsafe { std::env::remove_var("USERPROFILE") };
    }

    assert!(!saw_dialog, "a directory grant must not reach the dialog");
    let refusal = refusal.expect("the tool result explains the refusal");
    assert!(
        refusal.contains("protected credential directory"),
        "{refusal}"
    );
    assert_eq!(
        std::fs::read_to_string(&target).unwrap_or_default().trim(),
        "OLD",
        "the file is untouched"
    );
    agent.shutdown();
}

/// A Write into a protected credential directory goes through the same rules as
/// a Bash widening: one file is approvable with no session option, and the
/// dialog says so. The mock asks for the `authorized_keys` path, which also
/// carries the high-risk warning.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_credential_write_is_approved_as_one_file() {
    let _env = credential_env_lock().await;
    let (config_path, dir, data_dir) = setup("credential-write");
    let profile = dir.join("profile");
    std::fs::create_dir_all(profile.join(".ssh")).expect("create fake profile");
    let target = profile.join(".ssh").join("authorized_keys");
    std::fs::write(&target, "ssh-ed25519 AAAA existing").expect("write fixture");
    // SAFETY: credential tests serialize on `credential_env_lock`.
    let previous = std::env::var_os("USERPROFILE");
    unsafe { std::env::set_var("USERPROFILE", &profile) };

    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir.clone());
    let events = agent.events.clone();
    let session_id = new_session(&agent, dir.clone()).await;
    agent
        .ops
        .send(Op::SetPermissions {
            session_id: session_id.clone(),
            permissions: ExecMode::Auto.permissions(),
        })
        .await
        .unwrap();
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: format!(
                "{} {}",
                mock::CREDENTIAL_WRITE_TRIGGER,
                target
                    .to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/")
            ),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();

    let mut saw_dialog = false;
    let mut decisions_ok = false;
    let mut warning = false;
    let mut old_content_in_event = false;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "timed out");
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await
        else {
            continue;
        };
        if let Event::ApprovalRequested {
            request_id,
            detail,
            available_decisions,
            ..
        } = &event
        {
            saw_dialog = true;
            warning = detail.contains("key-based logins");
            decisions_ok = !available_decisions.contains(&ApprovalDecision::ApprovedForSession);
            agent
                .ops
                .send(Op::ApprovalReply {
                    request_id: request_id.clone(),
                    decision: ApprovalDecision::Allow,
                    feedback: None,
                })
                .await
                .unwrap();
        }
        if let Event::FileChanged { unified_diff, .. } = &event
            && unified_diff.contains("ssh-ed25519 AAAA existing")
        {
            old_content_in_event = true;
        }
        let done = matches!(event, Event::TurnComplete { .. });
        if done {
            break;
        }
    }
    if let Some(previous) = previous {
        unsafe { std::env::set_var("USERPROFILE", previous) };
    } else {
        unsafe { std::env::remove_var("USERPROFILE") };
    }

    assert!(saw_dialog, "a credential write prompts");
    assert!(decisions_ok, "no session-scoped option");
    assert!(warning, "authorized_keys carries the high-risk warning");
    assert_eq!(
        std::fs::read_to_string(&target).unwrap_or_default().trim(),
        "ssh-ed25519 AAAA new-key",
        "the approved write landed"
    );
    assert!(
        !old_content_in_event,
        "a write-only approval must not put the old credential content in a change event"
    );
    agent.shutdown();

    // The write-only rule also covers the durable side: neither the revert
    // baseline nor the stored change diff may carry the content the model was
    // never allowed to read.
    let store = pig_core::store::Store::open(&data_dir).expect("reopen store");
    let originals = store.file_originals(&session_id);
    assert!(
        originals.iter().all(|(_, content)| content
            .as_deref()
            .is_none_or(|c| !c.contains("AAAA existing"))),
        "the old credential must not become a persisted baseline: {originals:?}"
    );
    let changes = store.file_changes(&session_id);
    assert!(
        changes
            .iter()
            .all(|(_, diff, _, _)| !diff.contains("AAAA existing")),
        "the old credential must not enter the stored diff: {changes:?}"
    );
}

/// The production credential store lives outside the workspace. That write
/// dialog must keep the one-file explanation and the key-based-login warning:
/// before, the outside-write branch replaced the credential detail entirely,
/// so `~/.ssh/authorized_keys` was approved with only a boundary line.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_outside_credential_write_keeps_the_one_file_warning() {
    let _env = credential_env_lock().await;
    let (config_path, dir, data_dir) = setup("credential-outside-write");
    let profile = outside_credential_profile("outside-write");
    let target = profile.join(".ssh").join("authorized_keys");
    std::fs::write(&target, "ssh-ed25519 AAAA existing").expect("write fixture");
    // SAFETY: credential tests serialize on `credential_env_lock`.
    let previous = std::env::var_os("USERPROFILE");
    unsafe { std::env::set_var("USERPROFILE", &profile) };

    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let events = agent.events.clone();
    let session_id = new_session(&agent, dir.clone()).await;
    agent
        .ops
        .send(Op::SetPermissions {
            session_id: session_id.clone(),
            permissions: ExecMode::Auto.permissions(),
        })
        .await
        .unwrap();
    agent
        .ops
        .send(Op::SendMessage {
            session_id,
            content: format!(
                "{} {}",
                mock::CREDENTIAL_WRITE_TRIGGER,
                target
                    .to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/")
            ),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();

    let mut dialogs: Vec<(String, Vec<ApprovalDecision>)> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "timed out");
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await
        else {
            continue;
        };
        if let Event::ApprovalRequested {
            request_id,
            detail,
            available_decisions,
            ..
        } = &event
        {
            dialogs.push((detail.clone(), available_decisions.clone()));
            agent
                .ops
                .send(Op::ApprovalReply {
                    request_id: request_id.clone(),
                    decision: ApprovalDecision::Allow,
                    feedback: None,
                })
                .await
                .unwrap();
        }
        if matches!(event, Event::TurnComplete { .. }) {
            break;
        }
    }
    if let Some(previous) = previous {
        unsafe { std::env::set_var("USERPROFILE", previous) };
    } else {
        unsafe { std::env::remove_var("USERPROFILE") };
    }
    agent.shutdown();

    let outside = dialogs
        .iter()
        .find(|(detail, _)| detail.contains("outside the workspace"))
        .unwrap_or_else(|| panic!("the outside credential write must prompt: {dialogs:?}"));
    let detail = &outside.0;
    assert!(
        detail.contains("only this one file is shared"),
        "the one-file explanation is lost: {detail}"
    );
    assert!(
        detail.contains("cannot be remembered for the session"),
        "the no-memory explanation is lost: {detail}"
    );
    assert!(
        detail.contains("key-based logins"),
        "the authorized_keys warning is lost: {detail}"
    );
    assert!(
        !outside.1.contains(&ApprovalDecision::ApprovedForSession)
            && !outside.1.contains(&ApprovalDecision::ApprovedForProject),
        "an outside credential write offers no memory tier: {:?}",
        outside.1
    );
    assert_eq!(
        std::fs::read_to_string(&target).unwrap_or_default().trim(),
        "ssh-ed25519 AAAA new-key",
        "the approved outside credential write landed"
    );
    let _ = std::fs::remove_dir_all(&profile);
}

/// The same flow for a **sensitive-named** credential file (`id_rsa`): the
/// named single-file approval lifts both the credential veto and the
/// sensitive-name refusal — before, the name veto made this dialog doomed
/// (approve, then the write still failed).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_credential_write_to_a_sensitive_name_lands_after_the_named_approval() {
    let _env = credential_env_lock().await;
    let (config_path, dir, data_dir) = setup("credential-write-key");
    let profile = dir.join("profile");
    std::fs::create_dir_all(profile.join(".ssh")).expect("create fake profile");
    let target = profile.join(".ssh").join("id_rsa");
    std::fs::write(&target, "OLD-KEY").expect("write fixture");
    // SAFETY: credential tests serialize on `credential_env_lock`.
    let previous = std::env::var_os("USERPROFILE");
    unsafe { std::env::set_var("USERPROFILE", &profile) };

    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let events = agent.events.clone();
    let session_id = new_session(&agent, dir.clone()).await;
    agent
        .ops
        .send(Op::SetPermissions {
            session_id: session_id.clone(),
            permissions: ExecMode::Auto.permissions(),
        })
        .await
        .unwrap();
    agent
        .ops
        .send(Op::SendMessage {
            session_id,
            content: format!(
                "{} {}",
                mock::SCENARIO_OUTSIDE_WRITE_TRIGGER,
                target
                    .to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/")
            ),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();

    let mut saw_dialog = false;
    let mut named_file = false;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "timed out");
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await
        else {
            continue;
        };
        if let Event::ApprovalRequested {
            request_id, detail, ..
        } = &event
        {
            saw_dialog = true;
            named_file = detail.contains("id_rsa");
            agent
                .ops
                .send(Op::ApprovalReply {
                    request_id: request_id.clone(),
                    decision: ApprovalDecision::Allow,
                    feedback: None,
                })
                .await
                .unwrap();
        }
        let done = matches!(event, Event::TurnComplete { .. });
        if done {
            break;
        }
    }
    if let Some(previous) = previous {
        unsafe { std::env::set_var("USERPROFILE", previous) };
    } else {
        unsafe { std::env::remove_var("USERPROFILE") };
    }

    assert!(saw_dialog, "the credential write prompts");
    assert!(named_file, "the dialog names the exact file");
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "outside write",
        "the approved write landed past both vetoes"
    );
    agent.shutdown();
}

/// A Bash read request for one credential file: the dialog says "read", the
/// command is granted read (not write), and the file is never added to a
/// write set.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_credential_read_request_grants_read_not_write() {
    let _env = credential_env_lock().await;
    let (config_path, dir, data_dir) = setup("credential-read-grant");
    let profile = dir.join("profile");
    std::fs::create_dir_all(profile.join(".ssh")).expect("create fake profile");
    let target = profile.join(".ssh").join("known_hosts");
    std::fs::write(&target, "KNOWN-HOSTS-ENTRY").expect("write fixture");
    // SAFETY: credential tests serialize on `credential_env_lock`.
    let previous = std::env::var_os("USERPROFILE");
    unsafe { std::env::set_var("USERPROFILE", &profile) };

    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let events = agent.events.clone();
    let session_id = new_session(&agent, dir.clone()).await;
    agent
        .ops
        .send(Op::SetPermissions {
            session_id: session_id.clone(),
            permissions: ExecMode::Auto.permissions(),
        })
        .await
        .unwrap();
    agent
        .ops
        .send(Op::SendMessage {
            session_id,
            content: format!(
                "{} {}",
                mock::CREDENTIAL_READ_TRIGGER,
                target
                    .to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/")
            ),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();

    let mut saw_dialog = false;
    let mut said_read = false;
    let mut said_write = false;
    let mut decisions_ok = false;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "timed out");
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await
        else {
            continue;
        };
        if let Event::ApprovalRequested {
            request_id,
            detail,
            available_decisions,
            ..
        } = &event
        {
            saw_dialog = true;
            said_read = detail.contains("Grant read access");
            said_write = detail.contains("write");
            decisions_ok = !available_decisions.contains(&ApprovalDecision::ApprovedForSession);
            agent
                .ops
                .send(Op::ApprovalReply {
                    request_id: request_id.clone(),
                    decision: ApprovalDecision::Allow,
                    feedback: None,
                })
                .await
                .unwrap();
        }
        let done = matches!(event, Event::TurnComplete { .. });
        if done {
            break;
        }
    }
    if let Some(previous) = previous {
        unsafe { std::env::set_var("USERPROFILE", previous) };
    } else {
        unsafe { std::env::remove_var("USERPROFILE") };
    }

    assert!(saw_dialog, "a credential read request prompts");
    assert!(said_read, "the dialog says the granted access is read");
    assert!(!said_write, "the dialog must not claim write access");
    assert!(decisions_ok, "no session-scoped option");
    agent.shutdown();
}

/// A read request for an ordinary path is redundant — reads are already
/// full-disk. The gate drops it, the command runs without a dialog, and the
/// guidance rides the tool output (no model round-trip over a rejection).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_redundant_read_request_runs_without_a_dialog() {
    let (config_path, dir, data_dir) = setup("redundant-read");
    let target = dir.join("ordinary.txt");
    std::fs::write(&target, "PLAIN-CONTENT").expect("write fixture");

    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let events = agent.events.clone();
    let session_id = new_session(&agent, dir.clone()).await;
    agent
        .ops
        .send(Op::SetPermissions {
            session_id: session_id.clone(),
            permissions: ExecMode::Auto.permissions(),
        })
        .await
        .unwrap();
    agent
        .ops
        .send(Op::SendMessage {
            session_id,
            content: format!(
                "{} {}",
                mock::CREDENTIAL_READ_TRIGGER,
                target
                    .to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/")
            ),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();

    let mut collected = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "timed out");
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await
        else {
            continue;
        };
        let done = matches!(event, Event::TurnComplete { .. });
        collected.push(event);
        if done {
            break;
        }
    }

    assert!(
        !collected
            .iter()
            .any(|event| matches!(event, Event::ApprovalRequested { .. })),
        "a redundant read request must not prompt: {collected:#?}"
    );
    let ends = tool_ends(&collected);
    if common::native_sandbox_usable() {
        assert!(
            ends.iter().any(|(_, output, is_error)| {
                output.contains("PLAIN-CONTENT")
                    && output.contains("was already allowed")
                    && !is_error
            }),
            "the command ran and the output carries the guidance: {ends:?}"
        );
    } else {
        assert!(
            ends.iter().any(|(_, output, _)| {
                common::native_sandbox_refusal(output) && output.contains("was already allowed")
            }),
            "a redundant request is dropped before the native sandbox refusal: {ends:?}"
        );
    }
    agent.shutdown();
}

/// A widening that names a path a deny rule covers is rejected upfront: the
/// approval could never take effect (deny rules stay on inside approved
/// roots), so no dialog is shown and the command never runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_widening_naming_a_denied_path_is_rejected_upfront() {
    let (config_path, dir, data_dir) = setup("denied-widening");
    // The seeded env group matches `.env` at any depth; the request names the
    // file itself as the root, so the denial covers the root. The path sits
    // inside the workspace on purpose: the doom check runs before the
    // redundancy drop, and the workspace is already writable anyway.
    let target = dir.join("secret").join(".env");
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(&target, "SENTINEL").unwrap();
    let target_token = target.to_string_lossy().replace('\\', "/");

    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let events = agent.events.clone();
    let session_id = new_session(&agent, dir.clone()).await;
    agent
        .ops
        .send(Op::SetPermissions {
            session_id: session_id.clone(),
            permissions: ExecMode::Auto.permissions(),
        })
        .await
        .unwrap();
    agent
        .ops
        .send(Op::SendMessage {
            session_id,
            content: format!("{} {target_token}", mock::DENIED_WIDENING_TRIGGER),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();

    let mut collected = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "timed out: {collected:#?}");
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await
        else {
            continue;
        };
        let done = matches!(event, Event::TurnComplete { .. });
        collected.push(event);
        if done {
            break;
        }
    }

    assert!(
        !collected
            .iter()
            .any(|event| matches!(event, Event::ApprovalRequested { .. })),
        "a doomed widening must not prompt: {collected:#?}"
    );
    let ends = tool_ends(&collected);
    assert!(
        ends.iter().any(|(_, output, is_error)| {
            output.contains("cannot lift deny rules") && *is_error
        }),
        "the rejection explains that approval cannot help: {ends:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "SENTINEL",
        "the command never ran: {collected:#?}"
    );
    agent.shutdown();
}

/// A plain `Read` of a credential file — no request syntax — is detected by the
/// gate and asks on its own; after approval the read works.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plain_read_of_a_credential_file_prompts_automatically() {
    let _env = credential_env_lock().await;
    let (config_path, dir, data_dir) = setup("credential-auto-read");
    let profile = dir.join("profile");
    std::fs::create_dir_all(profile.join(".ssh")).expect("create fake profile");
    let target = profile.join(".ssh").join("known_hosts");
    std::fs::write(&target, "KNOWN-HOSTS-ENTRY").expect("write fixture");
    // SAFETY: credential tests serialize on `credential_env_lock`.
    let previous = std::env::var_os("USERPROFILE");
    unsafe { std::env::set_var("USERPROFILE", &profile) };

    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let events = agent.events.clone();
    let session_id = new_session(&agent, dir.clone()).await;
    agent
        .ops
        .send(Op::SetPermissions {
            session_id: session_id.clone(),
            permissions: ExecMode::Auto.permissions(),
        })
        .await
        .unwrap();
    agent
        .ops
        .send(Op::SendMessage {
            session_id,
            content: format!(
                "{} {}",
                mock::CREDENTIAL_AUTO_READ_TRIGGER,
                target
                    .to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/")
            ),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();

    let mut saw_dialog = false;
    let mut said_read = false;
    let mut said_write = false;
    let mut decisions_ok = false;
    let mut read_content = false;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "timed out");
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await
        else {
            continue;
        };
        if let Event::ApprovalRequested {
            request_id,
            detail,
            available_decisions,
            ..
        } = &event
        {
            saw_dialog = true;
            said_read = detail.starts_with("Read ") && detail.contains("only read access");
            said_write = detail.contains("Replace ") || detail.contains("write");
            decisions_ok = !available_decisions.contains(&ApprovalDecision::ApprovedForSession);
            agent
                .ops
                .send(Op::ApprovalReply {
                    request_id: request_id.clone(),
                    decision: ApprovalDecision::Allow,
                    feedback: None,
                })
                .await
                .unwrap();
        }
        if let Event::ToolCallEnd { output, .. } = &event
            && output.contains("KNOWN-HOSTS-ENTRY")
        {
            read_content = true;
        }
        let done = matches!(event, Event::TurnComplete { .. });
        if done {
            break;
        }
    }
    if let Some(previous) = previous {
        unsafe { std::env::set_var("USERPROFILE", previous) };
    } else {
        unsafe { std::env::remove_var("USERPROFILE") };
    }

    assert!(
        saw_dialog,
        "a plain credential read must prompt automatically"
    );
    assert!(said_read, "the dialog asks to read");
    assert!(!said_write, "the dialog must not claim write access");
    assert!(decisions_ok, "no session-scoped option");
    assert!(read_content, "after approval the read succeeds");
    agent.shutdown();
}

/// The same auto-detection through a symlink: the gate resolves the read
/// target to the credential file, the dialog names that file (not the link),
/// and the per-call grant it records matches the object the tool opens.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_symlinked_credential_read_prompts_for_the_real_target() {
    let _env = credential_env_lock().await;
    let (config_path, dir, data_dir) = setup("credential-symlink-read");
    let profile = dir.join("profile");
    std::fs::create_dir_all(profile.join(".ssh")).expect("create fake profile");
    let target = profile.join(".ssh").join("known_hosts");
    std::fs::write(&target, "KNOWN-HOSTS-ENTRY").expect("write fixture");
    let link = dir.join("innocent.txt");
    std::os::unix::fs::symlink(&target, &link).expect("create link");
    // SAFETY: credential tests serialize on `credential_env_lock`.
    let previous = std::env::var_os("USERPROFILE");
    unsafe { std::env::set_var("USERPROFILE", &profile) };

    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let events = agent.events.clone();
    let session_id = new_session(&agent, dir.clone()).await;
    agent
        .ops
        .send(Op::SetPermissions {
            session_id: session_id.clone(),
            permissions: ExecMode::Auto.permissions(),
        })
        .await
        .unwrap();
    agent
        .ops
        .send(Op::SendMessage {
            session_id,
            content: format!(
                "{} {}",
                mock::CREDENTIAL_AUTO_READ_TRIGGER,
                link.to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/")
            ),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();

    let mut saw_dialog = false;
    let mut named_target = false;
    let mut named_link = false;
    let mut read_content = false;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "timed out");
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await
        else {
            continue;
        };
        if let Event::ApprovalRequested {
            request_id, detail, ..
        } = &event
        {
            saw_dialog = true;
            named_target = detail.contains(
                &target
                    .to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/"),
            ) || detail.contains(&target.to_string_lossy().to_string());
            named_link = detail.contains("innocent.txt");
            agent
                .ops
                .send(Op::ApprovalReply {
                    request_id: request_id.clone(),
                    decision: ApprovalDecision::Allow,
                    feedback: None,
                })
                .await
                .unwrap();
        }
        if let Event::ToolCallEnd { output, .. } = &event
            && output.contains("KNOWN-HOSTS-ENTRY")
        {
            read_content = true;
        }
        if matches!(event, Event::TurnComplete { .. }) {
            break;
        }
    }
    if let Some(previous) = previous {
        unsafe { std::env::set_var("USERPROFILE", previous) };
    } else {
        unsafe { std::env::remove_var("USERPROFILE") };
    }

    assert!(saw_dialog, "the canonical credential target must prompt");
    assert!(
        named_target,
        "the dialog must name the object actually granted"
    );
    assert!(!named_link, "the link spelling is not the approved object");
    assert!(read_content, "after approval the linked read succeeds");
    agent.shutdown();
}

/// Windows counterpart of the symlink approval: a junction (no elevation
/// needed) resolves to the fake credential directory, so the dialog must name
/// the real file and the per-call grant must open that object, not the alias.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_junctioned_credential_read_prompts_for_the_real_target() {
    let _env = credential_env_lock().await;
    let (config_path, dir, data_dir) = setup("credential-junction-read");
    let profile = dir.join("profile");
    let store = profile.join(".ssh");
    std::fs::create_dir_all(&store).expect("create fake profile");
    let target = store.join("known_hosts");
    std::fs::write(&target, "KNOWN-HOSTS-ENTRY").expect("write fixture");
    common::junction(&dir.join("innocent-dir"), &store);
    // SAFETY: credential tests serialize on `credential_env_lock`.
    let previous = std::env::var_os("USERPROFILE");
    unsafe { std::env::set_var("USERPROFILE", &profile) };

    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let events = agent.events.clone();
    let session_id = new_session(&agent, dir.clone()).await;
    agent
        .ops
        .send(Op::SetPermissions {
            session_id: session_id.clone(),
            permissions: ExecMode::Auto.permissions(),
        })
        .await
        .unwrap();
    let alias = dir.join("innocent-dir").join("known_hosts");
    agent
        .ops
        .send(Op::SendMessage {
            session_id,
            content: format!(
                "{} {}",
                mock::CREDENTIAL_AUTO_READ_TRIGGER,
                alias.to_string_lossy().replace('\\', "/")
            ),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();

    let mut saw_dialog = false;
    let mut named_target = false;
    let mut named_alias = false;
    let mut read_content = false;
    let mut saw_native_refusal = false;
    let mut request_details = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "timed out");
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await
        else {
            continue;
        };
        if let Event::ApprovalRequested {
            request_id, detail, ..
        } = &event
        {
            saw_dialog = true;
            request_details.push(detail.clone());
            named_target = detail.contains(&target.to_string_lossy().replace('\\', "/"))
                || detail.contains(&target.to_string_lossy().to_string());
            named_alias = detail.contains("innocent-dir");
            agent
                .ops
                .send(Op::ApprovalReply {
                    request_id: request_id.clone(),
                    decision: ApprovalDecision::Allow,
                    feedback: None,
                })
                .await
                .unwrap();
        }
        if let Event::ToolCallEnd { output, .. } = &event {
            read_content |= output.contains("KNOWN-HOSTS-ENTRY");
            saw_native_refusal |= common::native_sandbox_refusal(output);
        }
        if matches!(event, Event::TurnComplete { .. }) {
            break;
        }
    }
    if let Some(previous) = previous {
        unsafe { std::env::set_var("USERPROFILE", previous) };
    } else {
        unsafe { std::env::remove_var("USERPROFILE") };
    }

    assert!(saw_dialog, "the junctioned credential target must prompt");
    assert!(
        named_target,
        "the dialog must name the object actually granted: {request_details:?}"
    );
    assert!(
        !named_alias,
        "the junction spelling is not the approved object"
    );
    if common::native_sandbox_usable() {
        assert!(read_content, "after approval the junctioned read succeeds");
    } else {
        assert!(saw_native_refusal, "the junctioned read fails closed");
        assert!(!read_content, "the refused read cannot reveal the target");
    }
    agent.shutdown();
}

/// A read request naming a credential **directory** is the listing approval:
/// the dialog says "List the file names in …", approval reveals entry names,
/// and contents stay denied — including through `ls -la`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_credential_directory_listing_is_approvable() {
    let _env = credential_env_lock().await;
    let (config_path, dir, data_dir) = setup("credential-listing");
    let profile = dir.join("profile");
    let store = profile.join(".ssh");
    std::fs::create_dir_all(&store).expect("create fake profile");
    std::fs::write(store.join("known_hosts"), "KNOWN-HOSTS").unwrap();
    std::fs::write(store.join("config"), "HOST-CONFIG").unwrap();
    // A private key in the store: its entry makes `ls -la` exit non-zero with
    // "Operation not permitted" (the name group denies the stat), which used to
    // trigger a doomed sandbox retry that dropped the listing grant.
    std::fs::write(store.join("id_rsa"), "PRIVATE-KEY").unwrap();
    // SAFETY: credential tests serialize on `credential_env_lock`.
    let previous = std::env::var_os("USERPROFILE");
    unsafe { std::env::set_var("USERPROFILE", &profile) };

    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let events = agent.events.clone();
    let session_id = new_session(&agent, dir.clone()).await;
    agent
        .ops
        .send(Op::SetPermissions {
            session_id: session_id.clone(),
            permissions: ExecMode::Auto.permissions(),
        })
        .await
        .unwrap();
    agent
        .ops
        .send(Op::SendMessage {
            session_id,
            content: format!(
                "{} {}",
                mock::CREDENTIAL_LISTING_TRIGGER,
                store
                    .to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/")
            ),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();

    let mut saw_dialog = false;
    let mut approvals = 0usize;
    let mut said_listing = false;
    let mut saw_names = false;
    let mut saw_contents = false;
    let mut saw_native_refusal = false;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "timed out");
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await
        else {
            continue;
        };
        if let Event::ApprovalRequested {
            request_id, detail, ..
        } = &event
        {
            saw_dialog = true;
            approvals += 1;
            said_listing = detail.contains("List the file names");
            agent
                .ops
                .send(Op::ApprovalReply {
                    request_id: request_id.clone(),
                    decision: ApprovalDecision::Allow,
                    feedback: None,
                })
                .await
                .unwrap();
        }
        if let Event::ToolCallEnd { output, .. } = &event {
            saw_names |=
                output.contains("known_hosts") && !output.contains("Operation not permitted");
            saw_contents |= output.contains("HOST-CONFIG");
            saw_native_refusal |= common::native_sandbox_refusal(output);
        }
        if matches!(event, Event::TurnComplete { .. }) {
            break;
        }
    }
    if let Some(previous) = previous {
        unsafe { std::env::set_var("USERPROFILE", previous) };
    } else {
        unsafe { std::env::remove_var("USERPROFILE") };
    }

    assert!(saw_dialog, "the directory read request prompts");
    assert_eq!(
        approvals, 1,
        "no doomed retry prompt: the listing grant was the whole approval"
    );
    assert!(said_listing, "the dialog offers the listing");
    if common::native_sandbox_usable() {
        assert!(saw_names, "entry names are visible after approval");
    } else {
        assert!(saw_native_refusal, "the listing command fails closed");
        assert!(!saw_names, "the refused listing reveals no names");
    }
    assert!(!saw_contents, "contents stay denied");
    agent.shutdown();
}
