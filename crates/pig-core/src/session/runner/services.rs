use std::collections::HashMap;
use std::path::PathBuf;

use pig_protocol::{AppConfig, CoreError, Event};
use tokio_util::sync::CancellationToken;

use crate::config;
use crate::session::{
    CompactOutcome, CompactTrigger, ModelSelection, PermissionSettings, resolve_approval,
};
use crate::tool;

use super::entry::{TurnFuture, resolve_entry};
use super::runtime::{install_sandbox_settings, layered_runtime};
use super::trust::persist_workspace_trust;
use super::{Manager, emit};

impl Manager {
    pub(super) fn git_info(&mut self, event_tx: async_channel::Sender<Event>, cwd: PathBuf) {
        let tx = event_tx.clone();
        tokio::spawn(async move {
            let cwd2 = cwd.clone();
            let result = tokio::task::spawn_blocking(move || crate::git::git_info(&cwd2)).await;
            if let Ok((current_branch, branches)) = result {
                let _ = tx
                    .send(Event::GitInfo {
                        cwd,
                        current_branch,
                        branches,
                    })
                    .await;
            }
        });
    }
    pub(super) fn checkout_branch(
        &mut self,
        event_tx: async_channel::Sender<Event>,
        cwd: PathBuf,
        branch: String,
    ) {
        let tx = event_tx.clone();
        tokio::spawn(async move {
            let cwd2 = cwd.clone();
            let branch2 = branch.clone();
            let result =
                tokio::task::spawn_blocking(move || crate::git::checkout(&cwd2, &branch2)).await;
            match result {
                Ok(Ok(())) => {
                    let _ = tx.send(Event::BranchChanged { cwd, branch }).await;
                }
                Ok(Err(error)) => {
                    let _ = tx
                        .send(Event::Error {
                            session_id: None,
                            error,
                            fatal: false,
                        })
                        .await;
                }
                Err(e) => {
                    let _ = tx
                        .send(Event::Error {
                            session_id: None,
                            error: CoreError::GitTask {
                                detail: e.to_string(),
                            },
                            fatal: false,
                        })
                        .await;
                }
            }
        });
    }
    pub(super) fn git_status(&mut self, event_tx: async_channel::Sender<Event>, cwd: PathBuf) {
        let tx = event_tx.clone();
        tokio::spawn(async move {
            let cwd2 = cwd.clone();
            let result = tokio::task::spawn_blocking(move || crate::git::git_status(&cwd2)).await;
            if let Ok(result) = result {
                let (is_git, unstaged, staged) = match result {
                    Some((unstaged, staged)) => (true, unstaged, staged),
                    None => (false, vec![], vec![]),
                };
                let _ = tx
                    .send(Event::GitStatus {
                        cwd,
                        is_git,
                        unstaged,
                        staged,
                    })
                    .await;
            }
        });
    }
    pub(super) fn git_diff(
        &mut self,
        event_tx: async_channel::Sender<Event>,
        cwd: PathBuf,
        path: String,
        staged: bool,
    ) {
        let tx = event_tx.clone();
        tokio::spawn(async move {
            let cwd2 = cwd.clone();
            let path2 = path.clone();
            let result =
                tokio::task::spawn_blocking(move || crate::git::git_diff(&cwd2, &path2, staged))
                    .await;
            if let Ok((diff, note)) = result {
                let _ = tx
                    .send(Event::GitDiff {
                        cwd,
                        path,
                        staged,
                        diff,
                        note,
                    })
                    .await;
            }
        });
    }
    pub(super) fn agent_output(
        &mut self,
        event_tx: async_channel::Sender<Event>,
        session_id: String,
        agent_id: String,
    ) {
        let Some(entry) = self.sessions.get(&session_id) else {
            return;
        };
        let partial = entry
            .state
            .agent_partials
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&agent_id)
            .cloned();
        emit(
            &event_tx,
            Event::AgentOutput {
                session_id,
                agent_id,
                run_id: partial.as_ref().map(|partial| partial.run_id.clone()),
                text: partial.map_or(String::new(), |partial| partial.text),
            },
        );
    }

    pub(super) fn list_agents(
        &mut self,
        event_tx: async_channel::Sender<Event>,
        session_id: String,
    ) {
        let sessions_dir = self.sessions_dir.clone();
        let Some(entry) = self.sessions.get(&session_id) else {
            emit(
                &event_tx,
                Event::Error {
                    session_id: Some(session_id),
                    error: CoreError::Internal {
                        detail: "Cannot list agents for a session that is not open".to_string(),
                    },
                    fatal: false,
                },
            );
            return;
        };
        let agents = if let Some(session) = entry.session.as_ref() {
            session.agent_catalog()
        } else {
            let mut agents = Vec::new();
            if let Some(run) = entry.active_run.clone() {
                agents.push(pig_protocol::AgentCatalogEntry {
                    agent_id: session_id.clone(),
                    run_id: Some(run.run_id),
                    turn_kind: Some(run.turn_kind),
                    parent_id: None,
                    depth: 0,
                    profile: "main".to_string(),
                    model: super::entry::model_label(entry).1,
                    status: Some(pig_protocol::AgentRunStatus::Running),
                    turns: 0,
                    usage: (0, 0, 0),
                    record_path: Some(sessions_dir.join(format!("{session_id}.jsonl"))),
                    result_path: None,
                });
            }
            agents.extend(crate::session::catalog::task_catalog(&entry.state));
            agents
        };
        emit(&event_tx, Event::AgentCatalogChanged { session_id, agents });
    }

    pub(super) fn load_subagent(
        &mut self,
        event_tx: async_channel::Sender<Event>,
        session_id: String,
        agent_id: String,
    ) {
        let sessions = &self.sessions;
        let sessions_dir = self.sessions_dir.clone();
        // No Session instance needed: read the subagent context JSONL directly
        // (read-only display in the right-side tab). `read_agent_by_id` is the
        // single containment gate: generated-id validation, a real session
        // component, canonical containment in this session's agent directory,
        // a non-symlink context file and meta-id equality. A UI-provided id
        // therefore cannot name a path outside its owning directory.
        let tx = event_tx.clone();
        // An active driver holds a steer-registry entry for its whole run
        // (foreground and background alike); a task still occupying or awaiting
        // a driver slot has not finished either. A session absent from memory is
        // not active.
        let running = sessions.get(&session_id).is_some_and(|entry| {
            let steering = {
                let steers = entry.state.agent_steers.lock().expect("agent steers lock");
                steers.contains_key(&agent_id)
            };
            steering
                || entry
                    .state
                    .tasks
                    .lock()
                    .expect("task registry lock")
                    .iter()
                    .any(|t| {
                        t.agent_id.as_deref() == Some(agent_id.as_str())
                            && matches!(
                                t.status,
                                pig_protocol::TaskStatus::Queued
                                    | pig_protocol::TaskStatus::Running
                            )
                    })
        });
        let run_id = sessions.get(&session_id).and_then(|entry| {
            entry
                .state
                .tasks
                .lock()
                .expect("task registry lock")
                .iter()
                .filter(|task| task.agent_id.as_deref() == Some(agent_id.as_str()))
                .max_by_key(|task| task.started_at)
                .and_then(|task| task.run.as_ref().map(|run| run.run_id.clone()))
        });
        let sessions_dir = sessions_dir.clone();
        let read_session = session_id.clone();
        let read_agent = agent_id.clone();
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                crate::agent::read_agent_by_id_stored(&sessions_dir, &read_session, &read_agent)
            })
            .await;
            match result {
                Ok(Ok((meta, stored))) => {
                    let (title, subtitle, items) =
                        crate::agent::display_stored_items(&meta, &stored);
                    let _ = tx
                        .send(Event::ConversationSnapshot {
                            session_id,
                            agent_id: Some(agent_id),
                            title: Some(title),
                            subtitle: Some(subtitle),
                            items,
                            running,
                            run_id,
                        })
                        .await;
                }
                Ok(Err(error)) => {
                    let _ = tx
                        .send(Event::Error {
                            session_id: Some(session_id),
                            error: CoreError::SubagentRead { detail: error },
                            fatal: false,
                        })
                        .await;
                }
                Err(e) => {
                    let _ = tx
                        .send(Event::Error {
                            session_id: Some(session_id),
                            error: CoreError::SubagentReadTask {
                                detail: e.to_string(),
                            },
                            fatal: false,
                        })
                        .await;
                }
            }
        });
    }
    pub(super) fn interrupt(&mut self, session_id: String) {
        let sessions = &mut self.sessions;
        // A user stop: the wrap-up pauses a non-empty queue
        // instead of draining it (cancel_promote stays false)
        if let Some(entry) = sessions.get_mut(&session_id)
            && let Some(cancel) = &entry.cancel
        {
            entry.cancel_promote = false;
            cancel.cancel();
        }
    }
    pub(super) fn retry_now(&mut self, session_id: String) {
        let sessions = &self.sessions;
        if let Some(entry) = sessions.get(&session_id)
            && let Some(control) = &entry.retry
        {
            control.retry_now();
        }
    }
    pub(super) fn approval_reply(
        &mut self,
        request_id: String,
        decision: pig_protocol::ApprovalDecision,
        feedback: Option<String>,
    ) {
        let pending = self.pending.clone();
        // Exactly one waiter (this request id) is resolved; there
        // is no key-based fan-out.
        resolve_approval(&pending, &request_id, decision, feedback);
    }
    pub(super) fn question_reply(&mut self, request_id: String, answers: Option<Vec<Vec<String>>>) {
        let pending_questions = self.pending_questions.clone();
        if let Some(reply) = pending_questions
            .lock()
            .expect("pending questions lock")
            .remove(&request_id)
        {
            let _ = reply.send(answers);
        }
    }
    pub(super) fn set_permissions(&mut self, session_id: String, permissions: PermissionSettings) {
        let sessions = &mut self.sessions;
        let store = self.store.clone();
        if let Some(entry) = sessions.get_mut(&session_id) {
            // Manual permission change: notification-only turns follow it
            entry.last_permissions = permissions;
            if let Some(session) = entry.session.as_mut() {
                session.set_permissions(permissions);
            }
            store
                .lock()
                .expect("store lock")
                .update_session(&session_id, |m| {
                    m.permissions = permissions;
                });
        }
    }
    pub(super) fn set_plan_mode(
        &mut self,
        event_tx: async_channel::Sender<Event>,
        session_id: String,
        enabled: bool,
    ) {
        let sessions = &mut self.sessions;
        let store = self.store.clone();
        // The plan toggle is orthogonal to the mode tier. The UI
        // is the initiator, but when a turn/compaction owns the
        // Session the switch cannot take effect immediately: it
        // is persisted now, announced as deferred, and applied
        // at the next turn start instead of being dropped.
        let Some(entry) = sessions.get_mut(&session_id) else {
            emit(
                &event_tx,
                Event::Error {
                    session_id: Some(session_id),
                    error: CoreError::SessionNotFound,
                    fatal: false,
                },
            );
            return;
        };
        store
            .lock()
            .expect("store lock")
            .update_session(&session_id, |m| {
                m.plan_enabled = enabled;
            });
        if let Some(session) = entry.session.as_mut() {
            session.set_plan_mode(enabled);
            entry.pending_plan = None;
        } else {
            entry.pending_plan = Some(enabled);
            emit(
                &event_tx,
                Event::PlanModeChanged {
                    session_id: session_id.clone(),
                    enabled,
                    deferred: true,
                },
            );
        }
    }
    pub(super) fn revert_file(
        &mut self,
        event_tx: async_channel::Sender<Event>,
        session_id: String,
        path: String,
    ) {
        let sessions = &mut self.sessions;
        match sessions.get_mut(&session_id) {
            Some(entry) if entry.session.is_some() => {
                entry
                    .session
                    .as_mut()
                    .expect("session")
                    .revert_file(&path, &event_tx);
            }
            Some(_) => emit(
                &event_tx,
                Event::Error {
                    session_id: Some(session_id),
                    error: CoreError::RevertTurnInFlight,
                    fatal: false,
                },
            ),
            None => emit(
                &event_tx,
                Event::Error {
                    session_id: Some(session_id),
                    error: CoreError::SessionNotFound,
                    fatal: false,
                },
            ),
        }
    }
    pub(super) fn recall_last_message(
        &mut self,
        event_tx: async_channel::Sender<Event>,
        session_id: String,
    ) {
        let sessions = &mut self.sessions;
        match sessions.get_mut(&session_id) {
            // session == None means a turn (or the compact
            // pseudo-turn) owns it: recall is idle-only
            Some(entry) if entry.session.is_some() => {
                let session = entry.session.as_mut().expect("session");
                if let Err(error) = session.recall_last_message(&event_tx) {
                    emit(
                        &event_tx,
                        Event::Error {
                            session_id: Some(session_id.clone()),
                            error,
                            fatal: false,
                        },
                    );
                }
            }
            Some(_) => emit(
                &event_tx,
                Event::Error {
                    session_id: Some(session_id.clone()),
                    error: CoreError::RecallTurnInFlight,
                    fatal: false,
                },
            ),
            None => emit(
                &event_tx,
                Event::Error {
                    session_id: Some(session_id.clone()),
                    error: CoreError::SessionNotFound,
                    fatal: false,
                },
            ),
        }
        self.refresh_notification_queue(&event_tx, &session_id);
    }
    pub(super) fn search_files(
        &mut self,
        event_tx: async_channel::Sender<Event>,
        session_id: String,
        query: String,
        cwd: Option<String>,
    ) {
        let sessions = &self.sessions;
        let default_cwd = self.default_cwd.clone();
        let cwd = cwd
            .map(std::path::PathBuf::from)
            .or_else(|| {
                sessions
                    .get(&session_id)
                    .and_then(|e| e.session.as_ref().map(|s| s.agent.cwd.clone()))
            })
            .unwrap_or_else(|| default_cwd.clone());
        let tx = event_tx.clone();
        let query_for_search = query.clone();
        tokio::spawn(async move {
            let results = tokio::task::spawn_blocking(move || {
                tool::search_files(&cwd, &query_for_search, 20)
            })
            .await
            .unwrap_or_default();
            let _ = tx
                .send(Event::FileSearchResults {
                    session_id,
                    query,
                    results,
                })
                .await;
        });
    }
    pub(super) fn model_lookup(&mut self, event_tx: async_channel::Sender<Event>, id: String) {
        let data_dir = self.data_dir.clone();
        // Answer directly on cache hit; refetch only on a miss (new model ID) with a stale cache,
        // to avoid hammering models.dev while the user tries IDs in the dialog
        const REFETCH_AFTER_SECS: u64 = 10 * 60;
        let tx = event_tx.clone();
        let cache_path = data_dir.join("models-dev-cache.json");
        tokio::spawn(async move {
            let lookup_id = id.clone();
            let loaded = tokio::task::spawn_blocking({
                let cache_path = cache_path.clone();
                move || crate::models_registry::load_cache(&cache_path)
            })
            .await
            .ok()
            .flatten();
            let (fetched_at, index) = match loaded {
                Some((_, index)) if index.contains_key(&lookup_id) => {
                    // Hit: no network needed
                    let _ = tx
                        .send(Event::ModelInfo {
                            id,
                            info: index.get(&lookup_id).cloned(),
                        })
                        .await;
                    return;
                }
                Some(pair) => pair,
                None => (0, HashMap::new()),
            };
            let fresh = fetched_at > 0
                && crate::models_registry::unix_now() < fetched_at + REFETCH_AFTER_SECS;
            let index = if fresh {
                index
            } else {
                match crate::models_registry::fetch_index().await {
                    Ok(fetched) => {
                        let for_save = fetched.clone();
                        let path2 = cache_path.clone();
                        let _ = tokio::task::spawn_blocking(move || {
                            crate::models_registry::save_cache(
                                &path2,
                                crate::models_registry::unix_now(),
                                &for_save,
                            );
                        })
                        .await;
                        fetched
                    }
                    Err(_) => index,
                }
            };
            let _ = tx
                .send(Event::ModelInfo {
                    id,
                    info: index.get(&lookup_id).cloned(),
                })
                .await;
        });
    }
    pub(super) fn compact(
        &mut self,
        event_tx: async_channel::Sender<Event>,
        session_id: String,
        instruction: Option<String>,
        turns: &mut futures_util::stream::FuturesUnordered<TurnFuture>,
    ) {
        let sessions = &mut self.sessions;
        match sessions.get_mut(&session_id) {
            Some(entry) if entry.session.is_some() => {
                // A manual compaction cannot summarize without
                // a model. Resolve before taking the session so a
                // missing/unusable model is a typed nonfatal
                // rejection with the history untouched, never a
                // silent truncation.
                let resolved = match resolve_entry(entry) {
                    Ok(resolved) => resolved,
                    Err(error) => {
                        let _ = event_tx.send_blocking(Event::Error {
                            session_id: Some(session_id),
                            error,
                            fatal: false,
                        });
                        return;
                    }
                };
                let mut session = entry.session.take().expect("session present");
                let tx = event_tx.clone();
                let sid = session_id.clone();
                entry.cancel = Some(CancellationToken::new());
                entry.steerable = false;
                session.begin_root_run(pig_protocol::TurnKind::Compact);
                entry.active_run = session.run_envelope();
                let cancel = entry.cancel.clone().expect("cancel");
                turns.push(Box::pin(async move {
                    // A manual /compact can arrive after a
                    // model switch without an intervening turn;
                    // align the history with the current media
                    // capabilities before the summary request so
                    // an unsupported document cannot silently
                    // force the truncation fallback.
                    let media =
                        tool::MediaCapabilities::new(resolved.input_image, resolved.input_pdf);
                    if (session.projected_input_image != Some(media.input_image)
                        || session.projected_input_pdf != Some(media.input_pdf))
                        && let Err(error) = session.reproject_media(media, resolved.context_window)
                    {
                        let _ = tx
                            .send(Event::Error {
                                session_id: Some(sid.clone()),
                                error,
                                fatal: true,
                            })
                            .await;
                        let outcome = session.root_result(crate::session::AgentRunStatus::Failed);
                        return (sid, session, outcome);
                    }
                    let outcome = session
                        .run_compact(
                            Some(&resolved),
                            CompactTrigger::Manual,
                            instruction.as_deref(),
                            false,
                            None,
                            &tx,
                            &cancel,
                        )
                        .await;
                    // Map the compact outcome onto the queue
                    // policy: a cancelled compaction is an
                    // abort (never a completed turn that could
                    // drain the queue); a skipped or failed
                    // compaction is a normal boundary (the
                    // failure is already surfaced as a
                    // nonfatal error and history is unchanged).
                    let turn_outcome = match outcome {
                        CompactOutcome::Cancelled => {
                            session.root_result(crate::session::AgentRunStatus::Cancelled)
                        }
                        CompactOutcome::Completed
                        | CompactOutcome::Skipped
                        | CompactOutcome::Failed => {
                            session.root_result(crate::session::AgentRunStatus::Completed)
                        }
                    };
                    (sid, session, turn_outcome)
                }));
            }
            _ => emit(
                &event_tx,
                Event::Error {
                    session_id: Some(session_id),
                    error: CoreError::CompactUnavailable,
                    fatal: false,
                },
            ),
        }
    }
    pub(super) fn set_model(
        &mut self,
        session_id: String,
        provider_id: String,
        model_id: String,
        reasoning_level: Option<String>,
    ) {
        let sessions = &mut self.sessions;
        let store = self.store.clone();
        if let Some(entry) = sessions.get_mut(&session_id) {
            entry.model_override = Some(ModelSelection {
                provider_id: provider_id.clone(),
                model_id: model_id.clone(),
                reasoning_level: reasoning_level.clone(),
            });
            entry.reasoning_level = reasoning_level.clone();
            // Write-through to the sessions table: both reopen and new-session inheritance read from here
            store
                .lock()
                .expect("store lock")
                .update_session(&session_id, |m| {
                    m.provider_id = Some(provider_id.clone());
                    m.model_id = Some(model_id.clone());
                    m.reasoning_level = reasoning_level.clone();
                });
            // The new model may share the same media capabilities but have a
            // different context window: reconcile window-derived PDF reserves
            // now so an idle session's cached history is priced for the model
            // the next request will use. Measured page-tree costs stay.
            if let Ok(resolved) = resolve_entry(entry)
                && let Some(session) = entry.session.as_mut()
            {
                session.reprice_document_estimates(resolved.context_window);
            }
        }
    }
    pub(super) fn set_reasoning(&mut self, session_id: String, reasoning_level: Option<String>) {
        let sessions = &mut self.sessions;
        let store = self.store.clone();
        if let Some(entry) = sessions.get_mut(&session_id) {
            // Saved independently of the model override: applies to the configured default model when there is no override (resolve's default_level)
            entry.reasoning_level = reasoning_level.clone();
            if let Some(sel) = &mut entry.model_override {
                sel.reasoning_level = reasoning_level.clone();
            }
            store
                .lock()
                .expect("store lock")
                .update_session(&session_id, |m| {
                    m.reasoning_level = reasoning_level.clone();
                });
        }
    }
    pub(super) fn get_config(&mut self, event_tx: async_channel::Sender<Event>) {
        let config = &self.config;
        emit(
            &event_tx,
            Event::ConfigSnapshot {
                config: config.as_ref().cloned().unwrap_or_default(),
            },
        );
        self.emit_provider_credentials(event_tx);
    }

    fn emit_provider_credentials(&self, event_tx: async_channel::Sender<Event>) {
        let providers = self
            .config
            .as_ref()
            .map(|config| config.providers.clone())
            .unwrap_or_default();
        emit(
            &event_tx,
            Event::ProviderCredentials {
                credentials: self.credentials.provider_statuses(&providers),
            },
        );
    }

    pub(super) fn save_config(
        &mut self,
        event_tx: async_channel::Sender<Event>,
        new_config: AppConfig,
    ) {
        if let Err(error) = self.save_config_result(event_tx.clone(), new_config) {
            emit(
                &event_tx,
                Event::Error {
                    session_id: None,
                    error,
                    fatal: false,
                },
            );
        }
    }

    pub(super) fn save_config_result(
        &mut self,
        event_tx: async_channel::Sender<Event>,
        new_config: AppConfig,
    ) -> Result<(), CoreError> {
        let config_path = self.config_path.clone();
        let config = &mut self.config;
        let sessions = &mut self.sessions;
        let removed: Vec<String> = config
            .as_ref()
            .map(|current| {
                current
                    .providers
                    .iter()
                    .filter(|provider| {
                        !new_config
                            .providers
                            .iter()
                            .any(|candidate| candidate.id == provider.id)
                    })
                    .map(|provider| provider.id.clone())
                    .collect()
            })
            .unwrap_or_default();
        let credentials = self.credentials.clone();
        let refreshed = new_config.providers.clone();
        let credential_result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                for provider_id in removed {
                    credentials
                        .delete(crate::credentials::CredentialId::provider(&provider_id))
                        .await
                        .map_err(|error| {
                            error.into_core(&crate::credentials::CredentialId::provider(
                                &provider_id,
                            ))
                        })?;
                }
                credentials.refresh_providers(&refreshed).await;
                Ok::<(), CoreError>(())
            })
        });
        credential_result?;
        config::save(&config_path, &new_config)?;
        *config = Some(new_config);
        if let Some(config) = config.as_ref() {
            install_sandbox_settings(config);
        }
        // Workspace runtimes resolve models from the layered
        // config; refresh the shared object so a provider/model
        // or trust edit is visible to the next turn. A turn
        // owns its Session during the edit; the wrap-up
        // installs this same entry runtime before any queued
        // turn starts. A trust change (grant or revoke) refreshes
        // every project surface through `apply_trust` and drops
        // the stale MCP status cache.
        let mut trust_banners = Vec::new();
        let plugins = self.plugins.clone();
        for (session_id, entry) in sessions.iter_mut() {
            let runtime = layered_runtime(
                config.as_ref(),
                &config_path,
                &entry.cwd,
                self.credentials.clone(),
                &plugins,
            );
            let was_trusted = entry
                .runtime
                .as_ref()
                .is_some_and(|runtime| runtime.trust().admits(&entry.cwd));
            let is_trusted = runtime
                .as_ref()
                .is_some_and(|runtime| runtime.trust().admits(&entry.cwd));
            let trust_changed = was_trusted != is_trusted;
            entry.runtime = runtime.clone();
            if trust_changed {
                entry.mcp_status = None;
            }
            if let Some(session) = entry.session.as_mut() {
                session.set_runtime(runtime);
                // A revoke re-opens the one trust entry point immediately:
                // the project surfaces still exist and await a decision.
                if trust_changed && let Some((path, count)) = session.pending_project_trust() {
                    trust_banners.push((session_id.clone(), path, count));
                }
            }
        }
        for (session_id, path, count) in trust_banners {
            emit(
                &event_tx,
                Event::ProjectHooksPending {
                    session_id,
                    path,
                    count,
                },
            );
        }
        emit(
            &event_tx,
            Event::ConfigSnapshot {
                config: config.as_ref().cloned().unwrap_or_default(),
            },
        );
        self.emit_provider_credentials(event_tx);
        Ok(())
    }

    pub(super) fn test_provider(
        &mut self,
        event_tx: async_channel::Sender<Event>,
        provider_id: String,
    ) {
        let config = &self.config;
        let found = config
            .as_ref()
            .and_then(|c| c.providers.iter().find(|p| p.id == provider_id).cloned());
        let tx = event_tx.clone();
        let credentials = self.credentials.clone();
        tokio::spawn(async move {
            let Some(provider) = found else {
                // Provider not found: structured error (a different channel from connection results)
                let _ = tx
                    .send(Event::Error {
                        session_id: None,
                        error: CoreError::ProviderNotFound,
                        fatal: false,
                    })
                    .await;
                return;
            };
            let _ = credentials
                .get(crate::credentials::CredentialId::provider(&provider.id))
                .await;
            let resolved = match config::resolve_provider_config(&provider, &credentials) {
                Ok(resolved) => resolved,
                Err(error) => {
                    let _ = tx
                        .send(Event::Error {
                            session_id: None,
                            error: CoreError::ProviderConfig {
                                detail: error.detail().to_string(),
                            },
                            fatal: false,
                        })
                        .await;
                    return;
                }
            };
            let model = provider
                .models
                .first()
                .map(|m| m.id.clone())
                .unwrap_or_else(|| "ping".to_string());
            let result = pig_provider::test_provider(
                &resolved.base_url,
                &resolved.api_key,
                resolved.api_format,
                &model,
            )
            .await;
            let _ = tx
                .send(Event::TestResult {
                    provider_id,
                    result,
                })
                .await;
        });
    }

    pub(super) fn set_provider_api_key(
        &mut self,
        event_tx: async_channel::Sender<Event>,
        provider_id: String,
        secret: pig_protocol::SecretString,
    ) {
        if !self
            .config
            .as_ref()
            .is_some_and(|config| config.providers.iter().any(|p| p.id == provider_id))
        {
            emit(
                &event_tx,
                Event::Error {
                    session_id: None,
                    error: CoreError::ProviderNotFound,
                    fatal: false,
                },
            );
            return;
        };
        let id = crate::credentials::CredentialId::provider(&provider_id);
        let credentials = self.credentials.clone();
        let providers = self
            .config
            .as_ref()
            .map(|config| config.providers.clone())
            .unwrap_or_default();
        tokio::spawn(async move {
            match credentials.set(id.clone(), secret.into_secret()).await {
                Ok(_) => emit(
                    &event_tx,
                    Event::ProviderCredentials {
                        credentials: credentials.provider_statuses(&providers),
                    },
                ),
                Err(error) => emit(
                    &event_tx,
                    Event::Error {
                        session_id: None,
                        error: error.into_core(&id),
                        fatal: false,
                    },
                ),
            }
        });
    }

    pub(super) fn clear_provider_api_key(
        &mut self,
        event_tx: async_channel::Sender<Event>,
        provider_id: String,
    ) {
        if !self
            .config
            .as_ref()
            .is_some_and(|config| config.providers.iter().any(|p| p.id == provider_id))
        {
            emit(
                &event_tx,
                Event::Error {
                    session_id: None,
                    error: CoreError::ProviderNotFound,
                    fatal: false,
                },
            );
            return;
        }
        let id = crate::credentials::CredentialId::provider(&provider_id);
        let credentials = self.credentials.clone();
        let providers = self
            .config
            .as_ref()
            .map(|config| config.providers.clone())
            .unwrap_or_default();
        tokio::spawn(async move {
            match credentials.delete(id.clone()).await {
                Ok(()) => emit(
                    &event_tx,
                    Event::ProviderCredentials {
                        credentials: credentials.provider_statuses(&providers),
                    },
                ),
                Err(error) => emit(
                    &event_tx,
                    Event::Error {
                        session_id: None,
                        error: error.into_core(&id),
                        fatal: false,
                    },
                ),
            }
        });
    }

    pub(super) fn list_mcp_servers(
        &mut self,
        event_tx: async_channel::Sender<Event>,
        session_id: String,
    ) {
        let sessions = &mut self.sessions;
        let servers = match sessions.get_mut(&session_id) {
            Some(entry) => {
                // Mid-turn (session=None) the manager is unreachable: answer from the cached list
                if let Some(statuses) = entry
                    .session
                    .as_ref()
                    .and_then(|session| session.mcp.as_ref())
                    .map(|mcp| mcp.statuses())
                {
                    entry.mcp_status = Some(statuses);
                }
                entry.mcp_status.clone()
            }
            None => None,
        };
        emit(
            &event_tx,
            Event::McpServerList {
                session_id,
                servers,
            },
        );
    }
    pub(super) fn trust_project_hooks(
        &mut self,
        event_tx: async_channel::Sender<Event>,
        session_id: String,
    ) {
        let config_path = self.config_path.clone();
        let config = &mut self.config;
        let sessions = &mut self.sessions;
        let store = self.store.clone();
        // Workspace root: live Session first, the entry's stored
        // cwd while the Session is inside a turn future
        // (session=None), then store meta.
        let cwd = sessions
            .get(&session_id)
            .map(|entry| entry.cwd.clone())
            .or_else(|| {
                store
                    .lock()
                    .expect("store lock")
                    .get_session(&session_id)
                    .map(|meta| meta.cwd)
            });
        let Some(cwd) = cwd else {
            emit(
                &event_tx,
                Event::Error {
                    session_id: Some(session_id),
                    error: CoreError::SessionNotFound,
                    fatal: false,
                },
            );
            return;
        };
        // Persist first; activate only the runtime loaded from
        // the saved file. A save or reload failure leaves this
        // session untouched (no hooks/allow/skills without a
        // matching trust + sandbox state).
        let runtime =
            match persist_workspace_trust(config, &config_path, &cwd, self.credentials.clone()) {
                Ok(runtime) => runtime,
                Err(error) => {
                    emit(
                        &event_tx,
                        Event::Error {
                            session_id: None,
                            error,
                            fatal: false,
                        },
                    );
                    return;
                }
            };
        if let Some(config) = config.as_ref() {
            // Same lifecycle as SaveConfig: the user level is the
            // process-wide sandbox source.
            install_sandbox_settings(config);
        }
        // Activate every project-file surface consistently for
        // this session through the one `apply_trust` boundary. The
        // stored runtime is the exact layered view built from the
        // saved trust file (including the trusted project
        // provider/model layer and sandbox overlay). While a turn
        // owns the Session, the decision lands at the wrap-up;
        // the next session create/resume reads the persisted list.
        if let Some(entry) = sessions.get_mut(&session_id) {
            entry.runtime = Some(runtime.clone());
            // The connected manager predates the decision: drop
            // the stale status cache and reconnect lazily on the
            // next step with project servers admitted.
            entry.mcp_status = None;
            if let Some(session) = entry.session.as_mut() {
                session.apply_trust(Some(runtime));
            }
        }
        emit(&event_tx, Event::ProjectHooksTrusted { session_id });
    }
}
