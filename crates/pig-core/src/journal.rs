use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use pig_protocol::{AgentRunEnvelope, Event, EventEnvelope};

const JOURNAL_HISTORY: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JournalReplayError {
    pub cursor: u64,
    pub oldest: u64,
    pub latest: u64,
}

impl std::fmt::Display for JournalReplayError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "event cursor {} is outside the retained range {}..={}",
            self.cursor, self.oldest, self.latest
        )
    }
}

impl std::error::Error for JournalReplayError {}

/// The single ordered event journal between the core and its consumers.
/// Existing local receivers keep their raw Event stream; projections receive
/// the same facts with stable identity and sequence metadata.
#[derive(Clone)]
pub struct EventJournal {
    events: async_channel::Receiver<EventEnvelope>,
    history: Arc<Mutex<VecDeque<EventEnvelope>>>,
    next_sequence: Arc<AtomicU64>,
}

impl EventJournal {
    pub(crate) fn channel() -> (
        async_channel::Sender<Event>,
        async_channel::Receiver<Event>,
        Self,
        EventPublisher,
    ) {
        let (input, input_events) = async_channel::unbounded();
        let (raw_events, raw) = async_channel::unbounded();
        let (journal_events, events) = async_channel::unbounded();
        let journal = Self {
            events,
            history: Arc::new(Mutex::new(VecDeque::with_capacity(JOURNAL_HISTORY))),
            next_sequence: Arc::new(AtomicU64::new(0)),
        };
        let publisher = EventPublisher {
            input: input_events,
            raw: raw_events,
            journal_events,
            journal: journal.clone(),
        };
        (input, raw, journal, publisher)
    }

    pub async fn recv(&self) -> Result<EventEnvelope, async_channel::RecvError> {
        self.events.recv().await
    }

    pub fn try_recv(&self) -> Result<EventEnvelope, async_channel::TryRecvError> {
        self.events.try_recv()
    }

    pub fn replay_after(&self, cursor: u64) -> Result<Vec<EventEnvelope>, JournalReplayError> {
        let history = self
            .history
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let latest = self.next_sequence.load(Ordering::Acquire);
        let oldest = history
            .front()
            .map_or(latest.saturating_add(1), |event| event.sequence);
        if cursor > latest || cursor < oldest.saturating_sub(1) {
            return Err(JournalReplayError {
                cursor,
                oldest,
                latest,
            });
        }
        Ok(history
            .iter()
            .filter(|event| event.sequence > cursor)
            .cloned()
            .collect())
    }

    fn append(&self, event: Event) -> EventEnvelope {
        let sequence = self.next_sequence.fetch_add(1, Ordering::AcqRel) + 1;
        let (agent_id, run_id) = run_identity(&event);
        let envelope = EventEnvelope {
            sequence,
            session_id: event.session_id().map(str::to_string),
            agent_id,
            run_id,
            event,
        };
        let mut history = self
            .history
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        history.push_back(envelope.clone());
        if history.len() > JOURNAL_HISTORY {
            history.pop_front();
        }
        envelope
    }
}

pub(crate) struct EventPublisher {
    input: async_channel::Receiver<Event>,
    raw: async_channel::Sender<Event>,
    journal_events: async_channel::Sender<EventEnvelope>,
    journal: EventJournal,
}

impl EventPublisher {
    pub(crate) async fn run(self) {
        while let Ok(event) = self.input.recv().await {
            let envelope = self.journal.append(event);
            let raw_open = self.raw.send(envelope.event.clone()).await.is_ok();
            let journal_open = self.journal_events.send(envelope).await.is_ok();
            if !raw_open && !journal_open {
                break;
            }
        }
    }
}

fn run_identity(event: &Event) -> (Option<String>, Option<String>) {
    fn envelope(run: &Option<AgentRunEnvelope>) -> (Option<String>, Option<String>) {
        run.as_ref().map_or((None, None), |run| {
            (Some(run.agent_id.clone()), Some(run.run_id.clone()))
        })
    }
    match event {
        Event::TurnStarted { run, .. }
        | Event::TurnComplete { run, .. }
        | Event::TurnAborted { run, .. }
        | Event::CompactStarted { run, .. }
        | Event::ContextCompacted { run, .. } => envelope(run),
        Event::SubagentCard {
            agent_id, run_id, ..
        }
        | Event::AgentOutput {
            agent_id, run_id, ..
        }
        | Event::ConversationStatus {
            agent_id, run_id, ..
        } => (Some(agent_id.clone()), run_id.clone()),
        Event::ConversationSnapshot {
            agent_id, run_id, ..
        }
        | Event::ConversationUpdate {
            agent_id, run_id, ..
        }
        | Event::ConversationRemove {
            agent_id, run_id, ..
        } => (agent_id.clone(), run_id.clone()),
        Event::SubagentProgress { run_id, .. } => (None, run_id.clone()),
        _ => (None, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn journal_orders_events_and_replays_from_a_cursor() {
        let (input, raw, journal, publisher) = EventJournal::channel();
        let publish = tokio::spawn(publisher.run());
        input
            .send(Event::TurnStarted {
                session_id: "s1".into(),
                turn_id: "t1".into(),
                run: Some(AgentRunEnvelope {
                    agent_id: "s1".into(),
                    run_id: "run-1".into(),
                    turn_kind: pig_protocol::TurnKind::Regular,
                }),
            })
            .await
            .unwrap();
        input
            .send(Event::AgentOutput {
                session_id: "s1".into(),
                agent_id: "a1".into(),
                run_id: Some("run-2".into()),
                text: "done".into(),
            })
            .await
            .unwrap();
        drop(input);
        publish.await.unwrap();

        assert!(matches!(
            raw.recv().await.unwrap(),
            Event::TurnStarted { .. }
        ));
        assert!(matches!(
            raw.recv().await.unwrap(),
            Event::AgentOutput { .. }
        ));
        let first = journal.recv().await.unwrap();
        let second = journal.recv().await.unwrap();
        assert_eq!(first.sequence, 1);
        assert_eq!(first.agent_id.as_deref(), Some("s1"));
        assert_eq!(first.run_id.as_deref(), Some("run-1"));
        assert_eq!(second.sequence, 2);
        assert_eq!(second.agent_id.as_deref(), Some("a1"));
        assert_eq!(second.run_id.as_deref(), Some("run-2"));
        let replay = journal.replay_after(1).unwrap();
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].sequence, 2);
        assert!(journal.replay_after(2).unwrap().is_empty());
        assert!(journal.replay_after(3).is_err());
    }
}
