use super::*;

/// One current or recovered activation. Child entries project the existing task
/// registry; the root entry projects its own latest `AgentRunResult`.
#[derive(Clone, Debug)]
pub struct AgentCatalogEntry {
    pub agent_id: String,
    pub run_id: Option<String>,
    pub parent_id: Option<String>,
    pub depth: u32,
    pub profile: String,
    pub model: String,
    pub status: Option<AgentRunStatus>,
    pub turns: usize,
    pub usage: (u64, u64, u64),
    pub record_path: Option<PathBuf>,
    pub result_path: Option<PathBuf>,
}

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
        let tasks = self
            .state
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        catalog.extend(tasks.iter().filter_map(|task| {
            let run = task.run.as_ref()?;
            Some(AgentCatalogEntry {
                agent_id: run.agent_id.clone(),
                run_id: Some(run.run_id.clone()),
                parent_id: Some(run.parent_agent_id.clone()),
                depth: run.delegation_depth,
                profile: run.profile.clone(),
                model: task.model.clone().unwrap_or_default(),
                status: Some(run_status(task.status)),
                turns: task.turns.unwrap_or(0) as usize,
                usage: run.usage,
                record_path: task.record_path.clone(),
                result_path: task.result_path.clone(),
            })
        }));
        catalog
    }
}
