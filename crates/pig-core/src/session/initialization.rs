use super::*;

/// Session inputs frozen once at the start boundary (fresh or resumed):
/// workspace trust, the rendered prompt sections and memory state. Captured in
/// one place so create/load cannot drift apart field by field.
struct SessionFrozen {
    trust: crate::config::WorkspaceTrust,
    plugin_tools: crate::plugins::PluginToolset,
    skills_prompt: String,
    agents_prompt: String,
    memory_enabled: bool,
    memory_rw_roots: Vec<PathBuf>,
    memory_prompt: String,
    profiles_summary: String,
    today: String,
}

impl SessionFrozen {
    fn capture(
        session_id: &str,
        cwd: &Path,
        data_dir: &Path,
        plugins: &crate::plugins::SharedPluginRegistry,
        runtime: Option<&crate::config::RuntimeConfig>,
        bot: Option<&pig_protocol::BotProfile>,
        global: bool,
    ) -> Self {
        let app_config = runtime.map(|runtime| runtime.config());
        let trust = runtime
            .map(|runtime| runtime.trust().clone())
            .unwrap_or_else(|| crate::config::WorkspaceTrust::untrusted(cwd));
        let skills_enabled = bot.is_none_or(|b| b.capabilities.skills);
        let skills_prompt = if skills_enabled {
            crate::plugins::skills_section_with_trust(
                cwd,
                plugins,
                !global && trust.admits(cwd),
            )
        } else {
            String::new()
        };
        let agents_prompt = if global {
            String::new()
        } else {
            prompt::agents_md(data_dir, cwd)
        };
        let bot_id = bot.map(|b| b.bot_id.as_str());
        let workspace_cwd = (!global).then_some(cwd);
        let memory_enabled =
            app_config.is_some_and(|config| config.memory.enabled) || bot.is_some();
        if memory_enabled {
            // The directories must genuinely exist before the prompt says
            // "this directory already exists" (also makes the RW roots
            // canonicalizable for the path gates).
            crate::memory::ensure_layered_initialized(data_dir, bot_id, workspace_cwd, None);
        }
        let memory_rw_roots = if memory_enabled {
            crate::memory::layered_memory_roots(data_dir, bot_id, workspace_cwd, None)
        } else {
            vec![]
        };
        let memory_prompt = if memory_enabled {
            crate::memory::layered_memory_section_raw(data_dir, bot_id, workspace_cwd, None)
        } else {
            String::new()
        };
        let profiles_summary = crate::agent::agent_description_list(
            &crate::agent::load_profiles_with_plugins(cwd, data_dir, &trust, plugins),
        );
        let plugin_tools =
            crate::plugins::PluginToolset::new(plugins, &trust, app_config.cloned(), session_id);
        Self {
            trust,
            plugin_tools,
            skills_prompt,
            agents_prompt,
            memory_enabled,
            memory_rw_roots,
            memory_prompt,
            profiles_summary,
            today: prompt::today(),
        }
    }
}

/// Everything that differs between a fresh session and a resumed one. The
/// shared assembler owns every field that is not part of this variation, so a
/// new field is initialized once instead of in two near-identical literals.
struct SessionParts {
    id: String,
    cwd: PathBuf,
    history: Vec<ChatMsg>,
    user_ixs: Vec<Option<usize>>,
    tracker: ChangeTracker,
    rollout: Rollout,
    rollout_failure: Option<CoreError>,
    pending: PendingApprovals,
    pending_questions: PendingQuestions,
    store: Arc<Mutex<Store>>,
    data_dir: PathBuf,
    task_notify: async_channel::Sender<crate::task::TaskChange>,
    runtime: Option<Arc<crate::config::RuntimeConfig>>,
    session_start_source: &'static str,
    bot: Option<pig_protocol::BotProfile>,
    global: bool,
}

fn assemble_session(parts: SessionParts, frozen: SessionFrozen) -> Session {
    let root_conversation_path = parts.rollout.path().to_path_buf();
    let app_config = parts.runtime.as_ref().map(|runtime| runtime.config());
    let SessionFrozen {
        trust,
        plugin_tools,
        skills_prompt,
        agents_prompt,
        memory_enabled,
        memory_rw_roots,
        memory_prompt,
        profiles_summary,
        today,
    } = frozen;
    let state = crate::task::SessionToolState::new(
        parts.id.clone(),
        parts.task_notify,
        vec![parts.data_dir.join("sessions")],
        memory_rw_roots,
    )
    .with_notification_store(parts.store.clone(), HashMap::new())
    .with_data_dir(parts.data_dir.clone());
    {
        let mut known = state
            .known_agents_md
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for path in prompt::baseline_agents_md_paths(&parts.data_dir, &parts.cwd) {
            known.insert(path);
        }
    }
    let session = Session {
        delivery_receipts: state.delivery_receipts.clone(),
        state,
        permission_rules: Arc::new(Mutex::new(load_permissions(
            &parts.cwd,
            &parts.data_dir,
            &trust,
        ))),
        git_snapshot: if parts.global {
            None
        } else {
            prompt::git_snapshot(&parts.cwd)
        },
        hooks: crate::hooks::HookSet::load_with_plugins(
            &parts.cwd,
            app_config,
            &plugin_tools.registry(),
            &trust,
        ),
        agent: Agent {
            id: parts.id,
            run: None,
            cwd: parts.cwd,
            delegation_depth: 0,
            config: None,
            provider_session: pig_provider::ProviderSession::new(),
            history: parts.history,
            tools: Vec::new(),
            schemas: Vec::new(),
        },
        subagent: None,
        user_ixs: parts.user_ixs,
        io_last_input: Vec::new(),
        tracker: parts.tracker,
        always_allowed: Arc::new(Mutex::new(HashSet::new())),
        plan_enabled: false,
        reviewer: Arc::new(UserApprovalReviewer::new(parts.pending.clone())),
        pending: parts.pending,
        pending_questions: parts.pending_questions,
        permissions: PermissionSettings::default(),
        model_override: None,
        rollout: Some(parts.rollout),
        rollout_failure: parts.rollout_failure,
        rollout_failure_reported: false,
        notification_delivery_failure: None,
        notification_reconcile_required: false,
        store: parts.store,
        data_dir: parts.data_dir,
        agents_prompt: agents_prompt.clone(),
        skills_prompt,
        memory_prompt,
        memory_enabled,
        date_frozen: today.clone(),
        date_reminded: today,
        agents_reminded: agents_prompt,
        memory_reminded: String::new(),
        permissions_reminded: None,
        profiles_summary,
        prompt_text: None,
        prompt_memory_enabled: None,
        prompt_skills_state: None,
        projected_input_image: None,
        projected_input_pdf: None,
        recorded_prompt: None,
        run_state: Default::default(),
        turn_timing: Default::default(),
        agent_seq: 0,
        tool_item_seq: 0,
        repeat: Default::default(),
        streamed_calls: Default::default(),
        early_tools: Vec::new(),
        steer_signal: None,
        runtime: parts.runtime,
        trust,
        plugin_tools,
        plugin_tool_filter: Default::default(),
        mcp: None,
        hook_context_stash: Vec::new(),
        session_start_hook_ran: false,
        session_start_source: parts.session_start_source,
        bot: parts.bot,
        global: parts.global,
    };
    session.state.agents.register_root(session.agent.id.clone());
    session.state.agents.update_agent_context(
        &session.agent.id,
        "main".to_string(),
        root_conversation_path,
    );
    session.refresh_project_sandbox();
    session
}

impl Session {
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        meta: SessionMeta,
        pending: PendingApprovals,
        pending_questions: PendingQuestions,
        store: Arc<Mutex<Store>>,
        sessions_dir: &Path,
        data_dir: PathBuf,
        task_notify: async_channel::Sender<crate::task::TaskChange>,
        plugins: &crate::plugins::SharedPluginRegistry,
        runtime: Option<Arc<crate::config::RuntimeConfig>>,
    ) -> Result<Self, CoreError> {
        let rollout = Rollout::create(sessions_dir, &meta)?;
        let bot = meta
            .bot_id
            .as_deref()
            .and_then(|id| store.lock().expect("store lock").get_bot(id));
        let frozen = SessionFrozen::capture(
            &meta.id,
            &meta.cwd,
            &data_dir,
            plugins,
            runtime.as_deref(),
            bot.as_ref(),
            meta.global,
        );
        Ok(assemble_session(
            SessionParts {
                id: meta.id,
                cwd: meta.cwd,
                history: Vec::new(),
                user_ixs: Vec::new(),
                tracker: ChangeTracker::default(),
                rollout,
                rollout_failure: None,
                pending,
                pending_questions,
                store,
                data_dir,
                task_notify,
                runtime,
                session_start_source: "startup",
                bot,
                global: meta.global,
            },
            frozen,
        ))
    }

    /// Rebuild from rollout. Diff baselines are restored from the
    /// file_originals table into ChangeTracker: after resume, changes are
    /// still computed as "session's first snapshot → current", so revert
    /// works across restarts.
    #[allow(clippy::too_many_arguments)]
    pub fn load(
        id: &str,
        sessions_dir: &Path,
        pending: PendingApprovals,
        pending_questions: PendingQuestions,
        store: Arc<Mutex<Store>>,
        data_dir: PathBuf,
        task_notify: async_channel::Sender<crate::task::TaskChange>,
        plugins: &crate::plugins::SharedPluginRegistry,
        runtime: Option<Arc<crate::config::RuntimeConfig>>,
    ) -> Result<(Self, Vec<RolloutRecord>), CoreError> {
        let mut records = Rollout::load(&sessions_dir.join(format!("{id}.jsonl")))?;
        let Some(RolloutRecord::Meta { cwd, .. }) = records.first() else {
            return Err(CoreError::RolloutNoMeta { id: id.to_string() });
        };
        let cwd = cwd.clone();
        let rollout_path = sessions_dir.join(format!("{id}.jsonl"));
        let mut rollout = Rollout::open_append(sessions_dir, id)?;
        let recoveries = crate::rollout::tool_execution_recoveries(&records).map_err(|detail| {
            CoreError::RolloutRead {
                path: rollout_path.display().to_string(),
                detail,
            }
        })?;
        if !recoveries.is_empty() {
            for record in &recoveries {
                rollout
                    .append(record)
                    .map_err(|error| error.into_core(&rollout_path))?;
            }
            rollout
                .sync()
                .map_err(|error| error.into_core(&rollout_path))?;
            records.extend(recoveries);
        }
        // Freeze the same inputs as create; the canonical PromptSnapshot below
        // replaces the last-sent values (older sessions fall back to these).
        let session_meta = store.lock().expect("store lock").get_session(id);
        let bot = session_meta
            .as_ref()
            .and_then(|meta| meta.bot_id.as_deref())
            .and_then(|bot_id| store.lock().expect("store lock").get_bot(bot_id));
        let global = session_meta.as_ref().is_some_and(|meta| meta.global);
        let effective_cwd = session_meta
            .as_ref()
            .map(|meta| meta.cwd.clone())
            .unwrap_or(cwd);
        let frozen = SessionFrozen::capture(
            id,
            &effective_cwd,
            &data_dir,
            plugins,
            runtime.as_deref(),
            bot.as_ref(),
            global,
        );
        let selection = session_meta.as_ref().and_then(|meta| {
            Some(ModelSelection {
                provider_id: meta.provider_id.clone()?,
                model_id: meta.model_id.clone()?,
                reasoning_level: meta.reasoning_level.clone(),
            })
        });
        let default_level = session_meta
            .as_ref()
            .and_then(|meta| meta.reasoning_level.clone());
        let resolved = resolve_runtime_model(
            runtime.as_deref(),
            selection.as_ref(),
            default_level.as_deref(),
        )
        .ok();
        let media = resolved
            .as_ref()
            .map(|model| tool::MediaCapabilities::new(model.input_image, model.input_pdf));
        let media_dir = pig_utils::paths::media_dir(sessions_dir, id);
        let bot_persona = bot.as_ref().map(crate::bots::bot_persona_section);
        let (history, user_ixs) = crate::rollout::rebuild_history_with_media_root(
            &records,
            // Placeholder system prompt: the first run_turn overwrites it
            // wholly with the frozen snapshot
            // Placeholder (overwritten by the first run_turn): the env-key
            // leg only, the per-model cap lands with the turn's own prompt
            prompt::system_prompt_with_bot(
                &effective_cwd,
                true,
                None,
                &frozen.today,
                &frozen.agents_prompt,
                &frozen.memory_prompt,
                &frozen.skills_prompt,
                bot_persona.as_deref(),
                global,
            ),
            media,
            &media_dir,
            resolved.as_ref().map(|model| model.context_window),
        );
        let originals = store
            .lock()
            .expect("store lock")
            .file_originals(id)
            .into_iter()
            .map(|(path, content)| (PathBuf::from(path), content))
            .collect();
        let mut tracker = ChangeTracker::default();
        tracker.restore(originals);
        // After resume, take over the same JSONL and keep appending; failure
        // must be loud (setting None would silently drop all subsequent
        // records). The handle was opened above before tool recovery.
        // Crash recovery: a hard kill runs no wrap-up, so the tail turn's
        // usage never reached TurnStats (the live interrupt path writes it at
        // cancel time, so this only fires for uncounted tails). Per-request
        // StepUsage records are durable — sum the tail turn's steps and append
        // a synthesized TurnStats as the "counted" marker (reopening sees it
        // and skips; replay restores the footer/session totals through the
        // normal path). Timing fields are unknowable after a crash (zeros) and
        // the in-flight request's usage is lost either way.
        let mut recovery_append_error: Option<CoreError> = None;
        if let Some((input, cache_read, output, turn_id)) = uncounted_tail_usage(&records) {
            // The rollout file's mtime approximates when the interrupted turn ended.
            let ended_at_ms = std::fs::metadata(sessions_dir.join(format!("{id}.jsonl")))
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or_else(crate::model_io::now_ms);
            let recovered = RolloutRecord::TurnStats {
                input,
                cache_read,
                output,
                duration_ms: 0,
                ended_at_ms,
                api_ms: 0,
                ttft_ms: 0,
                ttft_steps: 0,
                api_steps: 0,
                tool_ms: 0,
                user_wait_ms: 0,
                tool_exec_ms: 0,
                tool_wall_ms: 0,
                compact_ms: 0,
                hook_ms: 0,
                review_ms: 0,
                overhead_ms: 0,
                has_usage: true,
                finish_reason: None,
                turn_id,
            };
            if let Err(error) = rollout.append(&recovered) {
                tracing::warn!(
                    error = %error,
                    "failed to append the crash-recovery TurnStats marker; usage will be re-recovered on the next open"
                );
                recovery_append_error = Some(error.into_core(rollout.path()));
            }
            records.push(recovered);
        }
        // A recalled turn's PromptSnapshot still sits in the file but its User
        // record was removed by the Recall tombstone: restore the latest
        // snapshot whose turn survived so reminder state stays consistent.
        let prompt_snapshot = surviving_prompt_snapshot(&records);
        let delivery_receipts = crate::rollout::notification_delivery_receipts(&records);
        let receipt_list = delivery_receipts
            .iter()
            .map(|(id, channel)| (id.clone(), *channel))
            .collect::<Vec<_>>();
        let tasks = {
            let mut store = store.lock().expect("store lock");
            store.reconcile_with_history(id, &receipt_list)?;
            store.recover_interrupted_tasks(id)?;
            store.recover_terminal_notifications(id)?;
            store.reconcile_delivered(&receipt_list)?;
            store.background_tasks(id)?
        };
        let mut session = assemble_session(
            SessionParts {
                id: id.to_string(),
                cwd: effective_cwd,
                history,
                user_ixs,
                tracker,
                rollout,
                // A failed recovery-marker append must stop later appends: the
                // file may hold a torn line, and the next open re-runs the
                // (idempotent) recovery. The error is surfaced at the first
                // durable boundary.
                rollout_failure: recovery_append_error,
                pending,
                pending_questions,
                store,
                data_dir,
                task_notify,
                runtime,
                session_start_source: "resume",
                bot,
                global,
            },
            frozen,
        );
        session.restore_delivery_receipts(delivery_receipts);
        session.restore_root_run(&records);
        session.state.restore_tasks(tasks);
        session.restore_agent_catalog();
        session.state.agents.restore_mailbox()?;
        if let Some(snapshot) = prompt_snapshot {
            let trusted = session.trust.admits(&session.agent.cwd);
            let same_trust = snapshot_trusted(&snapshot.skills_state) == Some(trusted);
            session.git_snapshot = snapshot.git_snapshot.clone();
            session.agents_prompt = snapshot.agents_prompt.clone();
            if same_trust {
                // The snapshot is bound to its trust decision: a project skill
                // listing, profile listing and system prompt created while the
                // workspace was trusted must not resurface after a revoke (or
                // vice versa). The fresh capture above is the truth then; only
                // its reminder state is still safe to carry over.
                if snapshot.skills_state == skills_state_with_trust(&session.data_dir, trusted) {
                    session.skills_prompt = snapshot.skills_prompt.clone();
                }
                session.profiles_summary = snapshot.profiles_summary.clone();
                session.prompt_text = Some(snapshot.text.clone());
                session.prompt_memory_enabled = Some(snapshot.memory_enabled);
                session.prompt_skills_state = Some(snapshot.skills_state.clone());
                session.recorded_prompt = Some(snapshot.clone());
            } else {
                tracing::debug!(
                    "the prompt snapshot was created under a different workspace trust decision; \
                     rebuilding the prompt from the current project surfaces"
                );
            }
            if snapshot.memory_enabled == session.memory_enabled {
                session.memory_prompt = snapshot.memory_prompt.clone();
                session.memory_reminded = snapshot.reminders.memory.clone();
            } else {
                session.memory_reminded = String::new();
            }
            session.date_frozen = snapshot.date_frozen.clone();
            session.date_reminded = snapshot.reminders.date.clone();
            session.agents_reminded = snapshot.reminders.agents.clone();
            // The concrete access boundary is re-injected on the first turn
            // after every cold open; older messages remain byte-stable.
            session.permissions_reminded = None;
        }
        session.projected_input_image = media.map(|caps| caps.input_image);
        session.projected_input_pdf = media.map(|caps| caps.input_pdf);
        Ok((session, records))
    }

    /// Rebind a session to a workspace directory (`Some`) or detach it back to
    /// a global non-workspace session (`None`), refreshing trust, project memory,
    /// AGENTS.md, skills, hooks, and permission rules while preserving the Bot's
    /// persona and personal long-term memory.
    pub(crate) fn rebind_workspace(
        &mut self,
        new_cwd: Option<PathBuf>,
        runtime: Option<Arc<crate::config::RuntimeConfig>>,
    ) {
        let global = new_cwd.is_none();
        let effective_cwd = new_cwd.unwrap_or_default();
        self.agent.cwd = effective_cwd.clone();
        self.global = global;
        self.runtime = runtime;
        let frozen = SessionFrozen::capture(
            &self.agent.id,
            &effective_cwd,
            &self.data_dir,
            &self.plugin_tools.registry(),
            self.runtime.as_deref(),
            self.bot.as_ref(),
            global,
        );
        self.trust = frozen.trust;
        self.skills_prompt = frozen.skills_prompt;
        self.agents_prompt = frozen.agents_prompt.clone();
        self.memory_enabled = frozen.memory_enabled;
        self.memory_prompt = frozen.memory_prompt;
        self.profiles_summary = frozen.profiles_summary;
        self.git_snapshot = if global {
            None
        } else {
            prompt::git_snapshot(&effective_cwd)
        };
        let app_config = self.runtime.as_ref().map(|r| r.config());
        self.hooks = crate::hooks::HookSet::load_with_plugins(
            &effective_cwd,
            app_config,
            &self.plugin_tools.registry(),
            &self.trust,
        );
        *self
            .permission_rules
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            load_permissions(&effective_cwd, &self.data_dir, &self.trust);
        self.state.extra_rw_roots = frozen.memory_rw_roots;
        self.mcp = None;
        self.prompt_text = None;
        self.prompt_memory_enabled = None;
        self.prompt_skills_state = None;
        self.recorded_prompt = None;
        self.agents_reminded = frozen.agents_prompt;
        self.memory_reminded.clear();
        self.permissions_reminded = None;
        self.refresh_project_sandbox();
    }
}

/// Latest PromptSnapshot whose turn was not recalled. The snapshot record
/// follows its User record, so a trailing Recall removes that User from the
/// rebuilt context while its snapshot would otherwise keep the reminder state
/// of a turn that no longer exists.
pub(super) fn surviving_prompt_snapshot(
    records: &[RolloutRecord],
) -> Option<crate::rollout::PromptSnapshot> {
    let mut user_stack: Vec<usize> = Vec::new();
    let mut recalled: HashSet<usize> = HashSet::new();
    let mut candidates: Vec<(usize, crate::rollout::PromptSnapshot)> = Vec::new();
    for (ix, record) in records.iter().enumerate() {
        match record {
            RolloutRecord::User { .. } => user_stack.push(ix),
            RolloutRecord::Recall => {
                if let Some(recalled_user) = user_stack.pop() {
                    recalled.insert(recalled_user);
                }
            }
            RolloutRecord::PromptSnapshot { snapshot } => {
                if let Some(&user) = user_stack.last() {
                    candidates.push((user, snapshot.clone()));
                }
            }
            _ => {}
        }
    }
    candidates
        .into_iter()
        .rev()
        .find(|(user, _)| !recalled.contains(user))
        .map(|(_, snapshot)| snapshot)
}

/// Sum the tail turn's per-request usage when it was never counted: records
/// after the last User record hold StepUsage but no TurnStats (a turn that
/// ends normally — or is interrupted, which writes TurnStats at cancel time —
/// always closes with TurnStats). Returns (input, cache_read, output,
/// turn_id); None when the tail turn was counted or consumed nothing.
pub(super) fn uncounted_tail_usage(
    records: &[RolloutRecord],
) -> Option<(u64, u64, u64, Option<String>)> {
    let (mut input, mut cache_read, mut output, mut counted) = (0u64, 0u64, 0u64, false);
    let mut turn_id = None;
    for record in records.iter().rev() {
        if matches!(record, RolloutRecord::User { .. }) {
            break;
        }
        match record {
            RolloutRecord::StepUsage {
                input: i,
                cache_read: c,
                output: o,
                turn_id: step_turn,
                ..
            } => {
                input += i;
                cache_read += c;
                output += o;
                if turn_id.is_none() {
                    turn_id = step_turn.clone();
                }
            }
            RolloutRecord::TurnStats { .. } => counted = true,
            _ => {}
        }
    }
    (!counted && input + cache_read + output > 0).then_some((input, cache_read, output, turn_id))
}

/// Load permission rules for a workspace under one trust decision: the user
/// level (`{data_dir}/permissions.toml`) unioned with the project level
/// (`.pigcode/permissions.toml`). A trusted workspace also honors project
/// `allow` rules; an untrusted one loads only its denials (no approval
/// bypass). Missing files are empty; parse failures are non-fatal with a
/// warn-level log — a broken level is skipped while the other stays in effect.
pub(super) fn load_permissions(
    cwd: &std::path::Path,
    data_dir: &std::path::Path,
    trust: &crate::config::WorkspaceTrust,
) -> crate::permissions::PermissionRules {
    let (rules, errors) =
        crate::permissions::PermissionRules::load_layered_with_trust(cwd, data_dir, trust);
    for error in errors {
        tracing::warn!("permission rules level skipped ({error:?})");
    }
    if rules.skipped > 0 {
        tracing::warn!(
            "skipped {} rules with syntax errors (permissions.toml)",
            rules.skipped
        );
    }
    rules
}
