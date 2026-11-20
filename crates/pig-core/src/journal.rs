use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use pig_protocol::{AgentRunEnvelope, Event, EventEnvelope};
use tokio::sync::Notify;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JournalResyncReason {
    EpochMismatch,
    CursorOutOfRange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JournalResyncRequired {
    pub epoch: String,
    pub cursor: u64,
    pub oldest: u64,
    pub latest: u64,
    pub reason: JournalResyncReason,
}

impl std::fmt::Display for JournalResyncRequired {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "journal snapshot resync required for cursor {} in epoch {} ({:?})",
            self.cursor, self.epoch, self.reason
        )
    }
}

impl std::error::Error for JournalResyncRequired {}

/// The single ordered event journal between the core and its consumers.
/// Existing local receivers keep their raw Event stream; projections receive
/// the same facts with stable identity and sequence metadata.
#[derive(Clone, Debug)]
pub struct JournalSnapshot {
    pub epoch: String,
    pub cursor: u64,
    pub oldest: u64,
    pub events: Vec<EventEnvelope>,
}

#[derive(Default)]
struct JournalState {
    history: VecDeque<EventEnvelope>,
    latest: u64,
    read_cursor: u64,
    closed: bool,
}

#[derive(Clone)]
pub struct EventJournal {
    state: Arc<Mutex<JournalState>>,
    notify: Arc<Notify>,
    active_reader: Arc<AtomicBool>,
    epoch: String,
}

impl EventJournal {
    pub(crate) fn channel() -> (
        async_channel::Sender<Event>,
        async_channel::Receiver<Event>,
        Self,
        EventPublisher,
    ) {
        let (input, input_events) = async_channel::bounded(JOURNAL_HISTORY);
        let (raw_events, raw) = async_channel::bounded(JOURNAL_HISTORY);
        let journal = Self {
            state: Arc::new(Mutex::new(JournalState {
                history: VecDeque::with_capacity(JOURNAL_HISTORY),
                latest: 0,
                read_cursor: 0,
                closed: false,
            })),
            notify: Arc::new(Notify::new()),
            active_reader: Arc::new(AtomicBool::new(false)),
            epoch: crate::agent::runtime::new_runtime_id("journal"),
        };
        let publisher = EventPublisher {
            input: input_events,
            raw: raw_events,
            journal: journal.clone(),
        };
        (input, raw, journal, publisher)
    }

    pub fn capacity(&self) -> usize {
        JOURNAL_HISTORY
    }

    fn has_active_reader(&self) -> bool {
        self.active_reader.load(Ordering::Acquire)
    }

    fn take_next_locked(state: &mut JournalState) -> Result<Option<EventEnvelope>, ()> {
        let next = state
            .history
            .iter()
            .find(|event| event.sequence > state.read_cursor)
            .cloned();
        if let Some(envelope) = next {
            state.read_cursor = envelope.sequence;
            return Ok(Some(envelope));
        }
        if state.closed {
            return Err(());
        }
        Ok(None)
    }

    pub async fn recv(&self) -> Result<EventEnvelope, async_channel::RecvError> {
        self.active_reader.store(true, Ordering::Release);
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                match Self::take_next_locked(&mut state) {
                    Ok(Some(envelope)) => return Ok(envelope),
                    Ok(None) => {
                        // Register the waiter before releasing the lock so a concurrent append
                        // or close cannot miss this notification.
                        notified.as_mut().enable();
                    }
                    Err(()) => return Err(async_channel::RecvError),
                }
            }
            notified.await;
        }
    }

    pub fn try_recv(&self) -> Result<EventEnvelope, async_channel::TryRecvError> {
        self.active_reader.store(true, Ordering::Release);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match Self::take_next_locked(&mut state) {
            Ok(Some(envelope)) => Ok(envelope),
            Ok(None) => Err(async_channel::TryRecvError::Empty),
            Err(()) => Err(async_channel::TryRecvError::Closed),
        }
    }

    pub fn epoch(&self) -> &str {
        &self.epoch
    }

    pub fn replay_after_epoch(
        &self,
        epoch: &str,
        cursor: u64,
    ) -> Result<Vec<EventEnvelope>, JournalResyncRequired> {
        self.replay_or_snapshot(epoch, cursor)
            .map_err(|snapshot| JournalResyncRequired {
                epoch: snapshot.epoch,
                cursor,
                oldest: snapshot.oldest,
                latest: snapshot.cursor,
                reason: if epoch == self.epoch {
                    JournalResyncReason::CursorOutOfRange
                } else {
                    JournalResyncReason::EpochMismatch
                },
            })
    }

    pub fn replay_or_snapshot(
        &self,
        epoch: &str,
        cursor: u64,
    ) -> Result<Vec<EventEnvelope>, JournalSnapshot> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let oldest = state
            .history
            .front()
            .map_or(state.latest.saturating_add(1), |event| event.sequence);
        if epoch != self.epoch || cursor > state.latest || cursor < oldest.saturating_sub(1) {
            return Err(self.snapshot_locked(&state));
        }
        Ok(state
            .history
            .iter()
            .filter(|event| event.sequence > cursor)
            .cloned()
            .collect())
    }

    pub fn snapshot(&self) -> JournalSnapshot {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.snapshot_locked(&state)
    }

    fn snapshot_locked(&self, state: &JournalState) -> JournalSnapshot {
        JournalSnapshot {
            epoch: self.epoch.clone(),
            cursor: state.latest,
            oldest: state
                .history
                .front()
                .map_or(state.latest.saturating_add(1), |event| event.sequence),
            events: state.history.iter().cloned().collect(),
        }
    }

    pub fn replay_after(&self, cursor: u64) -> Result<Vec<EventEnvelope>, JournalReplayError> {
        self.replay_or_snapshot(&self.epoch, cursor)
            .map_err(|snapshot| JournalReplayError {
                cursor,
                oldest: snapshot.oldest,
                latest: snapshot.cursor,
            })
    }

    fn append(&self, event: Event) -> EventEnvelope {
        let (agent_id, run_id) = run_identity(&event);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let sequence = state.latest + 1;
        let envelope = EventEnvelope {
            epoch: self.epoch.clone(),
            sequence,
            session_id: event.session_id().map(str::to_string),
            agent_id,
            run_id,
            event,
        };
        state.latest = sequence;
        state.history.push_back(envelope.clone());
        if state.history.len() > JOURNAL_HISTORY {
            state.history.pop_front();
        }
        drop(state);
        self.notify.notify_waiters();
        envelope
    }

    fn close(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.closed = true;
        drop(state);
        self.notify.notify_waiters();
    }

    fn has_external_handles(&self) -> bool {
        Arc::strong_count(&self.state) > 1
    }
}

pub(crate) struct EventPublisher {
    input: async_channel::Receiver<Event>,
    raw: async_channel::Sender<Event>,
    journal: EventJournal,
}

impl EventPublisher {
    pub(crate) async fn run(self) {
        while let Ok(event) = self.input.recv().await {
            let envelope = self.journal.append(event);
            let raw_open = if self.raw.is_closed() {
                false
            } else if self.journal.has_active_reader() {
                // When a journal consumer is active, an unread raw receiver must never
                // backpressure or deadlock event publication.
                !matches!(
                    self.raw.try_send(envelope.event),
                    Err(async_channel::TrySendError::Closed(_))
                )
            } else {
                self.raw.send(envelope.event).await.is_ok()
            };
            if !raw_open && !self.journal.has_external_handles() {
                break;
            }
        }
        self.journal.close();
    }
}

fn run_identity(event: &Event) -> (Option<String>, Option<String>) {
    fn envelope(run: &Option<AgentRunEnvelope>) -> (Option<String>, Option<String>) {
        run.as_ref().map_or((None, None), |run| {
            (Some(run.agent_id.clone()), Some(run.run_id.clone()))
        })
    }
    match event {
        Event::AgentControl { event } => (event.agent_id.clone(), event.run_id.clone()),
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

    fn drain_channels(
        raw: async_channel::Receiver<Event>,
        journal: &EventJournal,
    ) -> (tokio::task::JoinHandle<()>, tokio::task::JoinHandle<()>) {
        let raw = tokio::spawn(async move { while raw.recv().await.is_ok() {} });
        let journal = journal.clone();
        let events = tokio::spawn(async move { while journal.recv().await.is_ok() {} });
        (raw, events)
    }

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
        assert_eq!(replay[0].epoch, journal.epoch());
        assert!(journal.replay_after(2).unwrap().is_empty());
        assert!(journal.replay_after(3).is_err());
        assert_eq!(
            journal
                .replay_after_epoch(journal.epoch(), 1)
                .expect("same-epoch replay")
                .len(),
            1
        );
        let stale = journal
            .replay_after_epoch("old-epoch", 1)
            .expect_err("stale epoch");
        assert_eq!(stale.reason, JournalResyncReason::EpochMismatch);
    }

    #[tokio::test]
    async fn journal_bounds_its_replay_window_and_requires_resync() {
        let (input, raw, journal, publisher) = EventJournal::channel();
        let publish = tokio::spawn(publisher.run());
        let drains = drain_channels(raw, &journal);
        for _ in 0..1_100 {
            input
                .send(Event::SessionList {
                    sessions: Vec::new(),
                })
                .await
                .expect("publish event");
        }
        drop(input);
        publish.await.expect("publisher");
        drains.0.await.expect("raw drain");
        drains.1.await.expect("journal drain");

        let latest = 1_100;
        let oldest_retained_cursor = latest - JOURNAL_HISTORY as u64;
        let replay = journal
            .replay_after(oldest_retained_cursor)
            .expect("retained replay");
        assert_eq!(replay.len(), JOURNAL_HISTORY);
        assert_eq!(replay[0].sequence, oldest_retained_cursor + 1);
        assert!(journal.replay_after(0).is_err());
        assert!(
            journal
                .replay_after(latest)
                .expect("current cursor")
                .is_empty()
        );
        let stale = journal
            .replay_after_epoch("old-epoch", oldest_retained_cursor)
            .expect_err("stale epoch");
        assert_eq!(stale.reason, JournalResyncReason::EpochMismatch);
        assert_eq!(stale.oldest, oldest_retained_cursor + 1);
        assert_eq!(stale.latest, latest);
    }

    #[tokio::test]
    async fn snapshots_are_consistent_under_publication_and_replace_expired_cursors() {
        let (input, raw, journal, publisher) = EventJournal::channel();
        let publish = tokio::spawn(publisher.run());
        let drains = drain_channels(raw, &journal);
        for _ in 0..1_100 {
            input
                .send(Event::SessionList {
                    sessions: Vec::new(),
                })
                .await
                .expect("publish event");
            let snapshot = journal.snapshot();
            assert_eq!(
                snapshot.cursor,
                snapshot.events.last().map_or(0, |event| event.sequence)
            );
        }
        drop(input);
        publish.await.expect("publisher");
        drains.0.await.expect("raw drain");
        drains.1.await.expect("journal drain");

        let snapshot = journal
            .replay_or_snapshot(journal.epoch(), 0)
            .expect_err("expired cursor needs a snapshot");
        assert_eq!(snapshot.cursor, 1_100);
        assert_eq!(snapshot.events.len(), JOURNAL_HISTORY);
        assert_eq!(snapshot.oldest, snapshot.events[0].sequence);
        assert_eq!(
            snapshot.events.last().map(|event| event.sequence),
            Some(snapshot.cursor)
        );
        let current = journal.snapshot();
        assert_eq!(current.cursor, snapshot.cursor);
        assert_eq!(current.epoch, snapshot.epoch);
        let replay = journal
            .replay_or_snapshot(journal.epoch(), snapshot.cursor)
            .expect("current cursor");
        assert!(replay.is_empty());
        let stale = journal
            .replay_or_snapshot("old-epoch", snapshot.cursor)
            .expect_err("stale epoch needs a snapshot");
        assert_eq!(stale.cursor, snapshot.cursor);
        assert_eq!(stale.epoch, snapshot.epoch);
    }
}

#[cfg(test)]
mod capacity_tests {
    use super::*;

    #[test]
    fn journal_channels_have_bounded_capacity() {
        let (input, raw, journal, _publisher) = EventJournal::channel();
        assert_eq!(input.capacity(), Some(JOURNAL_HISTORY));
        assert_eq!(raw.capacity(), Some(JOURNAL_HISTORY));
        assert_eq!(journal.capacity(), JOURNAL_HISTORY);
    }

    #[tokio::test]
    async fn journal_consumer_does_not_deadlock_when_raw_channel_is_unread() {
        let (input, _unread_raw, journal, publisher) = EventJournal::channel();
        let publish = tokio::spawn(publisher.run());
        let consumer = journal.clone();
        let drain = tokio::spawn(async move {
            let mut count = 0usize;
            while consumer.recv().await.is_ok() {
                count += 1;
            }
            count
        });
        for _ in 0..(JOURNAL_HISTORY + 500) {
            input
                .send(Event::SessionList {
                    sessions: Vec::new(),
                })
                .await
                .expect("publish event without deadlock");
        }
        drop(input);
        tokio::time::timeout(std::time::Duration::from_secs(5), publish)
            .await
            .expect("publisher must not deadlock on unread raw channel")
            .expect("publisher task");
        let received = tokio::time::timeout(std::time::Duration::from_secs(5), drain)
            .await
            .expect("journal consumer must finish")
            .expect("journal consumer task");
        assert!(received > 0);
        assert_eq!(journal.snapshot().cursor, (JOURNAL_HISTORY + 500) as u64);
    }

    #[tokio::test]
    async fn raw_consumer_does_not_deadlock_when_journal_is_unread() {
        let (input, raw, journal, publisher) = EventJournal::channel();
        let publish = tokio::spawn(publisher.run());
        let drain = tokio::spawn(async move {
            let mut count = 0usize;
            while raw.recv().await.is_ok() {
                count += 1;
            }
            count
        });
        for _ in 0..(JOURNAL_HISTORY + 500) {
            input
                .send(Event::SessionList {
                    sessions: Vec::new(),
                })
                .await
                .expect("publish event without deadlock");
        }
        drop(input);
        tokio::time::timeout(std::time::Duration::from_secs(5), publish)
            .await
            .expect("publisher must not deadlock on unread journal")
            .expect("publisher task");
        let received = tokio::time::timeout(std::time::Duration::from_secs(5), drain)
            .await
            .expect("raw consumer must finish")
            .expect("raw consumer task");
        assert_eq!(received, JOURNAL_HISTORY + 500);
        assert_eq!(journal.snapshot().cursor, (JOURNAL_HISTORY + 500) as u64);
    }
}
