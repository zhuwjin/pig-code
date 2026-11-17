use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use pig_protocol::AgentRunStatus;

use super::{AgentMailbox, AgentSteer};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AgentLifecycle {
    Queued,
    Running,
    Idle,
    Lost,
    Closed,
}

impl AgentLifecycle {
    pub(crate) fn is_active(self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentRunState {
    pub(crate) run_id: String,
    pub(crate) status: AgentRunStatus,
    pub(crate) parent_agent_id: Option<String>,
    pub(crate) depth: u32,
}

impl AgentRunState {
    pub(crate) fn new(
        run_id: String,
        status: AgentRunStatus,
        parent_agent_id: Option<String>,
        depth: u32,
    ) -> Self {
        Self {
            run_id,
            status,
            parent_agent_id,
            depth,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentPartial {
    pub(crate) run_id: String,
    pub(crate) text: String,
}

#[derive(Clone, Debug)]
pub(crate) struct AgentNode {
    pub(crate) agent_id: String,
    pub(crate) parent_agent_id: Option<String>,
    pub(crate) depth: u32,
    pub(crate) lifecycle: AgentLifecycle,
    pub(crate) current_run: Option<AgentRunState>,
    pub(crate) run_history: Vec<AgentRunState>,
    pub(crate) mailbox: AgentMailbox,
    pub(crate) partial: Option<AgentPartial>,
    supervised: bool,
}

impl AgentNode {
    pub(crate) fn root(agent_id: String) -> Self {
        Self {
            agent_id,
            parent_agent_id: None,
            depth: 0,
            lifecycle: AgentLifecycle::Idle,
            current_run: None,
            run_history: Vec::new(),
            mailbox: AgentMailbox::default(),
            partial: None,
            supervised: false,
        }
    }

    pub(crate) fn child(
        agent_id: String,
        parent_agent_id: String,
        depth: u32,
        run_id: String,
    ) -> Self {
        Self {
            agent_id,
            parent_agent_id: Some(parent_agent_id.clone()),
            depth,
            lifecycle: AgentLifecycle::Queued,
            current_run: Some(AgentRunState::new(
                run_id,
                AgentRunStatus::Queued,
                Some(parent_agent_id),
                depth,
            )),
            run_history: Vec::new(),
            mailbox: AgentMailbox::default(),
            partial: None,
            supervised: false,
        }
    }

    pub(crate) fn restored(
        agent_id: String,
        parent_agent_id: Option<String>,
        depth: u32,
        lifecycle: AgentLifecycle,
        current_run: Option<AgentRunState>,
    ) -> Self {
        Self {
            agent_id,
            parent_agent_id,
            depth,
            lifecycle,
            current_run,
            run_history: Vec::new(),
            mailbox: AgentMailbox::default(),
            partial: None,
            supervised: false,
        }
    }

    fn remember_run(&mut self, run: AgentRunState) {
        if let Some(previous) = self
            .run_history
            .iter_mut()
            .find(|previous| previous.run_id == run.run_id)
        {
            *previous = run;
        } else {
            self.run_history.push(run);
        }
    }

    fn remember_current_run(&mut self) {
        if let Some(current) = self.current_run.take() {
            self.remember_run(current);
        }
    }

    pub(crate) fn run(&self, run_id: &str) -> Option<&AgentRunState> {
        self.current_run
            .as_ref()
            .filter(|run| run.run_id == run_id)
            .or_else(|| {
                self.run_history
                    .iter()
                    .rev()
                    .find(|run| run.run_id == run_id)
            })
    }
}

/// Exclusive ownership of an agent's durable context and its pre-run budget
/// reservation. Releasing the guard does not change the node's lifecycle.
pub(crate) struct AgentWriterGuard {
    registry: AgentRegistry,
    agent_id: String,
    released: bool,
}

impl std::fmt::Debug for AgentWriterGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentWriterGuard")
            .field("agent_id", &self.agent_id)
            .field("released", &self.released)
            .finish()
    }
}

impl AgentWriterGuard {
    pub(crate) fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        self.registry
            .writers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.agent_id);
    }
}

impl Drop for AgentWriterGuard {
    fn drop(&mut self) {
        self.release();
    }
}

#[derive(Clone)]
pub(crate) struct AgentRegistry {
    nodes: Arc<Mutex<HashMap<String, AgentNode>>>,
    writers: Arc<Mutex<HashSet<String>>>,
    executions: Arc<tokio::sync::watch::Sender<usize>>,
    session_id: String,
    store: Option<Arc<Mutex<crate::store::Store>>>,
}

impl Default for AgentRegistry {
    fn default() -> Self {
        Self::new(String::new())
    }
}

impl AgentRegistry {
    pub(crate) fn new(session_id: String) -> Self {
        Self {
            nodes: Arc::new(Mutex::new(HashMap::new())),
            writers: Arc::new(Mutex::new(HashSet::new())),
            executions: Arc::new(tokio::sync::watch::channel(0).0),
            session_id,
            store: None,
        }
    }

    pub(crate) fn attach_store(&mut self, store: Arc<Mutex<crate::store::Store>>) {
        self.store = Some(store);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, AgentNode>> {
        self.nodes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn claim_writer(&self, agent_id: &str) -> Option<AgentWriterGuard> {
        let mut writers = self
            .writers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !writers.insert(agent_id.to_string()) {
            return None;
        }
        Some(AgentWriterGuard {
            registry: self.clone(),
            agent_id: agent_id.to_string(),
            released: false,
        })
    }

    pub(crate) fn writer_active(&self, agent_id: &str) -> bool {
        self.writers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(agent_id)
    }

    pub(crate) fn child_count(&self) -> usize {
        let mut ids: HashSet<String> = self
            .lock()
            .values()
            .filter(|node| node.depth > 0 && node.lifecycle != AgentLifecycle::Closed)
            .map(|node| node.agent_id.clone())
            .collect();
        ids.extend(
            self.writers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .iter()
                .cloned(),
        );
        ids.len()
    }

    fn supervise(&self, node: &mut AgentNode) {
        if node.supervised {
            return;
        }
        node.supervised = true;
        self.executions.send_modify(|count| *count += 1);
    }

    fn finish_supervision(&self, node: &mut AgentNode) {
        if !node.supervised {
            return;
        }
        node.supervised = false;
        self.executions
            .send_modify(|count| *count = count.saturating_sub(1));
    }

    pub(crate) async fn wait_for_idle(&self) {
        let mut executions = self.executions.subscribe();
        let _ = executions
            .wait_for(|count| *count == 0)
            .await
            .expect("the registry retains the execution sender");
    }

    pub(crate) fn register_root(&self, agent_id: String) {
        self.lock()
            .entry(agent_id.clone())
            .or_insert_with(|| AgentNode::root(agent_id));
    }

    /// A child may be adopted by a new direct parent only between runs.
    /// An active run or a closed conversation cannot be taken over.
    pub(crate) fn activate_child(&self, mut child: AgentNode) -> Result<(), String> {
        let mut nodes = self.lock();
        if let Some(existing) = nodes.get(&child.agent_id) {
            if existing.lifecycle.is_active() {
                return Err(format!("Agent {} is already running", child.agent_id));
            }
            if existing.lifecycle == AgentLifecycle::Closed {
                return Err(format!("Agent {} is closed", child.agent_id));
            }
        }
        if let Some(mut existing) = nodes.remove(&child.agent_id) {
            existing.remember_current_run();
            child.mailbox = existing.mailbox;
            child.run_history = existing.run_history;
        }
        self.supervise(&mut child);
        nodes.insert(child.agent_id.clone(), child);
        Ok(())
    }

    pub(crate) fn begin_run(&self, agent_id: &str, run_id: String) -> bool {
        let mut nodes = self.lock();
        let Some(node) = nodes.get_mut(agent_id) else {
            return false;
        };
        if node.lifecycle == AgentLifecycle::Closed
            || (node.lifecycle.is_active()
                && node
                    .current_run
                    .as_ref()
                    .is_some_and(|run| run.run_id != run_id))
        {
            return false;
        }
        if node
            .current_run
            .as_ref()
            .is_some_and(|run| run.run_id != run_id)
        {
            node.remember_current_run();
        }
        let parent_agent_id = node.parent_agent_id.clone();
        let depth = node.depth;
        self.supervise(node);
        node.lifecycle = AgentLifecycle::Running;
        node.current_run = Some(AgentRunState::new(
            run_id,
            AgentRunStatus::Running,
            parent_agent_id,
            depth,
        ));
        true
    }

    pub(crate) fn settle_run(&self, agent_id: &str, run_id: &str, status: AgentRunStatus) -> bool {
        let mut nodes = self.lock();
        let Some(node) = nodes.get_mut(agent_id) else {
            return false;
        };
        if node
            .current_run
            .as_ref()
            .is_none_or(|run| run.run_id != run_id)
        {
            return false;
        }
        if let Some(run) = node.current_run.as_mut() {
            run.status = status;
        }
        node.lifecycle = if node.lifecycle == AgentLifecycle::Closed {
            AgentLifecycle::Closed
        } else if status == AgentRunStatus::Lost {
            AgentLifecycle::Lost
        } else {
            AgentLifecycle::Idle
        };
        self.finish_supervision(node);
        true
    }

    pub(crate) fn restore(&self, mut node: AgentNode) {
        let mut nodes = self.lock();
        if let Some(mut previous) = nodes.remove(&node.agent_id) {
            if previous.supervised {
                nodes.insert(previous.agent_id.clone(), previous);
                return;
            }
            previous.remember_current_run();
            for run in previous.run_history {
                if let Some(existing) = node
                    .run_history
                    .iter_mut()
                    .find(|existing| existing.run_id == run.run_id)
                {
                    *existing = run;
                } else {
                    node.run_history.push(run);
                }
            }
            node.mailbox = previous.mailbox;
        }
        nodes.insert(node.agent_id.clone(), node);
    }

    pub(crate) fn close(&self, agent_id: &str) -> bool {
        let mut nodes = self.lock();
        let Some(node) = nodes.get_mut(agent_id) else {
            return false;
        };
        node.lifecycle = AgentLifecycle::Closed;
        true
    }

    pub(crate) fn close_all(&self) {
        let ids: Vec<String> = self.lock().keys().cloned().collect();
        for agent_id in ids {
            self.close(&agent_id);
        }
    }

    #[cfg(test)]
    pub(crate) fn node(&self, agent_id: &str) -> Option<AgentNode> {
        self.lock().get(agent_id).cloned()
    }

    pub(crate) fn run_status(&self, agent_id: &str, run_id: &str) -> Option<AgentRunStatus> {
        self.lock()
            .get(agent_id)
            .and_then(|node| node.current_run.as_ref())
            .filter(|run| run.run_id == run_id)
            .map(|run| run.status)
    }

    pub(crate) fn is_active(&self, agent_id: &str) -> bool {
        self.lock()
            .get(agent_id)
            .is_some_and(|node| node.lifecycle.is_active())
    }

    pub(crate) fn push_steer(&self, agent_id: &str, steer: AgentSteer) -> Result<(), String> {
        let mut nodes = self.lock();
        let Some(node) = nodes.get_mut(agent_id) else {
            return Err(format!("Agent {agent_id} is not registered"));
        };
        if !node.lifecycle.is_active() {
            return Err(format!("Agent {agent_id} is not active"));
        }
        let Some(run) = &node.current_run else {
            return Err(format!("Agent {agent_id} has no current run"));
        };
        if steer.notification.is_none()
            && let Some(store) = &self.store
        {
            store
                .lock()
                .expect("store lock")
                .enqueue_agent_steer(&self.session_id, agent_id, &run.run_id, &steer)
                .map_err(|error| format!("Failed to persist Agent mailbox message: {error:?}"))?;
        }
        node.mailbox.push(steer);
        Ok(())
    }

    pub(crate) fn prepend_steer(&self, agent_id: &str, steer: AgentSteer) -> bool {
        let mut nodes = self.lock();
        let Some(node) = nodes.get_mut(agent_id) else {
            return false;
        };
        if !node.lifecycle.is_active() {
            return false;
        }
        node.mailbox.prepend(steer);
        true
    }

    pub(crate) fn drain_mailbox(&self, agent_id: &str) -> Vec<AgentSteer> {
        self.lock()
            .get_mut(agent_id)
            .map(|node| node.mailbox.drain())
            .unwrap_or_default()
    }

    pub(crate) fn consume_steer(&self, message_id: &str) -> Result<(), String> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        store
            .lock()
            .expect("store lock")
            .mark_agent_steer_consumed(&self.session_id, message_id)
            .map_err(|error| format!("Failed to acknowledge Agent mailbox message: {error:?}"))
    }

    pub(crate) fn drain_notifications(&self, agent_id: &str) -> Vec<AgentSteer> {
        self.lock()
            .get_mut(agent_id)
            .map(|node| node.mailbox.drain_notifications())
            .unwrap_or_default()
    }

    pub(crate) fn restore_mailbox(&self) -> Result<(), pig_protocol::CoreError> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        let agent_ids: Vec<String> = self.lock().keys().cloned().collect();
        let mut pending = Vec::new();
        {
            let store = store.lock().expect("store lock");
            for agent_id in agent_ids {
                pending.extend(store.pending_agent_steers(&self.session_id, &agent_id)?);
            }
        }
        let mut nodes = self.lock();
        for stored in pending
            .into_iter()
            .filter(|stored| stored.steer.notification.is_none())
        {
            if let Some(node) = nodes.get_mut(&stored.agent_id) {
                node.mailbox.restore(stored.steer);
            }
        }
        Ok(())
    }

    pub(crate) fn retain_mailbox(&self, agent_id: &str, keep: impl FnMut(&AgentSteer) -> bool) {
        if let Some(node) = self.lock().get_mut(agent_id) {
            node.mailbox.retain(keep);
        }
    }

    pub(crate) fn record_partial(&self, agent_id: &str, run_id: &str, text: String) -> bool {
        let mut nodes = self.lock();
        let Some(node) = nodes.get_mut(agent_id) else {
            return false;
        };
        if node
            .current_run
            .as_ref()
            .is_none_or(|run| run.run_id != run_id)
        {
            return false;
        }
        node.partial = Some(AgentPartial {
            run_id: run_id.to_string(),
            text,
        });
        true
    }

    pub(crate) fn partial(&self, agent_id: &str) -> Option<AgentPartial> {
        self.lock()
            .get(agent_id)
            .and_then(|node| node.partial.clone())
    }

    pub(crate) fn snapshot(&self) -> Vec<AgentNode> {
        self.lock().values().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_run_transitions_keep_identity_and_ignore_stale_settlement() {
        let registry = AgentRegistry::default();
        registry.register_root("root".into());
        registry
            .activate_child(AgentNode::child(
                "child".into(),
                "root".into(),
                1,
                "run-1".into(),
            ))
            .expect("activate child");
        assert!(registry.is_active("child"));
        assert!(registry.begin_run("child", "run-1".into()));
        assert!(!registry.settle_run("child", "run-old", AgentRunStatus::Failed));
        assert!(registry.settle_run("child", "run-1", AgentRunStatus::Completed));
        assert!(!registry.is_active("child"));

        let node = registry.node("child").expect("child node");
        assert_eq!(node.parent_agent_id.as_deref(), Some("root"));
        assert_eq!(node.depth, 1);
        assert_eq!(node.lifecycle, AgentLifecycle::Idle);
        assert_eq!(
            node.current_run,
            Some(AgentRunState::new(
                "run-1".into(),
                AgentRunStatus::Completed,
                Some("root".into()),
                1,
            ))
        );
    }

    #[test]
    fn mailbox_delivery_requires_an_active_node_and_drains_at_settlement() {
        let registry = AgentRegistry::default();
        registry
            .activate_child(AgentNode::child(
                "child".into(),
                "root".into(),
                1,
                "run-1".into(),
            ))
            .expect("activate child");
        assert!(
            registry
                .push_steer("child", AgentSteer::message("work".into()))
                .is_ok()
        );
        assert!(
            registry
                .push_steer("missing", AgentSteer::message("work".into()))
                .is_err()
        );
        registry.settle_run("child", "run-1", AgentRunStatus::Completed);
        assert!(
            registry
                .push_steer("child", AgentSteer::message("late".into()))
                .is_err()
        );
        assert_eq!(registry.drain_mailbox("child").len(), 1);
        assert!(registry.drain_mailbox("child").is_empty());
    }

    #[test]
    fn pending_steer_restores_into_a_rebuilt_agent_node() {
        let dir = std::env::temp_dir().join(format!(
            "pig-agent-registry-mailbox-{}-{}",
            std::process::id(),
            crate::model_io::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Arc::new(Mutex::new(crate::store::Store::open(&dir).expect("store")));
        let mut registry = AgentRegistry::new("s1".into());
        registry.attach_store(store.clone());
        registry
            .activate_child(AgentNode::child(
                "child".into(),
                "root".into(),
                1,
                "run-1".into(),
            ))
            .expect("activate child");
        registry
            .push_steer("child", AgentSteer::message("pending".into()))
            .expect("durable enqueue");
        let steer_id = registry
            .drain_mailbox("child")
            .into_iter()
            .next()
            .expect("pending")
            .id;

        // Requeue without consuming to model a process ending with pending input.
        registry.prepend_steer(
            "child",
            AgentSteer::restored(steer_id.clone(), "pending".into(), None),
        );
        let mut restored = AgentRegistry::new("s1".into());
        restored.attach_store(store);
        restored.restore(AgentNode::child(
            "child".into(),
            "root".into(),
            1,
            "run-1".into(),
        ));
        restored.settle_run("child", "run-1", AgentRunStatus::Completed);
        restored.restore_mailbox().expect("restore mailbox");
        restored
            .activate_child(AgentNode::child(
                "child".into(),
                "root".into(),
                1,
                "run-2".into(),
            ))
            .expect("resume keeps pending mailbox");
        let pending = restored.drain_mailbox("child");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, steer_id);
        restored.consume_steer(&pending[0].id).expect("consume");
        restored.restore_mailbox().expect("consume is durable");
        assert!(restored.drain_mailbox("child").is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn writer_reservation_is_exclusive_and_counts_once_toward_the_child_budget() {
        let registry = AgentRegistry::default();
        let mut writer = registry.claim_writer("child").expect("reserve writer");
        assert_eq!(registry.child_count(), 1);
        assert!(registry.claim_writer("child").is_none());
        registry
            .activate_child(AgentNode::child(
                "child".into(),
                "root".into(),
                1,
                "run-1".into(),
            ))
            .expect("activate reserved child");
        assert_eq!(registry.child_count(), 1);
        writer.release();
        assert!(!registry.writer_active("child"));
        assert_eq!(registry.child_count(), 1);
        registry.close("child");
        assert_eq!(registry.child_count(), 0);
    }

    #[tokio::test]
    async fn close_waits_for_queued_and_running_descendants_to_settle() {
        let registry = AgentRegistry::default();
        for agent_id in ["queued", "running"] {
            registry
                .activate_child(AgentNode::child(
                    agent_id.into(),
                    "root".into(),
                    1,
                    format!("run-{agent_id}"),
                ))
                .expect("activate child");
        }
        registry.begin_run("running", "run-running".into());
        registry.close_all();
        let waiter = tokio::spawn({
            let registry = registry.clone();
            async move { registry.wait_for_idle().await }
        });
        assert!(registry.settle_run("queued", "run-queued", AgentRunStatus::Cancelled));
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        assert!(registry.settle_run("running", "run-running", AgentRunStatus::Completed));
        tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .expect("idle notification")
            .expect("wait task");
        assert_eq!(
            registry.node("running").expect("closed child").lifecycle,
            AgentLifecycle::Closed
        );
    }

    #[test]
    fn partial_output_is_scoped_to_the_current_run() {
        let registry = AgentRegistry::default();
        registry
            .activate_child(AgentNode::child(
                "child".into(),
                "root".into(),
                1,
                "run-1".into(),
            ))
            .expect("activate child");
        assert!(registry.record_partial("child", "run-1", "first".into()));
        assert!(!registry.record_partial("child", "run-old", "stale".into()));
        assert_eq!(
            registry.partial("child").expect("partial"),
            AgentPartial {
                run_id: "run-1".into(),
                text: "first".into(),
            }
        );
    }

    #[test]
    fn resume_replaces_the_current_run_but_not_the_agent_identity() {
        let registry = AgentRegistry::default();
        registry
            .activate_child(AgentNode::child(
                "child".into(),
                "root".into(),
                1,
                "run-1".into(),
            ))
            .expect("activate child");
        registry.settle_run("child", "run-1", AgentRunStatus::Cancelled);
        registry
            .activate_child(AgentNode::child(
                "child".into(),
                "new-parent".into(),
                2,
                "run-2".into(),
            ))
            .expect("resume idle child");
        assert!(registry.is_active("child"));
        let node = registry.node("child").expect("child");
        assert_eq!(
            node.current_run,
            Some(AgentRunState::new(
                "run-2".into(),
                AgentRunStatus::Queued,
                Some("new-parent".into()),
                2,
            ))
        );
        assert_eq!(
            node.run("run-1")
                .and_then(|run| run.parent_agent_id.as_deref()),
            Some("root")
        );
        assert!(registry.close("child"));
        assert!(registry.settle_run("child", "run-2", AgentRunStatus::Cancelled));
        assert!(
            registry
                .activate_child(AgentNode::child(
                    "child".into(),
                    "root".into(),
                    1,
                    "run-3".into(),
                ))
                .is_err()
        );
        assert_eq!(
            registry.node("child").expect("child").lifecycle,
            AgentLifecycle::Closed
        );
    }
}
