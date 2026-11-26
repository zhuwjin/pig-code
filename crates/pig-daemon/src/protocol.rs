use pig_protocol::{Event, SessionStateSnapshot};

pub use pig_protocol::{
    DAEMON_PROTOCOL_VERSION, DaemonEndpoint, DaemonMessage, DaemonRequest, ProjectedEventBatch,
};

pub const DAEMON_HISTORY: usize = 1024;

pub fn snapshot_events(snapshot: &SessionStateSnapshot) -> Vec<Event> {
    let mut events = Vec::with_capacity(
        9 + snapshot.pending_approvals.len() + snapshot.pending_questions.len(),
    );
    if let Some(configuration) = &snapshot.configuration {
        events.push(Event::SessionConfigured {
            session_id: snapshot.session_id.clone(),
            cwd: configuration.cwd.clone(),
            model: configuration.model.clone(),
            provider_name: configuration.provider_name.clone(),
            provider_id: configuration.provider_id.clone(),
            model_id: configuration.model_id.clone(),
            reasoning_level: configuration.reasoning_level.clone(),
            permissions: configuration.permissions,
            plan_enabled: configuration.plan_enabled,
        });
    }
    events.push(Event::ConversationSnapshot {
        session_id: snapshot.session_id.clone(),
        agent_id: None,
        title: None,
        subtitle: None,
        items: snapshot.items.clone(),
        running: snapshot.running,
        run_id: None,
    });
    events.push(Event::QueueChanged {
        session_id: snapshot.session_id.clone(),
        items: snapshot.pending_queue.clone(),
    });
    events.push(Event::SteerChanged {
        session_id: snapshot.session_id.clone(),
        items: snapshot.steer_queue.clone(),
    });
    events.push(Event::TodoListChanged {
        session_id: snapshot.session_id.clone(),
        items: snapshot.todos.clone(),
    });
    events.push(Event::TaskListChanged {
        session_id: snapshot.session_id.clone(),
        tasks: snapshot.tasks.clone(),
    });
    events.push(Event::TaskNotificationQueueChanged {
        session_id: snapshot.session_id.clone(),
        notifications: snapshot.task_notifications.clone(),
    });
    if let Some(usage) = &snapshot.context_usage {
        events.push(Event::ContextUsage {
            session_id: snapshot.session_id.clone(),
            used: usage.used,
            total: usage.total,
            cache_read_total: usage.cache_read_total,
            input_total: usage.input_total,
        });
    }
    if let Some(breakdown) = &snapshot.context_breakdown {
        events.push(Event::ContextBreakdownChanged {
            session_id: snapshot.session_id.clone(),
            breakdown: breakdown.clone(),
        });
    }
    events.extend(
        snapshot
            .pending_approvals
            .iter()
            .map(|approval| Event::ApprovalRequested {
                session_id: snapshot.session_id.clone(),
                request_id: approval.request_id.clone(),
                tool: approval.tool.clone(),
                detail: approval.detail.clone(),
                danger_key: approval.danger_key.clone(),
                requested_write_roots: approval.requested_write_roots.clone(),
                available_decisions: approval.available_decisions.clone(),
            }),
    );
    events.extend(
        snapshot
            .pending_questions
            .iter()
            .map(|question| Event::QuestionRequested {
                session_id: snapshot.session_id.clone(),
                request_id: question.request_id.clone(),
                questions: question.questions.clone(),
            }),
    );
    events
}
