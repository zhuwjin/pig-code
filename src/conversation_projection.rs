//! Projection from the core event stream into the shared conversation wire.
//!
//! The root session and every subagent use the same `ConversationItem` /
//! `ConversationUpdate` / `ConversationSnapshot` protocol. Subagents emit it
//! directly from their driver; the root event bus keeps emitting its richer
//! events for plugins and tests, and this projection turns them into the only
//! item path the app UI consumes. It also settles the parent Agent card when a
//! child conversation finishes.

use std::collections::HashMap;

use pig_protocol::{ConversationAgentCard, ConversationItem, ConversationToolStatus, Event};

#[derive(Default)]
struct ConversationState {
    items: Vec<ConversationItem>,
    active_turn: Option<String>,
    next_id: u64,
}

pub(crate) struct Projected {
    pub pass: bool,
    pub events: Vec<Event>,
}

impl Projected {
    fn pass(_event: &Event) -> Self {
        Self {
            pass: true,
            events: Vec::new(),
        }
    }

    fn only(events: Vec<Event>) -> Self {
        Self {
            pass: false,
            events,
        }
    }
}

pub(crate) struct ConversationProjection {
    sessions: HashMap<String, ConversationState>,
    finish_seq: u64,
}

impl ConversationProjection {
    pub(crate) fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            finish_seq: 0,
        }
    }

    pub(crate) fn project(&mut self, event: &Event) -> Projected {
        match event {
            Event::SessionConfigured { session_id, .. } => {
                self.sessions.remove(session_id);
                Projected::pass(event)
            }
            Event::TurnStarted {
                session_id,
                turn_id,
            } => {
                self.state(session_id).active_turn = Some(turn_id.clone());
                Projected::pass(event)
            }
            Event::TurnComplete { session_id, .. } | Event::TurnAborted { session_id } => {
                let mut events = Vec::new();
                if let Some(state) = self.sessions.get_mut(session_id) {
                    state.active_turn = None;
                    settle_reasoning(session_id, state, &mut events);
                }
                let mut projected = Projected::pass(event);
                projected.events = events;
                projected
            }
            Event::UserMessage {
                session_id,
                text,
                files,
                image_nums,
                sent_at_ms,
            } => {
                let state = self.state(session_id);
                let id = state.next_id("user");
                let item = ConversationItem::User {
                    id: id.clone(),
                    text: text.clone(),
                    files: files.clone(),
                    image_nums: image_nums.clone(),
                    sent_at_ms: *sent_at_ms,
                };
                Projected::only(vec![update(session_id, None, state, item)])
            }
            Event::ReasoningDelta {
                session_id,
                item_id,
                delta,
            } => {
                let state = self.state(session_id);
                let mut events = Vec::new();
                if !state.items.iter().any(|item| item.id() == item_id) {
                    settle_reasoning(session_id, state, &mut events);
                }
                let turn_id = state.active_turn.clone();
                if let Some(ConversationItem::Reasoning {
                    text, streaming, ..
                }) = state.items.iter_mut().find(|item| item.id() == item_id)
                {
                    text.push_str(delta);
                    *streaming = true;
                    let item = state
                        .items
                        .iter()
                        .find(|item| item.id() == item_id)
                        .cloned()
                        .expect("reasoning item");
                    events.push(update(session_id, None, state, item));
                } else {
                    let item = ConversationItem::Reasoning {
                        id: item_id.clone(),
                        text: delta.clone(),
                        streaming: true,
                        turn_id,
                        started_at_ms: Some(now_ms()),
                        duration_ms: None,
                        sources: Vec::new(),
                    };
                    events.push(update(session_id, None, state, item));
                }
                Projected::only(events)
            }
            Event::TextDelta {
                session_id,
                item_id,
                delta,
            } => {
                let state = self.state(session_id);
                let mut events = Vec::new();
                settle_reasoning(session_id, state, &mut events);
                let turn_id = state.active_turn.clone();
                if let Some(ConversationItem::Markdown {
                    text, streaming, ..
                }) = state.items.iter_mut().find(|item| item.id() == item_id)
                {
                    text.push_str(delta);
                    *streaming = true;
                    let item = state
                        .items
                        .iter()
                        .find(|item| item.id() == item_id)
                        .cloned()
                        .expect("markdown item");
                    events.push(update(session_id, None, state, item));
                } else {
                    let item = ConversationItem::Markdown {
                        id: item_id.clone(),
                        text: delta.clone(),
                        streaming: true,
                        turn_id,
                        started_at_ms: Some(now_ms()),
                        duration_ms: None,
                        sources: Vec::new(),
                    };
                    events.push(update(session_id, None, state, item));
                }
                Projected::only(events)
            }
            Event::TextDone {
                session_id,
                item_id,
                full_text,
            } => {
                let state = self.state(session_id);
                let mut events = Vec::new();
                settle_reasoning(session_id, state, &mut events);
                let turn_id = state.active_turn.clone();
                let item = ConversationItem::Markdown {
                    id: item_id.clone(),
                    text: full_text.clone(),
                    streaming: false,
                    turn_id,
                    started_at_ms: Some(now_ms()),
                    duration_ms: None,
                    sources: Vec::new(),
                };
                events.push(update(session_id, None, state, item));
                Projected::only(events)
            }
            Event::Sources {
                session_id,
                item_id,
                sources,
            } => {
                let state = self.state(session_id);
                let Some(item) = state.items.iter_mut().find(|item| item.id() == item_id) else {
                    return Projected::only(Vec::new());
                };
                if let ConversationItem::Markdown {
                    sources: current, ..
                } = item
                {
                    *current = sources.clone();
                }
                let item = item.clone();
                Projected::only(vec![update(session_id, None, state, item)])
            }
            Event::ToolInputStart {
                session_id,
                item_id,
                tool,
            } => {
                let state = self.state(session_id);
                let mut events = Vec::new();
                settle_reasoning(session_id, state, &mut events);
                let turn_id = state.active_turn.clone();
                let item = ConversationItem::Tool {
                    id: item_id.clone(),
                    tool: tool.clone(),
                    summary: String::new(),
                    detail: String::new(),
                    output: String::new(),
                    status: ConversationToolStatus::Input,
                    turn_id,
                    edit: None,
                    live_note: None,
                    agents: Vec::new(),
                    approval_pending: false,
                };
                events.push(update(session_id, None, state, item));
                Projected::only(events)
            }
            Event::ToolInputDelta {
                session_id,
                item_id,
                delta,
            } => {
                let state = self.state(session_id);
                let Some(item) = state.items.iter_mut().find(|item| item.id() == item_id) else {
                    return Projected::only(Vec::new());
                };
                if let ConversationItem::Tool { detail, .. } = item {
                    detail.push_str(delta);
                }
                let item = item.clone();
                Projected::only(vec![update(session_id, None, state, item)])
            }
            Event::ToolInputDiscard {
                session_id,
                item_id,
            } => {
                let state = self.state(session_id);
                Projected::only(vec![remove(session_id, None, state, vec![item_id.clone()])])
            }
            Event::ToolCallBegin {
                session_id,
                item_id,
                tool,
                input_summary,
                detail,
            } => {
                let state = self.state(session_id);
                let mut events = Vec::new();
                settle_reasoning(session_id, state, &mut events);
                let turn_id = state.active_turn.clone();
                let item = ConversationItem::Tool {
                    id: item_id.clone(),
                    tool: tool.clone(),
                    summary: input_summary.clone(),
                    detail: detail.clone(),
                    output: String::new(),
                    status: ConversationToolStatus::Running,
                    turn_id,
                    edit: None,
                    live_note: None,
                    agents: Vec::new(),
                    approval_pending: false,
                };
                events.push(update(session_id, None, state, item));
                Projected::only(events)
            }
            Event::ToolCallEnd {
                session_id,
                item_id,
                output,
                is_error,
                stopped,
                edit,
            } => {
                let state = self.state(session_id);
                let Some(existing) = state.items.iter_mut().find(|item| item.id() == item_id)
                else {
                    return Projected::only(Vec::new());
                };
                if let ConversationItem::Tool {
                    output: current,
                    status,
                    edit: current_edit,
                    live_note,
                    approval_pending,
                    ..
                } = existing
                {
                    *current = output.clone();
                    *status = if *stopped {
                        ConversationToolStatus::Stopped
                    } else if *is_error {
                        ConversationToolStatus::Error
                    } else {
                        ConversationToolStatus::Done
                    };
                    *current_edit = edit.clone();
                    *live_note = None;
                    *approval_pending = false;
                }
                let item = existing.clone();
                Projected::only(vec![update(session_id, None, state, item)])
            }
            Event::SubagentProgress {
                session_id,
                item_id,
                note,
            } => {
                let state = self.state(session_id);
                let Some(item) = state.items.iter_mut().find(|item| item.id() == item_id) else {
                    return Projected::only(Vec::new());
                };
                if let ConversationItem::Tool { live_note, .. } = item {
                    *live_note = Some(note.clone());
                }
                let item = item.clone();
                Projected::only(vec![update(session_id, None, state, item)])
            }
            Event::SubagentCard {
                session_id,
                item_id,
                agent_id,
                profile,
                description,
                model,
                background,
            } => {
                let state = self.state(session_id);
                let Some(item) = state.items.iter_mut().find(|item| item.id() == item_id) else {
                    return Projected::only(Vec::new());
                };
                if let ConversationItem::Tool { agents, .. } = item {
                    agents.push(ConversationAgentCard {
                        agent_id: agent_id.clone(),
                        profile: profile.clone(),
                        description: description.clone(),
                        model: model.clone(),
                        background: *background,
                        finished: false,
                        finished_seq: None,
                        live_note: None,
                    });
                }
                let item = item.clone();
                Projected::only(vec![update(session_id, None, state, item)])
            }
            Event::TaskNotificationBatch {
                session_id,
                notifications,
                sent_at_ms,
                ..
            } => {
                let state = self.state(session_id);
                let id = state.next_id("notification");
                let item = ConversationItem::Notification {
                    id,
                    notifications: notifications.clone(),
                    sent_at_ms: *sent_at_ms,
                };
                Projected::only(vec![update(session_id, None, state, item)])
            }
            Event::CompactStarted { session_id, .. } => {
                let state = self.state(session_id);
                let id = state.next_id("note");
                let item = ConversationItem::Note {
                    id,
                    text: String::new(),
                    error: false,
                    note_kind: Some("compacting".into()),
                    used_before: None,
                    used_after: None,
                    summary: None,
                };
                Projected::only(vec![update(session_id, None, state, item)])
            }
            Event::ContextCompacted {
                session_id,
                note,
                used_before,
                used_after,
                summary,
                ..
            } => {
                let state = self.state(session_id);
                let remove_ids: Vec<String> = state
                    .items
                    .iter()
                    .filter(|item| matches!(item, ConversationItem::Note { note_kind: Some(kind), .. } if kind == "compacting"))
                    .map(|item| item.id().to_string())
                    .collect();
                let mut events = Vec::new();
                if !remove_ids.is_empty() {
                    events.push(remove(session_id, None, state, remove_ids));
                }
                let id = state.next_id("note");
                let item = ConversationItem::Note {
                    id,
                    text: note.clone(),
                    error: false,
                    note_kind: Some("compacted".into()),
                    used_before: *used_before,
                    used_after: *used_after,
                    summary: summary.clone(),
                };
                events.push(update(session_id, None, state, item));
                Projected::only(events)
            }
            Event::MessageRecalled { session_id } => {
                let state = self.state(session_id);
                let cut = state
                    .items
                    .iter()
                    .rposition(|item| matches!(item, ConversationItem::User { .. }));
                let ids = cut.map_or_else(Vec::new, |index| {
                    state.items[index..]
                        .iter()
                        .map(|item| item.id().to_string())
                        .collect()
                });
                let mut projected = Projected::pass(event);
                if !ids.is_empty() {
                    projected.events.push(remove(session_id, None, state, ids));
                }
                projected
            }
            Event::ConversationStatus {
                session_id,
                agent_id,
                running,
            } => {
                let mut projected = Projected::pass(event);
                projected.events = if *running {
                    self.resume_parent_agent(session_id, agent_id)
                } else {
                    self.finish_parent_agent(session_id, agent_id)
                };
                projected
            }
            _ => Projected::pass(event),
        }
    }

    fn state(&mut self, session_id: &str) -> &mut ConversationState {
        self.sessions.entry(session_id.to_string()).or_default()
    }

    fn resume_parent_agent(&mut self, session_id: &str, agent_id: &str) -> Vec<Event> {
        let Some(state) = self.sessions.get_mut(session_id) else {
            return Vec::new();
        };
        let mut events = Vec::new();
        for row in &mut state.items {
            let ConversationItem::Tool { agents, .. } = row else {
                continue;
            };
            let Some(agent) = agents.iter_mut().find(|agent| agent.agent_id == agent_id) else {
                continue;
            };
            if agent.finished || agent.live_note.is_some() {
                agent.finished = false;
                agent.finished_seq = None;
                agent.live_note = None;
                let row = row.clone();
                events.push(update(session_id, None, state, row));
            }
            break;
        }
        events
    }

    fn finish_parent_agent(&mut self, session_id: &str, agent_id: &str) -> Vec<Event> {
        let Some(state) = self.sessions.get_mut(session_id) else {
            return Vec::new();
        };
        let mut events = Vec::new();
        for row in &mut state.items {
            let ConversationItem::Tool { agents, .. } = row else {
                continue;
            };
            let Some(agent) = agents.iter_mut().find(|agent| agent.agent_id == agent_id) else {
                continue;
            };
            if !agent.finished {
                agent.finished = true;
                self.finish_seq += 1;
                agent.finished_seq = Some(self.finish_seq);
            }
            agent.live_note = None;
            let row = row.clone();
            events.push(update(session_id, None, state, row));
            break;
        }
        events
    }
}

impl ConversationState {
    fn next_id(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        format!("{prefix}-{}", self.next_id)
    }
}

fn update(
    session_id: &str,
    agent_id: Option<&str>,
    state: &mut ConversationState,
    item: ConversationItem,
) -> Event {
    if let Some(index) = state
        .items
        .iter()
        .position(|current| current.id() == item.id())
    {
        state.items[index] = item.clone();
    } else {
        state.items.push(item.clone());
    }
    Event::ConversationUpdate {
        session_id: session_id.to_string(),
        agent_id: agent_id.map(str::to_string),
        item,
    }
}

fn remove(
    session_id: &str,
    agent_id: Option<&str>,
    state: &mut ConversationState,
    item_ids: Vec<String>,
) -> Event {
    state
        .items
        .retain(|item| !item_ids.iter().any(|id| id == item.id()));
    Event::ConversationRemove {
        session_id: session_id.to_string(),
        agent_id: agent_id.map(str::to_string),
        item_ids,
    }
}

fn settle_reasoning(session_id: &str, state: &mut ConversationState, events: &mut Vec<Event>) {
    let now = now_ms();
    let mut changed = Vec::new();
    for item in &mut state.items {
        if let ConversationItem::Reasoning {
            streaming,
            started_at_ms,
            duration_ms,
            ..
        } = item
        {
            if *streaming {
                *streaming = false;
                if duration_ms.is_none() {
                    *duration_ms = started_at_ms.map(|started| now.saturating_sub(started));
                }
                changed.push(item.clone());
            }
        }
    }
    for item in changed {
        events.push(update(session_id, None, state, item));
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pig_protocol::Event;

    #[test]
    fn root_items_become_conversation_updates() {
        let mut projection = ConversationProjection::new();
        projection.project(&Event::TurnStarted {
            session_id: "s1".into(),
            turn_id: "t1".into(),
        });
        let events = projection
            .project(&Event::UserMessage {
                session_id: "s1".into(),
                text: "hello".into(),
                files: Vec::new(),
                image_nums: Vec::new(),
                sent_at_ms: 1,
            })
            .events;
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], Event::ConversationUpdate { .. }));

        let delta = projection
            .project(&Event::TextDelta {
                session_id: "s1".into(),
                item_id: "answer".into(),
                delta: "hi".into(),
            })
            .events;
        assert_eq!(delta.len(), 1);
        let Event::ConversationUpdate { item, .. } = &delta[0] else {
            panic!("expected update");
        };
        let ConversationItem::Markdown {
            text,
            streaming,
            turn_id,
            ..
        } = item
        else {
            panic!("expected markdown");
        };
        assert_eq!(text, "hi");
        assert!(streaming);
        assert_eq!(turn_id.as_deref(), Some("t1"));
    }

    #[test]
    fn subagent_conversation_events_pass_through() {
        let mut projection = ConversationProjection::new();
        let item = ConversationItem::Tool {
            id: "t1-0".into(),
            tool: "TaskWait".into(),
            summary: "wait for b1".into(),
            detail: "{}".into(),
            output: String::new(),
            status: ConversationToolStatus::Running,
            turn_id: None,
            edit: None,
            live_note: None,
            agents: Vec::new(),
            approval_pending: false,
        };
        let snapshot = projection.project(&Event::ConversationSnapshot {
            session_id: "s1".into(),
            agent_id: Some("a1".into()),
            title: Some("child".into()),
            subtitle: Some("p · m".into()),
            items: vec![item.clone()],
            running: true,
        });
        assert!(snapshot.pass);
        assert!(snapshot.events.is_empty());

        let update = projection.project(&Event::ConversationUpdate {
            session_id: "s1".into(),
            agent_id: Some("a1".into()),
            item,
        });
        assert!(update.pass);
        assert!(update.events.is_empty());
    }

    #[test]
    fn finished_subagent_updates_the_parent_agent_card() {
        let mut projection = ConversationProjection::new();
        projection.project(&Event::ToolCallBegin {
            session_id: "s1".into(),
            item_id: "agent-call".into(),
            tool: "Agent".into(),
            input_summary: "delegate".into(),
            detail: "{}".into(),
        });
        projection.project(&Event::SubagentCard {
            session_id: "s1".into(),
            item_id: "agent-call".into(),
            agent_id: "a1".into(),
            profile: "coder".into(),
            description: "child".into(),
            model: "m".into(),
            background: false,
        });
        let projected = projection.project(&Event::ConversationStatus {
            session_id: "s1".into(),
            agent_id: "a1".into(),
            running: false,
        });
        let events = projected.events;
        assert!(projected.pass);
        let row = events
            .iter()
            .find_map(|event| match event {
                Event::ConversationUpdate {
                    item: ConversationItem::Tool { agents, .. },
                    ..
                } => Some(agents.clone()),
                _ => None,
            })
            .expect("parent tool update");
        assert!(row[0].finished);
        assert_eq!(row[0].finished_seq, Some(1));

        // Resuming (e.g. steering a background child) clears the settled state.
        let resumed = projection.project(&Event::ConversationStatus {
            session_id: "s1".into(),
            agent_id: "a1".into(),
            running: true,
        });
        let row = resumed
            .events
            .iter()
            .find_map(|event| match event {
                Event::ConversationUpdate {
                    item: ConversationItem::Tool { agents, .. },
                    ..
                } => Some(agents.clone()),
                _ => None,
            })
            .expect("parent tool update");
        assert!(!row[0].finished);
        assert_eq!(row[0].finished_seq, None);
    }

    #[test]
    fn conversation_item_serializes_like_chat_item() {
        let item = ConversationItem::Tool {
            id: "call".into(),
            tool: "Agent".into(),
            summary: "subagent".into(),
            detail: "{}".into(),
            output: String::new(),
            status: ConversationToolStatus::Running,
            turn_id: Some("turn".into()),
            edit: None,
            live_note: Some("working".into()),
            agents: vec![ConversationAgentCard {
                agent_id: "a1".into(),
                profile: "coder".into(),
                description: "child".into(),
                model: "m".into(),
                background: false,
                finished: true,
                finished_seq: Some(1),
                live_note: Some("note".into()),
            }],
            approval_pending: true,
        };
        let value = serde_json::to_value(item).expect("serialize");
        assert_eq!(value["kind"], "tool");
        assert_eq!(value["turnId"], "turn");
        assert_eq!(value["liveNote"], "working");
        assert_eq!(value["approvalPending"], true);
        assert_eq!(value["agents"][0]["agent_id"], "a1");
        assert_eq!(value["agents"][0]["finishedSeq"], 1);
        assert_eq!(value["agents"][0]["live_note"], "note");
    }
}
