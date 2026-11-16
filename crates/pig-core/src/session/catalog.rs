use super::*;

fn run_status(status: pig_protocol::TaskStatus) -> AgentRunStatus {
    match status {
        pig_protocol::TaskStatus::Queued => AgentRunStatus::Queued,
        pig_protocol::TaskStatus::Running => AgentRunStatus::Running,
        pig_protocol::TaskStatus::Completed => AgentRunStatus::Completed,
        pig_protocol::TaskStatus::Killed => AgentRunStatus::Cancelled,
        pig_protocol::TaskStatus::Failed | pig_protocol::TaskStatus::TimedOut => {
            AgentRunStatus::Failed
        }
        pig_protocol::TaskStatus::Lost => AgentRunStatus::Lost,
    }
}

pub(crate) fn task_catalog(state: &crate::task::SessionToolState) -> Vec<AgentCatalogEntry> {
    let tasks = state
        .tasks
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    tasks
        .iter()
        .filter_map(|task| {
            let run = task.run.as_ref()?;
            Some(AgentCatalogEntry {
                agent_id: run.agent_id.clone(),
                run_id: Some(run.run_id.clone()),
                turn_kind: Some(pig_protocol::TurnKind::Regular),
                parent_id: run.parent_agent_id.clone(),
                depth: run.delegation_depth,
                profile: run.profile.clone(),
                model: task.model.clone().unwrap_or_default(),
                status: Some(run_status(task.status)),
                turns: task.turns.unwrap_or(0) as usize,
                usage: run.usage,
                record_path: task.record_path.clone(),
                result_path: task.result_path.clone(),
            })
        })
        .collect()
}

impl Session {
    pub fn agent_catalog(&self) -> Vec<AgentCatalogEntry> {
        let root_run = self.agent.run.as_ref();
        let root_model = self.agent.config.as_ref().map_or_else(
            || {
                self.model_override
                    .as_ref()
                    .map(|model| model.model_id.clone())
                    .or_else(|| {
                        self.runtime
                            .as_deref()
                            .map(|runtime| runtime.config().default_model.clone())
                    })
                    .unwrap_or_default()
            },
            |model| model.model.clone(),
        );
        let mut catalog = vec![AgentCatalogEntry {
            agent_id: self.agent.id.clone(),
            run_id: root_run.and_then(|run| run.run_id.clone()),
            turn_kind: root_run.map(|run| run.turn_kind),
            parent_id: self
                .subagent
                .as_ref()
                .map(|subagent| subagent.parent_agent_id.clone()),
            depth: self.agent.delegation_depth,
            profile: if self.is_subagent() {
                self.subagent
                    .as_ref()
                    .map(|subagent| subagent.profile.name.clone())
                    .unwrap_or_default()
            } else {
                "main".to_string()
            },
            model: root_model,
            status: root_run.map(|run| run.status),
            turns: root_run.map_or(0, |run| run.turns),
            usage: root_run.map_or((0, 0, 0), |run| run.usage),
            record_path: self.rollout.as_ref().map_or_else(
                || {
                    self.subagent
                        .as_ref()
                        .map(|subagent| subagent.jsonl.clone())
                },
                |rollout| Some(rollout.path().to_path_buf()),
            ),
            result_path: root_run.and_then(|run| {
                run.artifact
                    .as_ref()
                    .and_then(|artifact| artifact.path.clone())
            }),
        }];
        catalog.extend(task_catalog(&self.state));
        catalog
    }
}

impl Session {
    pub(crate) fn restore_agent_catalog(&mut self) {
        let sessions_dir = self.data_dir.join("sessions");
        let agents_dir = crate::agent::agents_dir(&sessions_dir, &self.agent.id);
        let Ok(entries) = std::fs::read_dir(agents_dir) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
                continue;
            }
            if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
                continue;
            }
            let Some(agent_id) = path
                .file_stem()
                .and_then(|value| value.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            if crate::agent::validate_agent_id(&agent_id).is_err() {
                continue;
            }
            let (meta, _) = match crate::agent::read_agent(&path) {
                Ok(context) => context,
                Err(error) => {
                    tracing::warn!(%agent_id, "cannot recover child catalog entry: {error}");
                    continue;
                }
            };
            if meta.parent_session_id != self.agent.id {
                continue;
            }
            let mut tasks = self
                .state
                .tasks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if tasks
                .iter()
                .any(|task| task.agent_id.as_deref() == Some(agent_id.as_str()))
            {
                continue;
            }
            let task_id = format!(
                "b{}",
                self.state
                    .task_seq
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    + 1
            );
            let result_path = path.with_file_name(format!("{agent_id}.result.md"));
            tasks.push(crate::task::TaskEntry::recovered_agent(
                task_id.clone(),
                format!("Subagent {}: {}", meta.profile, meta.description),
                crate::task::AgentRunMetadata {
                    agent_id: agent_id.clone(),
                    run_id: meta.run_id.clone(),
                    parent_agent_id: Some(meta.parent_agent_id.clone()),
                    delegation_depth: meta.delegation_depth,
                    profile: meta.profile,
                    usage: (0, 0, 0),
                },
                path,
                result_path.is_file().then_some(result_path),
                meta.model,
                meta.created_at,
            ));
            drop(tasks);
            self.state.notify_terminal(&task_id);
        }
    }
}
