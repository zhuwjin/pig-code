use super::compact::{
    CompactionSelection, active_run_start, compactable_tokens, select_tail_plan, tail_budget,
};
use super::estimate::estimate_leading_system_tokens;
use super::*;

mod notifications;

fn user() -> RolloutRecord {
    RolloutRecord::User {
        sent_at_ms: 0,
        text: "hi".to_string(),
        files: vec![],
        images: vec![],
        sent_text: None,
        sent_images: vec![],
        model_text: None,
        sent_input_image: None,
    }
}

fn step(input: u64, cache_read: u64, output: u64) -> RolloutRecord {
    RolloutRecord::StepUsage {
        input,
        cache_read,
        output,
        used: 0,
        turn_id: None,
    }
}

fn stats() -> RolloutRecord {
    RolloutRecord::TurnStats {
        input: 1,
        cache_read: 1,
        output: 1,
        duration_ms: 0,
        ended_at_ms: 0,
        api_ms: 0,
        ttft_ms: 0,
        ttft_steps: 0,
        api_steps: 1,
        tool_ms: 0,
        has_usage: true,
        finish_reason: None,
        turn_id: None,
    }
}

#[test]
fn crashed_tail_turn_sums_step_usage() {
    // Completed turn (counted) + crashed tail turn → only the tail sums.
    // Legacy steps without a turn_id get a stable record-index identity so
    // a repeated failed recovery append cannot double count in SQLite.
    let records = vec![
        user(),
        step(5, 5, 5),
        stats(),
        user(),
        step(1, 2, 3),
        step(4, 5, 6),
    ];
    let expected = (5, 7, 9, Some("recovered-4".to_string()));
    assert_eq!(uncounted_tail_usage(&records), Some(expected.clone()));
    assert_eq!(
        uncounted_tail_usage(&records),
        Some(expected),
        "the derived identity is stable across reopens"
    );

    // A modern step's durable identity wins over the derived fallback.
    let mut modern = records.clone();
    modern[4] = RolloutRecord::StepUsage {
        input: 1,
        cache_read: 2,
        output: 3,
        used: 0,
        turn_id: Some("t1-1".into()),
    };
    assert_eq!(
        uncounted_tail_usage(&modern),
        Some((5, 7, 9, Some("t1-1".to_string())))
    );
}

/// Reopen tolerance: one malformed persisted row is skipped instead of
/// discarding the queue, and a corrupt image degrades only that attachment
/// while the queued text survives.
#[test]
fn restore_pending_keeps_valid_items_and_degrades_corrupt_images() {
    let dir = std::env::temp_dir().join(format!("pig-core-pending-restore-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let store = Arc::new(Mutex::new(Store::open(&dir).expect("store")));
    let permissions = serde_json::to_value(PermissionSettings::default()).expect("permissions");
    let item = |id: &str, text: &str, images: Vec<serde_json::Value>| {
        serde_json::json!({
            "id": id,
            "text": text,
            "files": [],
            "images": images,
            "permissions": permissions,
            "steer": false,
            "recovered": false,
        })
    };
    let pending = serde_json::json!([
        item("q1", "first survives", vec![]),
        // A malformed row without the required input fields is skipped.
        {"id": "q9"},
        item(
            "q2",
            "text survives the bad image",
            vec![serde_json::json!({
                "bytes_base64": "!!!not-base64!!!",
                "mime": "image/png",
            })],
        ),
    ]);
    store
        .lock()
        .expect("store")
        .set_pending("s1", &pending.to_string());
    let restored = restore_pending(&store, "s1");
    assert_eq!(
        restored.len(),
        2,
        "the malformed row is skipped, not the whole queue"
    );
    assert_eq!(restored[0].id, "q1");
    assert_eq!(restored[0].text, "first survives");
    assert_eq!(restored[1].id, "q2");
    assert_eq!(restored[1].text, "text survives the bad image");
    assert!(
        restored[1].images.is_empty(),
        "only the corrupt attachment is dropped"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Restored queue ids are user data: an empty, foreign or multibyte id must
/// not panic the reopen path or reset the counter below a valid id.
#[test]
fn restored_queue_seq_ignores_malformed_ids() {
    let message = |id: &str| PendingMessage {
        id: id.to_string(),
        text: String::new(),
        files: vec![],
        images: vec![],
        permissions: PermissionSettings::default(),
        steer: false,
        held: false,
        recovered: false,
    };
    let restored = vec![
        message("q7"),
        message(""),
        message("…"),
        message("not-a-queue-id"),
        message("q12"),
        message("q999999999999999999999999"),
    ];
    assert_eq!(restored_queue_seq(&restored), 12);
    assert_eq!(restored_queue_seq(&[]), 0);
}

#[test]
fn counted_tail_turn_is_skipped() {
    // Normal end and live interrupt both close with TurnStats
    let records = vec![user(), step(1, 2, 3), stats()];
    assert_eq!(uncounted_tail_usage(&records), None);
}

#[test]
fn usage_free_tail_is_skipped() {
    // Interrupted before any usage arrived; empty (never messaged) session
    assert_eq!(uncounted_tail_usage(&[user()]), None);
    assert_eq!(uncounted_tail_usage(&[]), None);
}

fn snapshot(date: &str) -> crate::rollout::PromptSnapshot {
    crate::rollout::PromptSnapshot {
        text: "system".into(),
        git_snapshot: None,
        agents_prompt: String::new(),
        skills_prompt: String::new(),
        skills_state: String::new(),
        memory_prompt: String::new(),
        memory_enabled: false,
        profiles_summary: String::new(),
        date_frozen: "2026-01-01".into(),
        reminders: crate::rollout::PromptReminderState {
            date: date.into(),
            agents: String::new(),
            memory: String::new(),
            permissions: None,
        },
    }
}

#[test]
fn recalled_turn_prompt_snapshot_is_not_restored() {
    let records = vec![
        user(),
        RolloutRecord::PromptSnapshot {
            snapshot: snapshot("first"),
        },
        user(),
        RolloutRecord::PromptSnapshot {
            snapshot: snapshot("second"),
        },
        RolloutRecord::Recall,
    ];
    let surviving = surviving_prompt_snapshot(&records).expect("first snapshot survives");
    assert_eq!(surviving.reminders.date, "first");
    // Without the recall the latest snapshot still wins.
    let mut no_recall = records.clone();
    no_recall.pop();
    assert_eq!(
        surviving_prompt_snapshot(&no_recall)
            .expect("latest snapshot")
            .reminders
            .date,
        "second"
    );
    // A recall that removes the only turn leaves no snapshot to restore.
    assert!(
        surviving_prompt_snapshot(&[
            user(),
            RolloutRecord::PromptSnapshot {
                snapshot: snapshot("only")
            },
            RolloutRecord::Recall
        ])
        .is_none()
    );
}

#[test]
fn crashed_tail_turn_keeps_its_durable_turn_identity() {
    let records = vec![
        user(),
        RolloutRecord::StepUsage {
            input: 10,
            cache_read: 2,
            output: 3,
            used: 12,
            turn_id: Some("turn-x".into()),
        },
    ];
    assert_eq!(
        uncounted_tail_usage(&records),
        Some((10, 2, 3, Some("turn-x".into())))
    );
}

/// The pure compaction API is the child-JSONL entry point: no Session or
/// rollout state is needed to derive the same boundary and preserved-user
/// plan.
#[test]
fn child_compaction_api_is_reusable() {
    let history = vec![
        ChatMsg::system("child system".into()),
        ChatMsg::user("task".into()),
        ChatMsg::assistant("old answer".into(), vec![], None),
        ChatMsg::user("current".into()),
    ];
    let leading = estimate_leading_system_tokens(&history);
    let budget = tail_budget(64_000, 4_000, leading, 1_000, 1_000);
    let target = budget.min(compactable_tokens(&history) / 2);
    let plan = select_tail_plan(
        &history,
        CompactionSelection {
            target_tokens: target,
            pin: None,
            keep_latest_segment: false,
            active_user: Some(3),
            active_run_start: active_run_start(&history, Some(3)),
        },
    )
    .expect("child plan");
    assert_eq!(plan.tail_start, 3);
    assert_eq!(plan.tail.len(), 1);
    assert_eq!(plan.preserved_user, None);
    assert_eq!(plan.omitted, 2);
}

const PROJECT_SKILL_MARKER: &str = "PROJECT_SKILL_TRUST_BOUNDARY_71C";
const PROJECT_PROFILE_MARKER: &str = "PROJECT_PROFILE_TRUST_BOUNDARY_71C";
const PROJECT_PROMPT_TEXT: &str = "PROJECT PROMPT TEXT 71C";

/// A fixture path that is absolute on the host running the tests
/// (`/data/...` is relative on Windows, so the rule validator drops it).
fn host_path(rest: &str) -> String {
    if cfg!(windows) {
        format!("C:/{rest}")
    } else {
        format!("/{rest}")
    }
}

/// A real Session workspace with one project surface of every kind: skill,
/// profile, permission allow, hook and sandbox overlay.
struct TrustFixture {
    dir: PathBuf,
    workspace: PathBuf,
    data_dir: PathBuf,
    sessions_dir: PathBuf,
    config_path: PathBuf,
}

impl TrustFixture {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "pig-core-trust-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        let workspace = dir.join("workspace");
        let data_dir = dir.join("data");
        let sessions_dir = data_dir.join("sessions");
        for path in [
            workspace.join(".pigcode/skills/proj-skill"),
            workspace.join(".pigcode/agents"),
            data_dir.clone(),
            sessions_dir.clone(),
        ] {
            std::fs::create_dir_all(&path).expect("fixture directory");
        }
        std::fs::write(
            workspace.join(".pigcode/skills/proj-skill/SKILL.md"),
            format!("---\nname: proj-skill\ndescription: {PROJECT_SKILL_MARKER}\n---\n\nbody\n"),
        )
        .expect("project skill");
        std::fs::write(
            workspace.join(".pigcode/agents/proj-agent.md"),
            format!(
                "---\nname: proj-agent\ndescription: {PROJECT_PROFILE_MARKER}\ntools: Read\n---\n\nbody\n"
            ),
        )
        .expect("project profile");
        std::fs::write(
            workspace.join(".pigcode/permissions.toml"),
            "allow = [\"Write(project-allowed.txt)\"]\n",
        )
        .expect("project permissions");
        std::fs::write(
            workspace.join(".pigcode/hooks.toml"),
            "[[hooks.pre_tool_use]]\ncommand = \"true\"\nmatcher = \"Write\"\n",
        )
        .expect("project hooks");
        std::fs::write(
            workspace.join(".pigcode/config.toml"),
            format!(
                "[sandbox.filesystem]\n\"{}\" = \"deny\"\n",
                host_path("data/vault")
            ),
        )
        .expect("project config");
        Self {
            config_path: dir.join("config.toml"),
            dir,
            workspace,
            data_dir,
            sessions_dir,
        }
    }

    /// Write the user-level config with or without the workspace trust entry.
    /// The project surface files stay on disk either way, exactly like a
    /// revoke from Settings.
    fn write_user(&self, trusted: bool) {
        let body = if trusted {
            let canonical = self.workspace.canonicalize().expect("canonical workspace");
            format!(
                "[hooks]\nenabled = true\ntrusted_projects = [{:?}]\n",
                canonical.display().to_string()
            )
        } else {
            "[hooks]\nenabled = true\n".to_string()
        };
        std::fs::write(&self.config_path, body).expect("user config");
    }

    fn runtime(&self, trusted: bool) -> Arc<crate::config::RuntimeConfig> {
        self.write_user(trusted);
        Arc::new(
            crate::config::RuntimeConfig::load(
                &self.config_path,
                &self.workspace,
                std::sync::Arc::new(crate::credentials::CredentialManager::mock()),
            )
            .expect("layered runtime"),
        )
    }

    fn store(&self) -> Arc<Mutex<Store>> {
        Arc::new(Mutex::new(Store::open(&self.data_dir).expect("store")))
    }

    fn create(&self, runtime: Option<Arc<crate::config::RuntimeConfig>>) -> Session {
        let meta = SessionMeta {
            id: "s1".to_string(),
            title: String::new(),
            title_custom: false,
            cwd: self.workspace.clone(),
            created_at: 0,
            updated_at: 0,
            pinned: false,
            archived: false,
            provider_id: None,
            model_id: None,
            reasoning_level: None,
            permissions: PermissionSettings::default(),
            plan_enabled: false,
        };
        let (task_tx, _) = tokio::sync::mpsc::unbounded_channel();
        Session::create(
            meta,
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashMap::new())),
            self.store(),
            &self.sessions_dir,
            self.data_dir.clone(),
            task_tx,
            &crate::plugins::PluginRegistry::shared(self.data_dir.clone()),
            runtime,
        )
        .expect("create session")
    }

    fn load(&self, runtime: Option<Arc<crate::config::RuntimeConfig>>) -> Session {
        let (task_tx, _) = tokio::sync::mpsc::unbounded_channel();
        Session::load(
            "s1",
            &self.sessions_dir,
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashMap::new())),
            self.store(),
            self.data_dir.clone(),
            task_tx,
            &crate::plugins::PluginRegistry::shared(self.data_dir.clone()),
            runtime,
        )
        .expect("load session")
        .0
    }

    fn assert_trusted_surfaces(&self, session: &Session) {
        assert!(session.trust.admits(&self.workspace));
        assert!(
            session
                .permission_rules
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .allow_hit("Write", "project-allowed.txt"),
            "the project allow rule applies under trust"
        );
        assert!(
            session.skills_prompt.contains(PROJECT_SKILL_MARKER),
            "the project skill enters the prompt listing"
        );
        assert!(
            session.profiles_summary.contains(PROJECT_PROFILE_MARKER),
            "the project profile enters the profile listing"
        );
        assert!(
            session.hooks.pending_project().is_none(),
            "project hooks run under trust"
        );
        assert!(
            session.pending_project_trust().is_none(),
            "no trust banner is due for a trusted workspace"
        );
        assert!(
            !session
                .state
                .sandbox_project
                .read()
                .expect("project layer")
                .is_empty(),
            "the project sandbox overlay is installed"
        );
        assert!(session.mcp.is_none(), "MCP reconnects lazily");
    }

    fn assert_untrusted_surfaces(&self, session: &Session) {
        assert!(!session.trust.admits(&self.workspace));
        assert!(
            !session
                .permission_rules
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .allow_hit("Write", "project-allowed.txt"),
            "an untrusted project allow must not apply"
        );
        assert!(
            !session.skills_prompt.contains(PROJECT_SKILL_MARKER),
            "the project skill listing is gone"
        );
        assert!(
            !session.profiles_summary.contains(PROJECT_PROFILE_MARKER),
            "the project profile listing is gone"
        );
        assert!(
            session.hooks.pending_project().is_some(),
            "the untrusted project file raises the banner again"
        );
        assert!(
            session
                .pending_project_trust()
                .is_some_and(|(_, count)| count >= 5),
            "the trust entry counts hooks, skills, profiles, permissions and config"
        );
        assert!(
            session
                .state
                .sandbox_project
                .read()
                .expect("project layer")
                .is_empty(),
            "the project sandbox overlay is cleared"
        );
    }
}

impl Drop for TrustFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The one trust boundary refreshes every project surface on grant and revoke;
/// `None` clears the trust token and the project overlay instead of leaving a
/// stale layer behind.
#[test]
fn apply_trust_refreshes_every_project_surface_for_grant_and_revoke() {
    let fixture = TrustFixture::new("apply");
    let trusted = fixture.runtime(true);
    let mut session = fixture.create(Some(trusted));
    fixture.assert_trusted_surfaces(&session);

    // Any cached prompt is invalid on a decision change.
    session.prompt_text = Some("stale trusted prompt".to_string());
    session.prompt_skills_state = Some("trusted:stale".to_string());

    let untrusted = fixture.runtime(false);
    session.apply_trust(Some(untrusted));
    fixture.assert_untrusted_surfaces(&session);
    assert!(
        session.prompt_text.is_none() && session.prompt_skills_state.is_none(),
        "the frozen prompt is invalidated on revoke"
    );

    let trusted = fixture.runtime(true);
    session.apply_trust(Some(trusted));
    fixture.assert_trusted_surfaces(&session);
    assert!(
        session.prompt_text.is_none(),
        "the frozen prompt is invalidated on grant too"
    );

    session.apply_trust(None);
    assert!(
        !session.trust.admits(&fixture.workspace) && session.runtime.is_none(),
        "no runtime means no trust token"
    );
    assert!(
        session
            .state
            .sandbox_project
            .read()
            .expect("project layer")
            .is_empty(),
        "no runtime means no project sandbox overlay"
    );
}

/// A same-trust runtime install (provider/config save, turn boundary) must not
/// churn the frozen prompt; `None` must clear the trust token and overlay.
#[test]
fn set_runtime_refreshes_only_on_a_trust_change() {
    let fixture = TrustFixture::new("set-runtime");
    let trusted = fixture.runtime(true);
    let mut session = fixture.create(Some(trusted));
    session.prompt_text = Some("cached prompt".to_string());
    session.prompt_skills_state = Some("cached state".to_string());

    let trusted = fixture.runtime(true);
    session.set_runtime(Some(trusted));
    assert_eq!(
        session.prompt_text.as_deref(),
        Some("cached prompt"),
        "a same-trust runtime install keeps the prompt cache"
    );
    fixture.assert_trusted_surfaces(&session);

    session.set_runtime(None);
    assert!(
        !session.trust.admits(&fixture.workspace) && session.runtime.is_none(),
        "no runtime clears the stale trust token"
    );
    assert!(
        session
            .state
            .sandbox_project
            .read()
            .expect("project layer")
            .is_empty(),
        "no runtime clears the project sandbox overlay"
    );
    assert!(session.prompt_text.is_none());
}

/// A model/window switch reprices the live history's window-derived PDF
/// reserves only. A measured page-tree cost is window-independent and is not
/// touched; no document bytes are read here.
#[test]
fn reprice_document_estimates_only_touches_window_fallbacks() {
    let fixture = TrustFixture::new("reprice");
    let runtime = fixture.runtime(true);
    let mut session = fixture.create(Some(runtime));
    let document = |estimate| pig_provider::ChatDocument {
        media_type: "application/pdf".into(),
        data_base64: "unused".into(),
        filename: "doc.pdf".into(),
        label: None,
        token_estimate: estimate,
    };
    let mut user = ChatMsg::user("documents".into());
    user.documents = vec![
        document(Some(pig_provider::DocumentTokenEstimate::measured(6_000))),
        document(Some(pig_provider::DocumentTokenEstimate::window_fallback(
            32_000, 128_000,
        ))),
        document(Some(pig_provider::DocumentTokenEstimate::measured(3_000))),
    ];
    session.agent.history = vec![ChatMsg::system("sys".into()), user];

    session.reprice_document_estimates(8_000);

    assert_eq!(
        session.agent.history[1].documents[0].token_estimate,
        Some(pig_provider::DocumentTokenEstimate::measured(6_000)),
        "a measured cost never changes with the window"
    );
    assert_eq!(
        session.agent.history[1].documents[1].token_estimate,
        Some(pig_provider::DocumentTokenEstimate::window_fallback(
            pig_utils::pdf::unknown_document_token_reserve(8_000),
            8_000
        )),
        "the fallback is repriced from its tag"
    );
    assert_eq!(
        session.agent.history[1].documents[2].token_estimate,
        Some(pig_provider::DocumentTokenEstimate::measured(3_000)),
        "every measured cost is preserved"
    );
}

/// A PromptSnapshot is bound to the trust decision that produced it: a
/// trusted-create → revoke → resume cycle must rebuild the prompt, skill
/// listing and profile listing rather than restoring project content.
#[test]
fn a_revoked_prompt_snapshot_is_not_restored_on_resume() {
    let fixture = TrustFixture::new("snapshot");
    let trusted = fixture.runtime(true);
    let mut session = fixture.create(Some(trusted));
    session.record(&user());
    session.prompt_text = Some(PROJECT_PROMPT_TEXT.to_string());
    session.prompt_memory_enabled = Some(session.memory_enabled);
    session.record_prompt_snapshot(PROJECT_PROMPT_TEXT);
    session.sync_rollout();
    drop(session);

    // Control: the same trust decision restores the cached prompt and both
    // project listings.
    let trusted = fixture.runtime(true);
    let resumed = fixture.load(Some(trusted));
    assert_eq!(resumed.prompt_text.as_deref(), Some(PROJECT_PROMPT_TEXT));
    assert!(resumed.profiles_summary.contains(PROJECT_PROFILE_MARKER));
    assert!(resumed.skills_prompt.contains(PROJECT_SKILL_MARKER));
    drop(resumed);

    // Revoke in the user config, then reopen the same session: no project
    // prompt text, skills or profiles may come back from the snapshot.
    let untrusted = fixture.runtime(false);
    let revoked = fixture.load(Some(untrusted));
    assert!(
        revoked.prompt_text.is_none(),
        "the trusted prompt text must not be restored after revocation"
    );
    assert!(
        revoked.recorded_prompt.is_none(),
        "the snapshot is not adopted as the current prompt state"
    );
    assert!(!revoked.profiles_summary.contains(PROJECT_PROFILE_MARKER));
    assert!(!revoked.skills_prompt.contains(PROJECT_SKILL_MARKER));
    assert!(
        revoked.prompt_skills_state.is_none(),
        "the snapshot's trusted skills state is not adopted"
    );
}
