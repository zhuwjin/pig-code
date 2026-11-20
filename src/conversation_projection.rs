//! Projection from the core event stream into the shared conversation wire.
//!
//! The root session and every subagent use the same `ConversationItem` /
//! `ConversationUpdate` / `ConversationSnapshot` protocol. Subagents emit it
//! directly from their driver; the root event bus keeps emitting its richer
//! events for plugins and tests, and this projection turns them into the only
//! item path the app UI consumes. It also settles the parent Agent card when a
//! child conversation finishes.

use std::collections::HashMap;

use pig_protocol::{
    ConversationAgentCard, ConversationItem, ConversationToolStatus, Event, EventEnvelope,
};

#[derive(Default)]
struct ConversationState {
    items: Vec<ConversationItem>,
    active_turn: Option<String>,
    next_id: u64,
}

pub(crate) struct ProjectionResync {
    pub cursor: u64,
    pub session_id: Option<String>,
}

pub(crate) struct Projected {
    pub pass: bool,
    pub events: Vec<Event>,
    pub resync: Option<ProjectionResync>,
}

impl Projected {
    fn pass(_event: &Event) -> Self {
        Self {
            pass: true,
            events: Vec::new(),
            resync: None,
        }
    }

    fn only(events: Vec<Event>) -> Self {
        Self {
            pass: false,
            events,
            resync: None,
        }
    }

    fn resync(cursor: u64, session_id: Option<String>) -> Self {
        Self {
            pass: false,
            events: Vec::new(),
            resync: Some(ProjectionResync { cursor, session_id }),
        }
    }
}

#[derive(Clone)]
struct RunStatus {
    run_id: Option<String>,
    running: bool,
}

pub(crate) struct ConversationProjection {
    sessions: HashMap<String, ConversationState>,
    subagents: HashMap<String, HashMap<String, ConversationState>>,
    run_status: HashMap<(String, String), RunStatus>,
    finish_seq: u64,
}

/// Entry point for journal-driven projections. Cursor state is delivery
/// deduplication only; domain state remains in the individual projection.
pub(crate) struct ProjectionRegistry {
    conversation: ConversationProjection,
    epoch: Option<String>,
    cursor: u64,
}

impl ProjectionRegistry {
    pub(crate) fn new() -> Self {
        Self {
            conversation: ConversationProjection::new(),
            epoch: None,
            cursor: 0,
        }
    }

    pub(crate) fn cursor(&self) -> u64 {
        self.cursor
    }

    pub(crate) fn project(&mut self, envelope: &EventEnvelope) -> Projected {
        if self
            .epoch
            .as_deref()
            .is_some_and(|epoch| epoch != envelope.epoch)
        {
            return Projected::resync(self.cursor, envelope.session_id.clone());
        }
        if envelope.sequence <= self.cursor {
            return Projected::only(Vec::new());
        }
        if envelope.sequence != self.cursor + 1 {
            return Projected::resync(self.cursor, envelope.session_id.clone());
        }
        self.epoch = Some(envelope.epoch.clone());
        self.cursor = envelope.sequence;
        self.conversation.project(&envelope.event)
    }

    pub(crate) fn reset_with_snapshot(
        &mut self,
        snapshot: &pig_core::JournalSnapshot,
    ) -> Result<(), String> {
        self.conversation = ConversationProjection::new();
        self.epoch = Some(snapshot.epoch.clone());
        self.cursor = snapshot.oldest.saturating_sub(1);
        for envelope in &snapshot.events {
            if self.project(envelope).resync.is_some() {
                return Err("journal snapshot is not contiguous".to_string());
            }
        }
        if self.cursor != snapshot.cursor {
            return Err("journal snapshot cursor mismatch".to_string());
        }
        Ok(())
    }
}

impl ConversationProjection {
    pub(crate) fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            subagents: HashMap::new(),
            run_status: HashMap::new(),
            finish_seq: 0,
        }
    }

    pub(crate) fn project(&mut self, event: &Event) -> Projected {
        match event {
            Event::SessionReplayComplete { session_id } => {
                let Some(state) = self.sessions.get(session_id) else {
                    return Projected::pass(event);
                };
                Projected::only(vec![Event::ConversationSnapshot {
                    session_id: session_id.clone(),
                    agent_id: None,
                    title: None,
                    subtitle: None,
                    items: state.items.clone(),
                    running: state.active_turn.is_some(),
                    run_id: None,
                }])
            }
            Event::SessionConfigured { session_id, .. } => {
                self.sessions.remove(session_id);
                self.subagents.remove(session_id);
                self.run_status
                    .retain(|(session, _), _| session != session_id);
                Projected::pass(event)
            }
            Event::TurnStarted {
                session_id,
                turn_id,
                ..
            } => {
                self.state(session_id).active_turn = Some(turn_id.clone());
                Projected::pass(event)
            }
            Event::TurnComplete { session_id, .. } | Event::TurnAborted { session_id, .. } => {
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
                ..
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
                run_id,
                profile,
                description,
                model,
                background,
            } => {
                let mut card = ConversationAgentCard {
                    agent_id: agent_id.clone(),
                    run_id: run_id.clone(),
                    profile: profile.clone(),
                    description: description.clone(),
                    model: model.clone(),
                    background: *background,
                    finished: false,
                    finished_seq: None,
                    live_note: None,
                };
                self.apply_known_status_to_card(session_id, &mut card);
                if let Some(state) = self.sessions.get_mut(session_id)
                    && let Some(item) = state.items.iter_mut().find(|item| item.id() == item_id)
                {
                    if let ConversationItem::Tool { agents, .. } = item {
                        agents.push(card);
                    }
                    let item = item.clone();
                    return Projected::only(vec![update(session_id, None, state, item)]);
                }
                let parent_agent_id = self.subagents.get(session_id).and_then(|states| {
                    states
                        .iter()
                        .find(|(_, state)| state.items.iter().any(|item| item.id() == item_id))
                        .map(|(agent_id, _)| agent_id.clone())
                });
                let Some(parent_agent_id) = parent_agent_id else {
                    return Projected::only(Vec::new());
                };
                let state = self.nested_state(session_id, &parent_agent_id);
                let Some(item) = state.items.iter_mut().find(|item| item.id() == item_id) else {
                    return Projected::only(Vec::new());
                };
                if let ConversationItem::Tool { agents, .. } = item {
                    agents.push(card);
                }
                let item = item.clone();
                Projected::only(vec![update_for_agent(
                    session_id,
                    Some(&parent_agent_id),
                    None,
                    state,
                    item,
                )])
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
            Event::ConversationSnapshot {
                session_id,
                agent_id: Some(agent_id),
                title,
                subtitle,
                items,
                running,
                run_id,
            } => {
                let mut items = items.clone();
                for item in &mut items {
                    self.apply_known_status_to_item(session_id, item);
                }
                self.nested_state(session_id, agent_id).items = items.clone();
                Projected::only(vec![Event::ConversationSnapshot {
                    session_id: session_id.clone(),
                    agent_id: Some(agent_id.clone()),
                    title: title.clone(),
                    subtitle: subtitle.clone(),
                    items,
                    running: *running,
                    run_id: run_id.clone(),
                }])
            }
            Event::ConversationUpdate {
                session_id,
                agent_id: Some(agent_id),
                run_id,
                item,
            } => {
                let mut item = item.clone();
                self.apply_known_status_to_item(session_id, &mut item);
                let state = self.nested_state(session_id, agent_id);
                Projected::only(vec![update_for_agent(
                    session_id,
                    Some(agent_id),
                    run_id.clone(),
                    state,
                    item,
                )])
            }
            Event::ConversationRemove {
                session_id,
                agent_id: Some(agent_id),
                item_ids,
                ..
            } => {
                self.nested_state(session_id, agent_id)
                    .items
                    .retain(|item| !item_ids.iter().any(|id| id == item.id()));
                Projected::pass(event)
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
            Event::AgentControl { event } => {
                let Some(agent_id) = event.agent_id.clone() else {
                    return Projected::only(Vec::new());
                };
                if agent_id == event.session_id {
                    return Projected::only(Vec::new());
                }
                let running = match event.kind.as_str() {
                    "run_started" | "run_stopping" => true,
                    "run_settled" => false,
                    _ => return Projected::only(Vec::new()),
                };
                let status = Event::ConversationStatus {
                    session_id: event.session_id.clone(),
                    agent_id,
                    running,
                    run_id: event.run_id.clone(),
                };
                let mut projected = self.project(&status);
                let include_status = projected.pass;
                projected.pass = false;
                if include_status {
                    projected.events.insert(0, status);
                }
                projected
            }
            Event::ConversationStatus {
                session_id,
                agent_id,
                running,
                run_id,
            } => {
                let duplicate = self
                    .run_status
                    .get(&(session_id.clone(), agent_id.clone()))
                    .is_some_and(|status| status.run_id == *run_id && status.running == *running);
                self.run_status.insert(
                    (session_id.clone(), agent_id.clone()),
                    RunStatus {
                        run_id: run_id.clone(),
                        running: *running,
                    },
                );
                let mut projected = if duplicate {
                    Projected::only(Vec::new())
                } else {
                    Projected::pass(event)
                };
                projected.events = self.update_root_parent_agents(
                    session_id,
                    agent_id,
                    run_id.as_deref(),
                    *running,
                );
                projected.events.extend(self.update_nested_parent_agents(
                    session_id,
                    agent_id,
                    run_id.as_deref(),
                    *running,
                ));
                projected
            }
            _ => Projected::pass(event),
        }
    }

    fn state(&mut self, session_id: &str) -> &mut ConversationState {
        self.sessions.entry(session_id.to_string()).or_default()
    }

    fn nested_state(&mut self, session_id: &str, agent_id: &str) -> &mut ConversationState {
        self.subagents
            .entry(session_id.to_string())
            .or_default()
            .entry(agent_id.to_string())
            .or_default()
    }

    fn apply_known_status_to_card(&mut self, session_id: &str, card: &mut ConversationAgentCard) {
        let key = (session_id.to_string(), card.agent_id.clone());
        let Some(status) = self.run_status.get(&key).cloned() else {
            return;
        };
        if !run_matches(card.run_id.as_deref(), status.run_id.as_deref()) {
            return;
        }
        set_agent_card_running(&mut self.finish_seq, card, status.running);
    }

    fn apply_known_status_to_item(&mut self, session_id: &str, item: &mut ConversationItem) {
        let ConversationItem::Tool { agents, .. } = item else {
            return;
        };
        for card in agents {
            self.apply_known_status_to_card(session_id, card);
        }
    }

    fn update_nested_parent_agents(
        &mut self,
        session_id: &str,
        agent_id: &str,
        run_id: Option<&str>,
        running: bool,
    ) -> Vec<Event> {
        let Some(states) = self.subagents.get_mut(session_id) else {
            return Vec::new();
        };
        let mut events = Vec::new();
        for (parent_agent_id, state) in states {
            let changed = update_conversation_agent_cards(
                &mut self.finish_seq,
                state,
                agent_id,
                run_id,
                running,
            );
            events.extend(
                changed.into_iter().map(|row| {
                    update_for_agent(session_id, Some(parent_agent_id), None, state, row)
                }),
            );
        }
        events
    }

    fn update_root_parent_agents(
        &mut self,
        session_id: &str,
        agent_id: &str,
        run_id: Option<&str>,
        running: bool,
    ) -> Vec<Event> {
        let Some(state) = self.sessions.get_mut(session_id) else {
            return Vec::new();
        };
        update_conversation_agent_cards(&mut self.finish_seq, state, agent_id, run_id, running)
            .into_iter()
            .map(|row| update(session_id, None, state, row))
            .collect()
    }
}

impl ConversationState {
    fn next_id(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        format!("{prefix}-{}", self.next_id)
    }
}

fn run_matches(card_run: Option<&str>, status_run: Option<&str>) -> bool {
    status_run.is_none() || card_run == status_run
}

fn update_conversation_agent_cards(
    finish_seq: &mut u64,
    state: &mut ConversationState,
    agent_id: &str,
    run_id: Option<&str>,
    running: bool,
) -> Vec<ConversationItem> {
    let mut changed = Vec::new();
    for row in &mut state.items {
        let ConversationItem::Tool { agents, .. } = row else {
            continue;
        };
        let mut row_changed = false;
        for card in agents
            .iter_mut()
            .filter(|card| card.agent_id == agent_id && run_matches(card.run_id.as_deref(), run_id))
        {
            row_changed |= set_agent_card_running(finish_seq, card, running);
        }
        if row_changed {
            changed.push(row.clone());
        }
    }
    changed
}

fn set_agent_card_running(
    finish_seq: &mut u64,
    card: &mut ConversationAgentCard,
    running: bool,
) -> bool {
    if running {
        if !card.finished && card.live_note.is_none() {
            return false;
        }
        card.finished = false;
        card.finished_seq = None;
        card.live_note = None;
        return true;
    }
    let changed = !card.finished || card.live_note.is_some();
    if !card.finished {
        card.finished = true;
        *finish_seq += 1;
        card.finished_seq = Some(*finish_seq);
    }
    card.live_note = None;
    changed
}

fn update(
    session_id: &str,
    agent_id: Option<&str>,
    state: &mut ConversationState,
    item: ConversationItem,
) -> Event {
    update_for_agent(session_id, agent_id, None, state, item)
}

fn update_for_agent(
    session_id: &str,
    agent_id: Option<&str>,
    run_id: Option<String>,
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
        run_id,
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
        run_id: None,
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
            run: None,
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
    fn subagent_conversation_events_are_reemitted_and_tracked() {
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
            run_id: None,
        });
        assert!(!snapshot.pass);
        assert!(matches!(
            snapshot.events.as_slice(),
            [Event::ConversationSnapshot { .. }]
        ));

        let update = projection.project(&Event::ConversationUpdate {
            session_id: "s1".into(),
            agent_id: Some("a1".into()),
            run_id: None,
            item,
        });
        assert!(!update.pass);
        assert!(matches!(
            update.events.as_slice(),
            [Event::ConversationUpdate { .. }]
        ));
    }

    #[test]
    fn nested_background_cards_follow_their_own_run() {
        let mut projection = ConversationProjection::new();
        let row = ConversationItem::Tool {
            id: "nested-call".into(),
            tool: "Agent".into(),
            summary: "delegate".into(),
            detail: "{}".into(),
            output: String::new(),
            status: ConversationToolStatus::Running,
            turn_id: None,
            edit: None,
            live_note: None,
            agents: Vec::new(),
            approval_pending: false,
        };
        projection.project(&Event::ConversationUpdate {
            session_id: "s1".into(),
            agent_id: Some("parent".into()),
            run_id: Some("parent-run".into()),
            item: row,
        });
        let added = projection.project(&Event::SubagentCard {
            session_id: "s1".into(),
            item_id: "nested-call".into(),
            agent_id: "grandchild".into(),
            run_id: Some("run-1".into()),
            profile: "coder".into(),
            description: "nested background".into(),
            model: "m".into(),
            background: true,
        });
        assert!(!added.pass);
        let Event::ConversationUpdate {
            agent_id: Some(parent),
            item: ConversationItem::Tool { agents, .. },
            ..
        } = &added.events[0]
        else {
            panic!("nested card update")
        };
        assert_eq!(parent, "parent");
        assert!(!agents[0].finished);

        let finished = projection.project(&Event::ConversationStatus {
            session_id: "s1".into(),
            agent_id: "grandchild".into(),
            running: false,
            run_id: Some("run-1".into()),
        });
        let Event::ConversationUpdate {
            item: ConversationItem::Tool { agents, .. },
            ..
        } = &finished.events[0]
        else {
            panic!("nested finish update")
        };
        assert!(agents[0].finished);

        let resumed = projection.project(&Event::SubagentCard {
            session_id: "s1".into(),
            item_id: "nested-call".into(),
            agent_id: "grandchild".into(),
            run_id: Some("run-2".into()),
            profile: "coder".into(),
            description: "nested background resumed".into(),
            model: "m".into(),
            background: true,
        });
        let Event::ConversationUpdate {
            item: ConversationItem::Tool { agents, .. },
            ..
        } = &resumed.events[0]
        else {
            panic!("nested resume card")
        };
        assert!(agents[0].finished);
        assert!(!agents[1].finished);

        let stale = projection.project(&Event::ConversationStatus {
            session_id: "s1".into(),
            agent_id: "grandchild".into(),
            running: false,
            run_id: Some("run-1".into()),
        });
        assert!(stale.events.is_empty());
        let finished = projection.project(&Event::ConversationStatus {
            session_id: "s1".into(),
            agent_id: "grandchild".into(),
            running: false,
            run_id: Some("run-2".into()),
        });
        let Event::ConversationUpdate {
            item: ConversationItem::Tool { agents, .. },
            ..
        } = &finished.events[0]
        else {
            panic!("second run finish update")
        };
        assert!(agents.iter().all(|agent| agent.finished));
    }

    #[test]
    fn a_nested_card_observes_a_terminal_status_that_arrived_first() {
        let mut projection = ConversationProjection::new();
        projection.project(&Event::ConversationStatus {
            session_id: "s1".into(),
            agent_id: "grandchild".into(),
            running: false,
            run_id: Some("run-1".into()),
        });
        projection.project(&Event::ConversationUpdate {
            session_id: "s1".into(),
            agent_id: Some("parent".into()),
            run_id: None,
            item: ConversationItem::Tool {
                id: "nested-call".into(),
                tool: "Agent".into(),
                summary: "delegate".into(),
                detail: "{}".into(),
                output: String::new(),
                status: ConversationToolStatus::Running,
                turn_id: None,
                edit: None,
                live_note: None,
                agents: Vec::new(),
                approval_pending: false,
            },
        });
        let added = projection.project(&Event::SubagentCard {
            session_id: "s1".into(),
            item_id: "nested-call".into(),
            agent_id: "grandchild".into(),
            run_id: Some("run-1".into()),
            profile: "coder".into(),
            description: "nested background".into(),
            model: "m".into(),
            background: true,
        });
        let Event::ConversationUpdate {
            item: ConversationItem::Tool { agents, .. },
            ..
        } = &added.events[0]
        else {
            panic!("late nested card update")
        };
        assert!(agents[0].finished);
        assert!(agents[0].finished_seq.is_some());
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
            run_id: None,
            profile: "coder".into(),
            description: "child".into(),
            model: "m".into(),
            background: false,
        });
        let projected = projection.project(&Event::ConversationStatus {
            session_id: "s1".into(),
            agent_id: "a1".into(),
            running: false,
            run_id: None,
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
            run_id: None,
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
                run_id: None,
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

    #[test]
    fn runtime_control_and_legacy_status_project_once() {
        let mut projection = ConversationProjection::new();
        let status = Event::ConversationStatus {
            session_id: "s1".into(),
            agent_id: "a1".into(),
            running: false,
            run_id: Some("run-1".into()),
        };
        let control = Event::AgentControl {
            event: pig_protocol::AgentControlEvent {
                session_id: "s1".into(),
                sequence: 4,
                event_id: "event-4".into(),
                kind: "run_settled".into(),
                agent_id: Some("a1".into()),
                run_id: Some("run-1".into()),
                attempt: Some(1),
                backend_epoch: Some("epoch-1".into()),
                payload: serde_json::json!({}),
                created_at_ms: 0,
            },
        };
        let projected = projection.project(&control);
        assert!(!projected.pass);
        assert_eq!(projected.events.len(), 1);
        assert!(matches!(
            projected.events[0],
            Event::ConversationStatus { .. }
        ));
        let legacy = projection.project(&status);
        assert!(!legacy.pass);
        assert!(legacy.events.is_empty());
    }

    #[test]
    fn projection_registry_deduplicates_journal_envelopes() {
        let mut registry = ProjectionRegistry::new();
        let event = Event::TaskListChanged {
            session_id: "s1".into(),
            tasks: Vec::new(),
        };
        let envelope = |sequence| EventEnvelope {
            sequence,
            session_id: Some("s1".into()),
            agent_id: None,
            run_id: None,
            epoch: "journal-test".into(),
            event: event.clone(),
        };
        assert!(registry.project(&envelope(1)).pass);
        let duplicate = registry.project(&envelope(1));
        assert!(!duplicate.pass);
        assert!(duplicate.events.is_empty());
        assert!(registry.project(&envelope(2)).pass);
    }

    #[test]
    fn projection_registry_resumes_from_a_consistent_snapshot_cut() {
        let mut registry = ProjectionRegistry::new();
        let envelope = |sequence| EventEnvelope {
            sequence,
            session_id: Some("s1".into()),
            agent_id: None,
            run_id: None,
            epoch: "journal-test".into(),
            event: Event::TaskListChanged {
                session_id: "s1".into(),
                tasks: Vec::new(),
            },
        };
        assert!(registry.project(&envelope(1)).pass);
        let snapshot = pig_core::JournalSnapshot {
            epoch: "journal-test".into(),
            cursor: 3,
            oldest: 3,
            events: vec![envelope(3)],
        };
        registry
            .reset_with_snapshot(&snapshot)
            .expect("consistent snapshot");
        let duplicate = registry.project(&envelope(3));
        assert!(!duplicate.pass);
        assert!(duplicate.resync.is_none());
        assert!(registry.project(&envelope(4)).pass);
        let gap = registry.project(&envelope(6));
        assert_eq!(gap.resync.expect("gap requires resync").cursor, 4);
        let stale = registry.project(&EventEnvelope {
            epoch: "another-epoch".into(),
            ..envelope(5)
        });
        assert!(stale.resync.is_some());
    }

    #[test]
    fn replay_completion_emits_a_full_root_conversation_snapshot() {
        let mut registry = ProjectionRegistry::new();
        let user = EventEnvelope {
            epoch: "journal-test".into(),
            sequence: 1,
            session_id: Some("s1".into()),
            agent_id: None,
            run_id: None,
            event: Event::UserMessage {
                session_id: "s1".into(),
                text: "persisted user input".into(),
                files: Vec::new(),
                image_nums: Vec::new(),
                sent_at_ms: 42,
            },
        };
        registry.project(&user);
        let ready = registry.project(&EventEnvelope {
            sequence: 2,
            event: Event::SessionReplayComplete {
                session_id: "s1".into(),
            },
            ..user.clone()
        });
        assert_eq!(ready.events.len(), 1);
        assert!(matches!(
            &ready.events[0],
            Event::ConversationSnapshot { items, running: false, .. }
                if items.len() == 1
        ));
    }
}
