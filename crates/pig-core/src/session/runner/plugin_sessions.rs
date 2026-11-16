//! Session actions a plugin requests through the narrow host services:
//! subscribe/create/send/interrupt/wait, a controlled one-shot model call, and
//! delegated subagents. The runner is the only owner of sessions, so every
//! action arrives as one Op and answers with one `PluginSessionResult`.
//!
//! Isolation rules enforced here:
//! - a project package may only touch sessions inside its trusted workspace;
//! - a plugin never chooses the permissions of a message it sends: the
//!   session's own current permissions are used;
//! - model calls are a single non-streaming request with a purpose, a prompt
//!   cap and an output-token cap, recorded in the ordinary API usage log;
//! - waits are bounded (the gateway's service timeout is the outer limit).

use super::entry::{TurnFuture, resolve_entry};
use super::*;

use futures_util::stream::FuturesUnordered;
use pig_protocol::{
    CoreError, Event, MessageDelivery, PluginAgentConversation, PluginAgentOutcome,
    PluginAgentProfileInfo, PluginAgentStarted, PluginModelResponse, PluginModelUsage,
    PluginPackageInfo, PluginSessionAction, PluginSource, TaskStatus,
};
use std::path::Path;
use std::time::{Duration, Instant};

const MESSAGE_MAX_BYTES: usize = 64 * 1024;
const MODEL_PROMPT_MAX_BYTES: usize = 64 * 1024;
const MODEL_SYSTEM_MAX_BYTES: usize = 16 * 1024;
const MODEL_DEFAULT_OUTPUT_TOKENS: u64 = 2048;
const MODEL_MAX_OUTPUT_TOKENS: u64 = 4096;
const MODEL_TIMEOUT: Duration = Duration::from_secs(120);
const WAIT_MAX_MS: u64 = 30_000;

/// Purposes a plugin may declare for a model call. The value is the capability
/// target (`model.request`), so a package grants only the purposes it needs.
pub const PLUGIN_MODEL_PURPOSES: &[&str] = &[
    "review",
    "commit_message",
    "classify",
    "memory",
    "summary",
    "custom",
];

pub(super) struct PluginTurnWaiter {
    pub(super) session_id: String,
    pub(super) request_id: String,
    pub(super) package: String,
    pub(super) digest: String,
    pub(super) deadline: Instant,
}

impl Manager {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn plugin_session_action(
        &mut self,
        event_tx: async_channel::Sender<Event>,
        request_id: String,
        package: String,
        digest: String,
        action: PluginSessionAction,
        turns: &mut FuturesUnordered<TurnFuture>,
    ) {
        let result = match action {
            PluginSessionAction::Create {
                cwd,
                provider_id,
                model_id,
                reasoning_level,
            } => {
                let cwd = normalize_workspace_path(&cwd);
                let scope = self.plugin_scope(&package, &digest);
                let result = scope.and_then(|package| {
                    ensure_workspace_in_package(&package, &cwd)?;
                    Ok(())
                });
                Some(match result {
                    Ok(()) => {
                        let before: std::collections::BTreeSet<String> =
                            self.sessions.keys().cloned().collect();
                        self.new_session(
                            event_tx.clone(),
                            cwd,
                            provider_id,
                            model_id,
                            reasoning_level,
                            None,
                            Some(false),
                        );
                        let created = self
                            .sessions
                            .keys()
                            .find(|id| !before.contains(*id))
                            .cloned();
                        match created {
                            Some(session_id) => Ok(serde_json::json!({ "sessionId": session_id })),
                            None => Err(internal("the session could not be created")),
                        }
                    }
                    Err(error) => Err(error),
                })
            }
            PluginSessionAction::Send {
                session_id,
                content,
                delivery,
            } => self.plugin_send(
                &package,
                &digest,
                session_id,
                content,
                delivery,
                event_tx.clone(),
                turns,
            ),
            PluginSessionAction::Interrupt { session_id } => {
                let prepared = self
                    .sessions
                    .get(&session_id)
                    .map(|entry| entry.cwd.clone())
                    .ok_or(CoreError::SessionNotFound)
                    .and_then(|cwd| {
                        ensure_cwd_in_package(&self.plugin_scope(&package, &digest)?, &cwd)
                    });
                Some(match prepared {
                    Ok(()) => {
                        self.interrupt(session_id);
                        Ok(serde_json::Value::Null)
                    }
                    Err(error) => Err(error),
                })
            }
            PluginSessionAction::Wait {
                session_id,
                timeout_ms,
            } => self.plugin_wait(
                &package,
                &digest,
                request_id.clone(),
                session_id,
                timeout_ms,
            ),
            PluginSessionAction::Model {
                session_id,
                purpose,
                system,
                prompt,
                max_output_tokens,
            } => {
                match self.plugin_model_request(
                    &event_tx,
                    &request_id,
                    &package,
                    &digest,
                    session_id,
                    purpose,
                    system,
                    prompt,
                    max_output_tokens,
                ) {
                    Ok(()) => None,
                    Err(error) => Some(Err(error)),
                }
            }
            PluginSessionAction::AgentList { session_id } => {
                Some(self.plugin_agent_list(&package, &digest, session_id))
            }
            PluginSessionAction::AgentStart {
                session_id,
                profile,
                prompt,
                description,
                background,
            } => {
                if background {
                    Some(self.plugin_agent_start(
                        &event_tx,
                        &package,
                        &digest,
                        session_id,
                        profile,
                        prompt,
                        description,
                    ))
                } else {
                    Some(Err(CoreError::PluginInvalidInput {
                        detail: "agent.start only supports background delegation; use agent.wait"
                            .into(),
                    }))
                }
            }
            PluginSessionAction::AgentSend {
                session_id,
                agent_id,
                message,
            } => Some(self.plugin_agent_send(&package, &digest, session_id, agent_id, message)),
            PluginSessionAction::AgentStop {
                session_id,
                agent_id,
            } => Some(self.plugin_agent_stop(&package, &digest, session_id, agent_id)),
            PluginSessionAction::AgentWait {
                session_id,
                agent_id,
                timeout_ms,
            } => {
                match self.plugin_agent_wait(
                    &event_tx,
                    &request_id,
                    &package,
                    &digest,
                    session_id,
                    agent_id,
                    timeout_ms,
                ) {
                    Ok(()) => None,
                    Err(error) => Some(Err(error)),
                }
            }
            PluginSessionAction::AgentRead {
                session_id,
                agent_id,
            } => {
                match self.plugin_agent_read(
                    &event_tx,
                    &request_id,
                    &package,
                    &digest,
                    session_id,
                    agent_id,
                ) {
                    Ok(()) => None,
                    Err(error) => Some(Err(error)),
                }
            }
        };
        if let Some(result) = result {
            emit_plugin_result(&event_tx, &request_id, &package, &digest, result);
        }
    }

    fn plugin_send(
        &mut self,
        package: &str,
        digest: &str,
        session_id: String,
        content: String,
        delivery: MessageDelivery,
        event_tx: async_channel::Sender<Event>,
        turns: &mut FuturesUnordered<TurnFuture>,
    ) -> Option<Result<serde_json::Value, CoreError>> {
        let prepared = self
            .sessions
            .get(&session_id)
            .map(|entry| (entry.cwd.clone(), entry.last_permissions))
            .ok_or(CoreError::SessionNotFound)
            .and_then(|(cwd, permissions)| {
                ensure_cwd_in_package(&self.plugin_scope(package, digest)?, &cwd)?;
                Ok(permissions)
            });
        let permissions = match prepared {
            Ok(permissions) => permissions,
            Err(error) => return Some(Err(error)),
        };
        if content.trim().is_empty() || content.len() > MESSAGE_MAX_BYTES {
            return Some(Err(invalid_input(
                "a plugin message must be non-empty and within 64 KiB",
            )));
        }
        self.send_message(
            event_tx,
            session_id,
            content,
            Vec::new(),
            Vec::new(),
            permissions,
            delivery,
            turns,
        );
        Some(Ok(serde_json::json!({ "accepted": true })))
    }

    fn plugin_wait(
        &mut self,
        package: &str,
        digest: &str,
        request_id: String,
        session_id: String,
        timeout_ms: u64,
    ) -> Option<Result<serde_json::Value, CoreError>> {
        let entry = match self.sessions.get(&session_id) {
            Some(entry) => entry,
            None => return Some(Err(CoreError::SessionNotFound)),
        };
        let scope = match self.plugin_scope(package, digest) {
            Ok(scope) => scope,
            Err(error) => return Some(Err(error)),
        };
        if let Err(error) = ensure_cwd_in_package(&scope, &entry.cwd) {
            return Some(Err(error));
        }
        if entry.cancel.is_none() {
            return Some(Ok(serde_json::json!({ "status": "idle" })));
        }
        let timeout = Duration::from_millis(timeout_ms.clamp(1, WAIT_MAX_MS));
        self.plugin_waiters.push(PluginTurnWaiter {
            session_id,
            request_id,
            package: package.to_owned(),
            digest: digest.to_owned(),
            deadline: Instant::now() + timeout,
        });
        None
    }

    /// The package's registry entry, with the trust/incompatibility checks a
    /// session action needs.
    pub(super) fn plugin_scope(
        &self,
        package: &str,
        digest: &str,
    ) -> Result<PluginPackageInfo, CoreError> {
        let registry = self.plugins.lock().expect("plugin registry lock");
        let package = registry.package(package, digest)?.clone();
        if !package.enabled || package.incompatible.is_some() {
            return Err(CoreError::PluginPermission {
                detail: "the plugin is not active".into(),
            });
        }
        if let PluginSource::Project { workspace } = &package.source {
            let trust = crate::config::WorkspaceTrust::for_config(self.config.as_ref(), workspace);
            if !trust.admits(workspace) {
                return Err(CoreError::PluginPermission {
                    detail: "the workspace containing this plugin is not trusted".into(),
                });
            }
        }
        Ok(package)
    }

    fn plugin_model_request(
        &self,
        event_tx: &async_channel::Sender<Event>,
        request_id: &str,
        package: &str,
        digest: &str,
        session_id: Option<String>,
        purpose: String,
        system: Option<String>,
        prompt: String,
        max_output_tokens: Option<u64>,
    ) -> Result<(), CoreError> {
        if !PLUGIN_MODEL_PURPOSES.contains(&purpose.as_str()) {
            return Err(invalid_input(format!(
                "unknown model purpose `{purpose}`; use one of {}",
                PLUGIN_MODEL_PURPOSES.join(", ")
            )));
        }
        if prompt.trim().is_empty() || prompt.len() > MODEL_PROMPT_MAX_BYTES {
            return Err(invalid_input(
                "the model prompt must be non-empty and within 64 KiB",
            ));
        }
        if system
            .as_deref()
            .is_some_and(|system| system.len() > MODEL_SYSTEM_MAX_BYTES)
        {
            return Err(invalid_input("the system prompt is over 16 KiB"));
        }
        let scope = self.plugin_scope(package, digest)?;
        let resolved = match session_id.as_deref() {
            Some(session_id) => {
                let entry = self
                    .sessions
                    .get(session_id)
                    .ok_or(CoreError::SessionNotFound)?;
                ensure_cwd_in_package(&scope, &entry.cwd)?;
                resolve_entry(entry)?
            }
            None => {
                let cwd = match &scope.source {
                    PluginSource::Project { workspace } => workspace,
                    _ => &self.default_cwd,
                };
                let runtime = super::runtime::layered_runtime(
                    self.config.as_ref(),
                    &self.config_path,
                    cwd,
                    self.credentials.clone(),
                    &self.plugins,
                );
                let runtime = runtime.ok_or(CoreError::NoModelConfigured)?;
                runtime
                    .resolve_model(None, None)
                    .map_err(|error| CoreError::ProviderConfig {
                        detail: error.to_string(),
                    })?
            }
        };
        let mut config = resolved;
        config.max_output_tokens = config.max_output_tokens.min(
            max_output_tokens
                .unwrap_or(MODEL_DEFAULT_OUTPUT_TOKENS)
                .clamp(1, MODEL_MAX_OUTPUT_TOKENS),
        );
        // One-shots do not participate in the session's prompt-cache routing.
        config.prompt_cache_key = None;
        let event_tx = event_tx.clone();
        let request_id = request_id.to_owned();
        let package = package.to_owned();
        let digest = digest.to_owned();
        let store = self.store.clone();
        let usage_session = session_id.unwrap_or_else(|| "plugin".into());
        let source = format!("plugin:{purpose}");
        tokio::spawn(async move {
            let started_at_ms = crate::model_io::now_ms();
            let started = Instant::now();
            let cancel = tokio_util::sync::CancellationToken::new();
            let outcome = tokio::time::timeout(MODEL_TIMEOUT, async {
                match system {
                    Some(system) => {
                        pig_provider::complete_messages(
                            &config,
                            &[
                                pig_provider::ChatMsg::system(system),
                                pig_provider::ChatMsg::user(prompt),
                            ],
                            &[],
                            &cancel,
                        )
                        .await
                    }
                    None => pig_provider::complete_text(&config, prompt, None, &cancel).await,
                }
            })
            .await;
            let duration_ms = started.elapsed().as_millis() as u64;
            let mut record = crate::api_usage::ApiUsageRecord::new(
                &usage_session,
                &source,
                &config,
                started_at_ms,
                duration_ms,
            );
            let result = match outcome {
                Ok(Ok(call)) => {
                    record.finish = "stop".into();
                    record.set_usage(call.usage);
                    record.response = call.response;
                    let usage = call.usage.map(|usage| PluginModelUsage {
                        input_tokens: usage.input,
                        output_tokens: usage.output,
                    });
                    Ok(serde_json::to_value(PluginModelResponse {
                        text: call.text,
                        usage,
                    })
                    .unwrap_or_default())
                }
                Ok(Err(call)) => {
                    record.finish = "error".into();
                    record.error = Some(pig_provider::core_error_en(&call.error));
                    record.set_usage(call.usage);
                    record.response = call.response;
                    Err(call.error)
                }
                Err(_) => {
                    record.finish = "timeout".into();
                    record.error = Some("The plugin model request timed out".into());
                    Err(CoreError::Network {
                        detail: "the plugin model request timed out".into(),
                    })
                }
            };
            store.lock().expect("store lock").record_api_usage(&record);
            emit_plugin_result(&event_tx, &request_id, &package, &digest, result);
        });
        Ok(())
    }

    fn plugin_agent_list(
        &self,
        package: &str,
        digest: &str,
        session_id: Option<String>,
    ) -> Result<serde_json::Value, CoreError> {
        let scope = self.plugin_scope(package, digest)?;
        let (cwd, trust) = match session_id.as_deref() {
            Some(session_id) => {
                let entry = self
                    .sessions
                    .get(session_id)
                    .ok_or(CoreError::SessionNotFound)?;
                ensure_cwd_in_package(&scope, &entry.cwd)?;
                let trust = entry
                    .session
                    .as_ref()
                    .and_then(|session| session.runtime_config())
                    .or(entry.runtime.as_deref())
                    .map(|runtime| runtime.trust().clone())
                    .unwrap_or_else(|| crate::config::WorkspaceTrust::untrusted(&entry.cwd));
                (entry.cwd.clone(), trust)
            }
            None => match &scope.source {
                PluginSource::Project { workspace } => (
                    workspace.clone(),
                    crate::config::WorkspaceTrust::for_config(self.config.as_ref(), workspace),
                ),
                _ => (
                    self.default_cwd.clone(),
                    crate::config::WorkspaceTrust::for_config(
                        self.config.as_ref(),
                        &self.default_cwd,
                    ),
                ),
            },
        };
        let profiles =
            crate::agent::load_profiles_with_plugins(&cwd, &self.data_dir, &trust, &self.plugins);
        let profiles: Vec<PluginAgentProfileInfo> = profiles
            .into_iter()
            .map(|profile| PluginAgentProfileInfo {
                name: profile.name,
                description: profile.description,
                tools: profile.tools,
                model: profile.model,
                source: profile.source.as_str().to_owned(),
            })
            .collect();
        Ok(serde_json::json!({ "profiles": profiles }))
    }

    #[allow(clippy::too_many_arguments)]
    fn plugin_agent_start(
        &mut self,
        event_tx: &async_channel::Sender<Event>,
        package: &str,
        digest: &str,
        session_id: String,
        profile: String,
        prompt: String,
        description: String,
    ) -> Result<serde_json::Value, CoreError> {
        let scope = self.plugin_scope(package, digest)?;
        let (cwd, model, idle) = {
            let entry = self
                .sessions
                .get(&session_id)
                .ok_or(CoreError::SessionNotFound)?;
            (
                entry.cwd.clone(),
                resolve_entry(entry)?,
                entry.session.is_some(),
            )
        };
        ensure_cwd_in_package(&scope, &cwd)?;
        if !idle {
            return Err(CoreError::PluginPermission {
                detail: "the session is busy; wait for its turn to end before delegating".into(),
            });
        }
        let entry = self.sessions.get_mut(&session_id).expect("session entry");
        let Some(session) = entry.session.as_mut() else {
            return Err(CoreError::PluginPermission {
                detail: "the session is busy; wait for its turn to end before delegating".into(),
            });
        };
        let started = session
            .start_delegated_agent(&profile, &prompt, &description, &model, event_tx)
            .map_err(|detail| CoreError::PluginPermission { detail })?;
        serde_json::to_value(PluginAgentStarted {
            agent_id: started.agent_id,
            task_id: started.task_id,
            profile: started.profile,
            model: started.model,
            background: started.background,
            status: "running".into(),
        })
        .map_err(|error| internal(error.to_string()))
    }

    fn plugin_agent_send(
        &self,
        package: &str,
        digest: &str,
        session_id: String,
        agent_id: String,
        message: String,
    ) -> Result<serde_json::Value, CoreError> {
        let scope = self.plugin_scope(package, digest)?;
        let entry = self
            .sessions
            .get(&session_id)
            .ok_or(CoreError::SessionNotFound)?;
        ensure_cwd_in_package(&scope, &entry.cwd)?;
        if message.trim().is_empty() || message.len() > MESSAGE_MAX_BYTES {
            return Err(invalid_input(
                "an agent steering message must be non-empty and within 64 KiB",
            ));
        }
        let mut steers = entry.state.agent_steers.lock().expect("agent steer lock");
        match steers.get_mut(&agent_id) {
            Some(queue) => {
                queue.push_back(message);
                Ok(serde_json::Value::Null)
            }
            None => Err(CoreError::PluginPermission {
                detail: format!("no running subagent `{agent_id}` in this session"),
            }),
        }
    }

    fn plugin_agent_stop(
        &self,
        package: &str,
        digest: &str,
        session_id: String,
        agent_id: String,
    ) -> Result<serde_json::Value, CoreError> {
        let scope = self.plugin_scope(package, digest)?;
        let entry = self
            .sessions
            .get(&session_id)
            .ok_or(CoreError::SessionNotFound)?;
        ensure_cwd_in_package(&scope, &entry.cwd)?;
        let task_id = entry.state.running_agent_task(&agent_id).ok_or_else(|| {
            CoreError::PluginPermission {
                detail: format!("no running subagent `{agent_id}` in this session"),
            }
        })?;
        crate::task::stop_task(
            &entry.state.tasks,
            &task_id,
            &entry.state.task_notify,
            &session_id,
        )
        .map_err(|detail| CoreError::PluginPermission { detail })?;
        Ok(serde_json::Value::Null)
    }

    #[allow(clippy::too_many_arguments)]
    fn plugin_agent_wait(
        &self,
        event_tx: &async_channel::Sender<Event>,
        request_id: &str,
        package: &str,
        digest: &str,
        session_id: String,
        agent_id: String,
        timeout_ms: u64,
    ) -> Result<(), CoreError> {
        let scope = self.plugin_scope(package, digest)?;
        let entry = self
            .sessions
            .get(&session_id)
            .ok_or(CoreError::SessionNotFound)?;
        ensure_cwd_in_package(&scope, &entry.cwd)?;
        if entry
            .state
            .tasks
            .lock()
            .expect("task registry lock")
            .iter()
            .all(|task| task.agent_id.as_deref() != Some(agent_id.as_str()))
        {
            return Err(CoreError::PluginPermission {
                detail: format!("no subagent `{agent_id}` in this session"),
            });
        }
        let tasks = entry.state.tasks.clone();
        let event_tx = event_tx.clone();
        let request_id = request_id.to_owned();
        let package = package.to_owned();
        let digest = digest.to_owned();
        let timeout = Duration::from_millis(timeout_ms.clamp(1, WAIT_MAX_MS));
        tokio::spawn(async move {
            let deadline = Instant::now() + timeout;
            loop {
                let snapshot = tasks
                    .lock()
                    .expect("task registry lock")
                    .iter()
                    .find(|task| task.agent_id.as_deref() == Some(agent_id.as_str()))
                    .map(|task| (task.status, task.result_path.clone(), task.output.clone()));
                let Some((status, result_path, output)) = snapshot else {
                    emit_plugin_result(
                        &event_tx,
                        &request_id,
                        &package,
                        &digest,
                        Err(CoreError::PluginPermission {
                            detail: format!("subagent `{agent_id}` ended before it was observed"),
                        }),
                    );
                    return;
                };
                if matches!(status, TaskStatus::Running | TaskStatus::Queued) {
                    if Instant::now() >= deadline {
                        emit_plugin_result(
                            &event_tx,
                            &request_id,
                            &package,
                            &digest,
                            Ok(serde_json::to_value(PluginAgentOutcome {
                                agent_id: agent_id.clone(),
                                status: "timeout".into(),
                                summary: String::new(),
                                result_path: None,
                            })
                            .unwrap_or_default()),
                        );
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
                let status_name = match status {
                    TaskStatus::Completed => "completed",
                    TaskStatus::Killed => "stopped",
                    TaskStatus::Failed => "failed",
                    _ => "finished",
                };
                let summary = result_path
                    .as_ref()
                    .and_then(|path| std::fs::read_to_string(path).ok())
                    .map(|text| session_preview(&text))
                    .filter(|text| !text.is_empty())
                    .unwrap_or_else(|| session_preview(&output));
                emit_plugin_result(
                    &event_tx,
                    &request_id,
                    &package,
                    &digest,
                    Ok(serde_json::to_value(PluginAgentOutcome {
                        agent_id: agent_id.clone(),
                        status: status_name.into(),
                        summary,
                        result_path: result_path.map(|path| path.display().to_string()),
                    })
                    .unwrap_or_default()),
                );
                return;
            }
        });
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn plugin_agent_read(
        &self,
        event_tx: &async_channel::Sender<Event>,
        request_id: &str,
        package: &str,
        digest: &str,
        session_id: String,
        agent_id: String,
    ) -> Result<(), CoreError> {
        let scope = self.plugin_scope(package, digest)?;
        let entry = self
            .sessions
            .get(&session_id)
            .ok_or(CoreError::SessionNotFound)?;
        ensure_cwd_in_package(&scope, &entry.cwd)?;
        let running = entry
            .state
            .tasks
            .lock()
            .expect("task registry lock")
            .iter()
            .any(|task| {
                task.agent_id.as_deref() == Some(agent_id.as_str())
                    && matches!(task.status, TaskStatus::Running | TaskStatus::Queued)
            });
        let sessions_dir = self.sessions_dir.clone();
        let event_tx = event_tx.clone();
        let request_id = request_id.to_owned();
        let package = package.to_owned();
        let digest = digest.to_owned();
        let read_session = session_id.clone();
        let read_agent = agent_id.clone();
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                crate::agent::read_agent_by_id(&sessions_dir, &read_session, &read_agent)
            })
            .await;
            let result = match result {
                Ok(Ok((meta, messages))) => {
                    let (title, _subtitle, items) = crate::agent::display_items(&meta, &messages);
                    serde_json::to_value(PluginAgentConversation {
                        agent_id,
                        title,
                        running,
                        items,
                    })
                    .map_err(|error| internal(error.to_string()))
                }
                Ok(Err(detail)) => Err(CoreError::SubagentRead { detail }),
                Err(error) => Err(internal(error.to_string())),
            };
            emit_plugin_result(&event_tx, &request_id, &package, &digest, result);
        });
        Ok(())
    }

    /// Called from `turn_finished`: settle the session's waiting plugins.
    pub(super) fn settle_plugin_waiters(
        &mut self,
        event_tx: &async_channel::Sender<Event>,
        session_id: &str,
        outcome: &TurnOutcome,
    ) {
        let status = match outcome.status {
            crate::session::AgentRunStatus::Completed => "completed",
            crate::session::AgentRunStatus::Cancelled => "aborted",
            crate::session::AgentRunStatus::Failed => "failed",
            crate::session::AgentRunStatus::Queued
            | crate::session::AgentRunStatus::Running
            | crate::session::AgentRunStatus::Lost => {
                unreachable!("settled agent runs have terminal outcomes")
            }
        };
        let waiters: Vec<PluginTurnWaiter> = {
            let (ready, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.plugin_waiters)
                .into_iter()
                .partition(|waiter| waiter.session_id == session_id);
            self.plugin_waiters = waiting;
            ready
        };
        for waiter in waiters {
            emit_plugin_result(
                event_tx,
                &waiter.request_id,
                &waiter.package,
                &waiter.digest,
                Ok(serde_json::json!({ "status": status })),
            );
        }
    }

    /// Called from the loop's tick: a wait whose deadline passed answers
    /// `timeout` instead of leaking until the gateway gives up.
    pub(super) fn expire_plugin_waiters(&mut self, event_tx: &async_channel::Sender<Event>) {
        let now = Instant::now();
        let (expired, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.plugin_waiters)
            .into_iter()
            .partition(|waiter| waiter.deadline <= now);
        self.plugin_waiters = waiting;
        for waiter in expired {
            emit_plugin_result(
                event_tx,
                &waiter.request_id,
                &waiter.package,
                &waiter.digest,
                Ok(serde_json::json!({ "status": "timeout" })),
            );
        }
    }
}

fn emit_plugin_result(
    event_tx: &async_channel::Sender<Event>,
    request_id: &str,
    package: &str,
    digest: &str,
    result: Result<serde_json::Value, CoreError>,
) {
    emit(
        event_tx,
        Event::PluginSessionResult {
            request_id: request_id.to_owned(),
            package: package.to_owned(),
            digest: digest.to_owned(),
            result,
        },
    );
}

fn session_preview(text: &str) -> String {
    const MAX: usize = 2048;
    if text.chars().count() <= MAX {
        return text.to_owned();
    }
    let mut preview: String = text.chars().take(MAX).collect();
    preview.push('…');
    preview
}

fn ensure_workspace_in_package(package: &PluginPackageInfo, cwd: &Path) -> Result<(), CoreError> {
    let PluginSource::Project { workspace } = &package.source else {
        return Ok(());
    };
    if !crate::plugins::canonical_workspace_contains(workspace, cwd) {
        return Err(CoreError::PluginPermission {
            detail: "a project plugin may only touch sessions inside its workspace".into(),
        });
    }
    Ok(())
}

pub(super) fn ensure_cwd_in_package(
    package: &PluginPackageInfo,
    cwd: &Path,
) -> Result<(), CoreError> {
    ensure_workspace_in_package(package, cwd)
}

fn internal(detail: impl Into<String>) -> CoreError {
    CoreError::Internal {
        detail: detail.into(),
    }
}

fn invalid_input(detail: impl Into<String>) -> CoreError {
    CoreError::PluginPermission {
        detail: detail.into(),
    }
}
