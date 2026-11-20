use super::*;

fn runtime_task_catalog(state: &crate::task::SessionToolState) -> Option<Vec<AgentCatalogEntry>> {
    let snapshot = state.agents.runtime_snapshot()?;
    let agents: std::collections::HashMap<String, crate::agent::runtime::RuntimeAgent> = snapshot
        .agents
        .into_iter()
        .map(|agent| (agent.agent_id.clone(), agent))
        .collect();
    let tasks = state
        .tasks
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut entries = Vec::new();
    for run in snapshot.runs {
        if run.agent_id == state.session_id {
            continue;
        }
        let Some(agent) = agents.get(&run.agent_id) else {
            continue;
        };
        let task = tasks.iter().find(|task| {
            task.run
                .as_ref()
                .is_some_and(|meta| meta.run_id == run.run_id)
        });
        let status = match run.status {
            crate::agent::runtime::RuntimeRunStatus::Queued => pig_protocol::AgentRunStatus::Queued,
            crate::agent::runtime::RuntimeRunStatus::Running
            | crate::agent::runtime::RuntimeRunStatus::Stopping => {
                pig_protocol::AgentRunStatus::Running
            }
            crate::agent::runtime::RuntimeRunStatus::Completed => {
                pig_protocol::AgentRunStatus::Completed
            }
            crate::agent::runtime::RuntimeRunStatus::Cancelled => {
                pig_protocol::AgentRunStatus::Cancelled
            }
            crate::agent::runtime::RuntimeRunStatus::Failed => pig_protocol::AgentRunStatus::Failed,
            crate::agent::runtime::RuntimeRunStatus::Lost => pig_protocol::AgentRunStatus::Lost,
        };
        entries.push(AgentCatalogEntry {
            agent_id: run.agent_id.clone(),
            run_id: Some(run.run_id.clone()),
            turn_kind: Some(pig_protocol::TurnKind::Regular),
            parent_id: agent.parent_agent_id.clone(),
            depth: agent.depth,
            profile: agent.profile.clone(),
            model: task.and_then(|task| task.model.clone()).unwrap_or_default(),
            status: Some(status),
            turns: run.turns,
            usage: run.usage,
            record_path: agent.conversation_path.clone(),
            result_path: task.and_then(|task| task.result_path.clone()),
        });
    }
    entries.sort_by_key(|entry| (entry.agent_id.clone(), entry.run_id.clone()));
    Some(entries)
}

pub(crate) fn task_catalog(state: &crate::task::SessionToolState) -> Vec<AgentCatalogEntry> {
    if let Some(entries) = runtime_task_catalog(state) {
        return entries;
    }
    let nodes: std::collections::HashMap<String, crate::agent::AgentNode> = state
        .agents
        .snapshot()
        .into_iter()
        .map(|node| (node.agent_id.clone(), node))
        .collect();
    let tasks = state
        .tasks
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    tasks
        .iter()
        .filter_map(|task| {
            let run = task.run.as_ref()?;
            let node = nodes.get(&run.agent_id)?;
            let run_state = node.run(&run.run_id)?;
            Some(AgentCatalogEntry {
                agent_id: run.agent_id.clone(),
                run_id: Some(run.run_id.clone()),
                turn_kind: Some(pig_protocol::TurnKind::Regular),
                parent_id: run_state.parent_agent_id.clone(),
                depth: run_state.depth,
                profile: run.profile.clone(),
                model: task.model.clone().unwrap_or_default(),
                status: Some(run_state.status),
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
            status: root_run.and_then(|run| {
                run.run_id
                    .as_deref()
                    .and_then(|run_id| self.state.agents.run_status(&self.agent.id, run_id))
            }),
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
        let Some(snapshot) = self.state.agents.runtime_snapshot() else {
            return;
        };
        let agents = snapshot
            .agents
            .into_iter()
            .map(|agent| (agent.agent_id.clone(), agent))
            .collect::<std::collections::HashMap<_, _>>();
        let existing = self
            .state
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .filter_map(|task| task.run.as_ref().map(|run| run.run_id.clone()))
            .collect::<std::collections::HashSet<_>>();
        let mut recovered = snapshot
            .runs
            .into_iter()
            .filter(|run| run.agent_id != self.agent.id && !existing.contains(&run.run_id))
            .filter_map(|run| {
                let agent = agents.get(&run.agent_id)?.clone();
                Some((agent, run))
            })
            .collect::<Vec<_>>();
        recovered.sort_by(|(left_agent, left_run), (right_agent, right_run)| {
            (
                left_agent.depth,
                &left_agent.agent_id,
                left_run.started_at_ms,
                &left_run.run_id,
            )
                .cmp(&(
                    right_agent.depth,
                    &right_agent.agent_id,
                    right_run.started_at_ms,
                    &right_run.run_id,
                ))
        });
        for (agent, run) in recovered {
            let Some(path) = agent.conversation_path.clone() else {
                tracing::warn!(agent_id = %agent.agent_id, run_id = %run.run_id, "runtime agent has no conversation artifact");
                continue;
            };
            let task_id = format!(
                "b{}",
                self.state
                    .task_seq
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    + 1
            );
            let result_path = path.with_file_name(format!("{}.result.md", agent.agent_id));
            let status = match run.status {
                crate::agent::runtime::RuntimeRunStatus::Completed => {
                    pig_protocol::TaskStatus::Completed
                }
                crate::agent::runtime::RuntimeRunStatus::Cancelled => {
                    pig_protocol::TaskStatus::Killed
                }
                crate::agent::runtime::RuntimeRunStatus::Failed => pig_protocol::TaskStatus::Failed,
                crate::agent::runtime::RuntimeRunStatus::Queued
                | crate::agent::runtime::RuntimeRunStatus::Running
                | crate::agent::runtime::RuntimeRunStatus::Stopping
                | crate::agent::runtime::RuntimeRunStatus::Lost => pig_protocol::TaskStatus::Lost,
            };
            let mut task = crate::task::TaskEntry::recovered_agent(
                task_id.clone(),
                format!("Subagent {}", agent.profile),
                crate::task::AgentRunMetadata {
                    agent_id: agent.agent_id.clone(),
                    run_id: run.run_id.clone(),
                    parent_agent_id: agent.parent_agent_id.clone(),
                    delegation_depth: agent.depth,
                    profile: agent.profile.clone(),
                    usage: run.usage,
                },
                path,
                result_path.is_file().then_some(result_path),
                String::new(),
                agent.created_at_ms / 1_000,
            );
            task.status = status;
            task.turns = Some(run.turns.min(u32::MAX as usize) as u32);
            if status != pig_protocol::TaskStatus::Lost {
                task.reason = run.stop_reason.clone();
                task.ended_at = run.ended_at_ms.map(|value| value / 1_000);
            }
            self.state
                .tasks
                .lock()
                .expect("task registry lock")
                .push(task);
            self.state.notify_task(&task_id);
            if status == pig_protocol::TaskStatus::Lost {
                self.state.notify_terminal(&task_id);
            }
        }
    }
}
