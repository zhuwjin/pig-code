use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use futures_util::stream::FuturesUnordered;
use pig_protocol::{CoreError, Event};
use pig_provider::ResolvedModel;
use tokio_util::sync::CancellationToken;

use crate::session::{PendingMailbox, PendingMessage, persist_pending};
use crate::store::Store;

use super::entry::{SessionEntry, TurnFuture, apply_pending_plan, resolve_entry};
use super::{Manager, emit};

/// Steer cap aligned with ZCode (MAX_TURN_STEER_INPUT_BYTES): larger text
/// falls back to the future-turn lane
const MAX_STEER_BYTES: usize = 200_000;

/// Queue-lane snapshot for Event::QueueChanged (filters the shared list; held
/// items belong to the confirmation prompt and never appear as queue rows)
pub(super) fn queue_snapshot(pending: &PendingMailbox) -> Vec<pig_protocol::QueueItem> {
    pending
        .lock()
        .expect("pending lock")
        .iter()
        .filter(|message| !message.steer && !message.held)
        .map(queue_item)
        .collect()
}

/// Steer-lane snapshot for Event::SteerChanged (filters the shared list)
pub(super) fn steer_snapshot(pending: &PendingMailbox) -> Vec<pig_protocol::QueueItem> {
    pending
        .lock()
        .expect("pending lock")
        .iter()
        .filter(|message| message.steer)
        .map(queue_item)
        .collect()
}

fn queue_item(message: &PendingMessage) -> pig_protocol::QueueItem {
    pig_protocol::QueueItem {
        id: message.id.clone(),
        text: message.text.clone(),
        permissions: message.permissions,
        images: message.images.clone(),
    }
}

/// Mint the next queue id ("q1", "q2", ...; per session, shared by both lanes)
pub(super) fn next_queue_id(seq: &mut u64) -> String {
    *seq += 1;
    format!("q{seq}")
}

/// Number of future-turn queue items (held items and steer items excluded)
pub(super) fn queue_len(entry: &SessionEntry) -> usize {
    entry
        .pending
        .lock()
        .expect("pending lock")
        .iter()
        .filter(|message| !message.steer && !message.held)
        .count()
}

/// The first held item's prompt payload: (text, queue lane snapshot)
fn held_prompt(entry: &SessionEntry) -> Option<(String, Vec<pig_protocol::QueueItem>)> {
    let pending = entry.pending.lock().expect("pending lock");
    let held = pending.iter().find(|message| message.held)?;
    Some((
        held.text.clone(),
        pending
            .iter()
            .filter(|message| !message.steer && !message.held)
            .map(queue_item)
            .collect(),
    ))
}

/// Insert into the future-turn lane ahead of every other queue item (the
/// held-at-tail invariant is preserved for held messages).
fn insert_queue_front(pending: &mut VecDeque<PendingMessage>, item: PendingMessage) {
    let at = pending
        .iter()
        .position(|message| !message.steer)
        .unwrap_or(pending.len());
    pending.insert(at, item);
}

/// Append to the future-turn lane, before any held items (held stay at the
/// tail so no drain can pop one during the confirmation flow).
pub(super) fn push_queue_back(pending: &mut VecDeque<PendingMessage>, item: PendingMessage) {
    let at = pending
        .iter()
        .position(|message| message.held)
        .unwrap_or(pending.len());
    pending.insert(at, item);
}

/// Pop the next real user message. Task notifications have their own mailbox
/// and never become queue items. Held items require their explicit disposition.
fn pop_queue_head(entry: &mut SessionEntry) -> Option<PendingMessage> {
    let mut pending = entry.pending.lock().expect("pending lock");
    let index = pending
        .iter()
        .position(|message| !message.steer && !message.held)?;
    pending.remove(index)
}

/// Start a turn from one admitted message. Fallible by construction: the
/// session must be idle (a stale held-queue resolution is the realistic race),
/// and the message is returned intact on failure so the caller can persist it
/// again instead of losing it.
pub(super) fn start_turn(
    entry: &mut SessionEntry,
    session_id: String,
    message: PendingMessage,
    config: &ResolvedModel,
    event_tx: &async_channel::Sender<Event>,
    turns: &mut FuturesUnordered<TurnFuture>,
) -> Result<(), PendingMessage> {
    start_turn_inner(entry, session_id, message, config, false, event_tx, turns)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn start_turn_inner(
    entry: &mut SessionEntry,
    session_id: String,
    message: PendingMessage,
    config: &ResolvedModel,
    notification_only: bool,
    event_tx: &async_channel::Sender<Event>,
    turns: &mut FuturesUnordered<TurnFuture>,
) -> Result<(), PendingMessage> {
    let Some(mut session) = entry.session.take() else {
        return Err(message);
    };
    // Enforce the single-source invariant before any resolution: the Session
    // and the entry share one `Arc<RuntimeConfig>`, so a refresh that happened
    // mid-turn is installed here (a trust change still goes through the one
    // `apply_trust` boundary inside `set_runtime`).
    session.set_runtime(entry.runtime.clone());
    apply_pending_plan(entry, &mut session, event_tx);
    session.set_permissions(message.permissions);
    if let Some(selection) = &entry.model_override {
        session.set_model(selection.clone());
    }
    let cancel = CancellationToken::new();
    let control = pig_provider::CallControl::new(cancel.clone());
    entry.cancel = Some(cancel);
    entry.retry = Some(control.clone());
    entry.steerable = !notification_only;
    let pending = entry.pending.clone();
    let notifications = entry.notifications.clone();
    let tx = event_tx.clone();
    let config = config.clone();
    turns.push(Box::pin(async move {
        let outcome = session
            .run_turn(
                message.text,
                message.files,
                message.images,
                &config,
                &tx,
                control,
                pending,
                notifications,
                notification_only,
            )
            .await;
        (session_id, session, outcome)
    }));
    Ok(())
}

/// Start the queue-lane head as the next turn. The model is resolved before
/// anything is removed: a resolution failure keeps every item persisted and
/// emits a typed nonfatal error. Returns whether a turn started.
pub(super) fn start_queued_head(
    entry: &mut SessionEntry,
    session_id: &str,
    event_tx: &async_channel::Sender<Event>,
    turns: &mut FuturesUnordered<TurnFuture>,
    store: &Arc<Mutex<Store>>,
) -> bool {
    if queue_len(entry) == 0 || entry.session.is_none() {
        persist_pending(store, session_id, &entry.pending);
        return false;
    }
    let resolved = match resolve_entry(entry) {
        Ok(resolved) => resolved,
        Err(error) => {
            let _ = event_tx.send_blocking(Event::Error {
                session_id: Some(session_id.to_string()),
                error,
                fatal: false,
            });
            persist_pending(store, session_id, &entry.pending);
            return false;
        }
    };
    let Some(item) = pop_queue_head(entry) else {
        persist_pending(store, session_id, &entry.pending);
        return false;
    };
    let items = queue_snapshot(&entry.pending);
    let _ = event_tx.send_blocking(Event::QueueChanged {
        session_id: session_id.to_string(),
        items,
    });
    persist_pending(store, session_id, &entry.pending);
    if let Err(message) = start_turn(
        entry,
        session_id.to_string(),
        item,
        &resolved,
        event_tx,
        turns,
    ) {
        insert_queue_front(&mut entry.pending.lock().expect("pending lock"), message);
        persist_pending(store, session_id, &entry.pending);
        let _ = event_tx.send_blocking(Event::Error {
            session_id: Some(session_id.to_string()),
            error: CoreError::Internal {
                detail: "The session was busy; the queued message stays queued.".to_string(),
            },
            fatal: false,
        });
        return false;
    }
    true
}

/// Stop every active task of a session through the existing task lifecycle
/// (token cancellation for subagents, process-tree kill for shells). Used by
/// session deletion so a removed session leaves no background executor behind.
pub(super) fn stop_session_tasks(entry: &SessionEntry) {
    let running: Vec<String> = entry
        .state
        .tasks
        .lock()
        .expect("task registry lock")
        .iter()
        .filter(|task| {
            matches!(
                task.status,
                pig_protocol::TaskStatus::Queued | pig_protocol::TaskStatus::Running
            )
        })
        .map(|task| task.id.clone())
        .collect();
    for id in running {
        let _ = crate::task::stop_task(
            &entry.state.tasks,
            &id,
            &entry.state.task_notify,
            &entry.state.session_id,
        );
    }
}

impl Manager {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn send_message(
        &mut self,
        event_tx: async_channel::Sender<Event>,
        session_id: String,
        content: String,
        files: Vec<String>,
        images: Vec<pig_protocol::PendingImage>,
        permissions: pig_protocol::PermissionSettings,
        delivery: pig_protocol::MessageDelivery,
        turns: &mut FuturesUnordered<TurnFuture>,
    ) {
        let store = self.store.clone();
        let sessions = &mut self.sessions;
        let Some(entry) = sessions.get_mut(&session_id) else {
            emit(
                &event_tx,
                Event::Error {
                    session_id: Some(session_id),
                    error: CoreError::SessionNotFoundOpen,
                    fatal: false,
                },
            );
            return;
        };
        // Record the latest permissions: notification-only turns use them (queued case included)
        entry.last_permissions = permissions;
        let id = next_queue_id(&mut entry.queue_seq);
        // Turn in progress -> two lanes of one list. Steer
        // (explicit request, steerable chat turn in flight,
        // text-only, within the cap) injects into the running
        // turn at the next safe boundary; everything else runs
        // as the next turn (Interrupt does not clear the queue)
        if entry.session.is_none() {
            let steer = delivery == pig_protocol::MessageDelivery::Steer
                && entry.steerable
                && images.is_empty()
                && content.len() <= MAX_STEER_BYTES;
            entry
                .pending
                .lock()
                .expect("pending lock")
                .push_back(PendingMessage {
                    id,
                    text: content,
                    files,
                    images,
                    permissions,
                    steer,
                    held: false,
                });
            let queue_items = queue_snapshot(&entry.pending);
            emit(
                &event_tx,
                Event::QueueChanged {
                    session_id: session_id.clone(),
                    items: queue_items,
                },
            );
            if steer {
                let steer_items = steer_snapshot(&entry.pending);
                emit(
                    &event_tx,
                    Event::SteerChanged {
                        session_id: session_id.clone(),
                        items: steer_items,
                    },
                );
            }
            persist_pending(&store, &session_id, &entry.pending);
            return;
        }
        let mut message = PendingMessage {
            id,
            text: content,
            files,
            images,
            permissions,
            steer: false,
            held: false,
        };
        // Paused queue + a new send: hold the message for the
        // confirmation flow (ZCode's held-queue disposition) —
        // silently queueing past a held backlog would be
        // surprising. Held messages share the pending list, so a
        // second send appends another instead of overwriting the
        // first.
        if !entry.auto_drain && queue_len(entry) > 0 {
            message.held = true;
            entry
                .pending
                .lock()
                .expect("pending lock")
                .push_back(message);
            persist_pending(&store, &session_id, &entry.pending);
            if let Some((text, items)) = held_prompt(entry) {
                emit(
                    &event_tx,
                    Event::HeldQueuePrompt {
                        session_id: session_id.clone(),
                        text,
                        items,
                    },
                );
            }
            return;
        }
        match resolve_entry(entry) {
            Ok(resolved) => {
                if let Err(message) = start_turn(
                    entry,
                    session_id.clone(),
                    message,
                    &resolved,
                    &event_tx,
                    turns,
                ) {
                    // The message is never lost: it stays queued
                    // and persisted for the next idle boundary.
                    push_queue_back(&mut entry.pending.lock().expect("pending lock"), message);
                    let items = queue_snapshot(&entry.pending);
                    emit(
                        &event_tx,
                        Event::QueueChanged {
                            session_id: session_id.clone(),
                            items,
                        },
                    );
                    persist_pending(&store, &session_id, &entry.pending);
                    emit(
                        &event_tx,
                        Event::Error {
                            session_id: Some(session_id),
                            error: CoreError::Internal {
                                detail: "The session was busy; the message was queued instead."
                                    .to_string(),
                            },
                            fatal: false,
                        },
                    );
                }
            }
            Err(error) => {
                // No usable model: keep the message persisted
                // (previously it was dropped) and report a
                // typed, nonfatal error.
                push_queue_back(&mut entry.pending.lock().expect("pending lock"), message);
                let items = queue_snapshot(&entry.pending);
                emit(
                    &event_tx,
                    Event::QueueChanged {
                        session_id: session_id.clone(),
                        items,
                    },
                );
                persist_pending(&store, &session_id, &entry.pending);
                emit(
                    &event_tx,
                    Event::Error {
                        session_id: Some(session_id),
                        error,
                        fatal: false,
                    },
                );
            }
        }
    }
    pub(super) fn cancel_queued(
        &mut self,
        event_tx: async_channel::Sender<Event>,
        session_id: String,
        id: String,
    ) {
        let store = self.store.clone();
        let sessions = &mut self.sessions;
        // Both lanes are addressable by the same id space; held
        // items are not user-visible rows and are never canceled
        // through this op.
        if let Some(entry) = sessions.get_mut(&session_id) {
            let mut pending = entry.pending.lock().expect("pending lock");
            let lane = pending
                .iter()
                .find(|m| m.id == id && !m.held)
                .map(|m| m.steer);
            if let Some(_lane) = lane {
                pending.retain(|m| m.id != id);
            }
            drop(pending);
            if lane == Some(false) {
                let items = queue_snapshot(&entry.pending);
                emit(
                    &event_tx,
                    Event::QueueChanged {
                        session_id: session_id.clone(),
                        items,
                    },
                );
            } else if lane == Some(true) {
                let items = steer_snapshot(&entry.pending);
                emit(
                    &event_tx,
                    Event::SteerChanged {
                        session_id: session_id.clone(),
                        items,
                    },
                );
            }
            if lane.is_some() {
                persist_pending(&store, &session_id, &entry.pending);
            }
            // Deleting the last queued item clears a now
            // meaningless pause (nothing left to resume). A
            // held message still awaiting its disposition keeps
            // the pause: its prompt was raised against this
            // backlog.
            let held_waiting = entry
                .pending
                .lock()
                .expect("pending lock")
                .iter()
                .any(|m| m.held);
            if !entry.auto_drain && queue_len(entry) == 0 && !held_waiting {
                entry.auto_drain = true;
                emit(
                    &event_tx,
                    Event::QueueDrainChanged {
                        session_id: session_id.clone(),
                        auto_drain: true,
                        reason: None,
                    },
                );
            }
        }
    }
    pub(super) fn reorder_queued(
        &mut self,
        event_tx: async_channel::Sender<Event>,
        session_id: String,
        id: String,
        before_id: Option<String>,
    ) {
        let store = self.store.clone();
        let sessions = &mut self.sessions;
        // Reorder only within the dragged item's current lane. A missing or
        // cross-lane anchor degrades to that lane's tail.
        if let Some(entry) = sessions.get_mut(&session_id) {
            let mut pending = entry.pending.lock().expect("pending lock");
            let Some((pos, steer)) = pending
                .iter()
                .enumerate()
                .find(|(_, message)| message.id == id && !message.held)
                .map(|(pos, message)| (pos, message.steer))
            else {
                return;
            };
            let item = pending.remove(pos).expect("position checked");
            let target = before_id.as_deref().and_then(|anchor| {
                pending
                    .iter()
                    .position(|message| message.id == anchor && message.steer == steer)
            });
            match (target, steer) {
                (Some(at), _) => pending.insert(at, item),
                (None, false) => push_queue_back(&mut pending, item),
                (None, true) => {
                    let at = pending
                        .iter()
                        .rposition(|message| message.steer)
                        .map_or(0, |pos| pos + 1);
                    pending.insert(at, item);
                }
            }
            drop(pending);
            if steer {
                let items = steer_snapshot(&entry.pending);
                emit(
                    &event_tx,
                    Event::SteerChanged {
                        session_id: session_id.clone(),
                        items,
                    },
                );
            } else {
                let items = queue_snapshot(&entry.pending);
                emit(
                    &event_tx,
                    Event::QueueChanged {
                        session_id: session_id.clone(),
                        items,
                    },
                );
            }
            persist_pending(&store, &session_id, &entry.pending);
        }
    }

    pub(super) fn move_queued_lane(
        &mut self,
        event_tx: async_channel::Sender<Event>,
        session_id: String,
        id: String,
        steer: bool,
    ) {
        let store = self.store.clone();
        let sessions = &mut self.sessions;
        let Some(entry) = sessions.get_mut(&session_id) else {
            return;
        };
        let mut pending = entry.pending.lock().expect("pending lock");
        let Some(pos) = pending
            .iter()
            .position(|message| message.id == id && !message.held)
        else {
            return;
        };
        let message = &pending[pos];
        if message.steer == steer {
            return;
        }
        if steer
            && (entry.session.is_some()
                || !entry.steerable
                || !message.images.is_empty()
                || message.text.len() > MAX_STEER_BYTES)
        {
            return;
        }
        pending[pos].steer = steer;
        drop(pending);
        let queue_items = queue_snapshot(&entry.pending);
        emit(
            &event_tx,
            Event::QueueChanged {
                session_id: session_id.clone(),
                items: queue_items,
            },
        );
        let steer_items = steer_snapshot(&entry.pending);
        emit(
            &event_tx,
            Event::SteerChanged {
                session_id: session_id.clone(),
                items: steer_items,
            },
        );
        persist_pending(&store, &session_id, &entry.pending);
    }
    pub(super) fn send_queued_now(
        &mut self,
        event_tx: async_channel::Sender<Event>,
        session_id: String,
        id: String,
        turns: &mut FuturesUnordered<TurnFuture>,
    ) {
        let store = self.store.clone();
        let sessions = &mut self.sessions;
        let Some(entry) = sessions.get_mut(&session_id) else {
            emit(
                &event_tx,
                Event::Error {
                    session_id: Some(session_id),
                    error: CoreError::SessionNotFound,
                    fatal: false,
                },
            );
            return;
        };
        // Validate the target before touching the drain state:
        // a stale/cancelled row must neither unpause the queue
        // nor start anything.
        let target_exists = entry
            .pending
            .lock()
            .expect("pending lock")
            .iter()
            .any(|m| m.id == id && !m.held);
        if !target_exists {
            emit(
                &event_tx,
                Event::Error {
                    session_id: Some(session_id),
                    error: CoreError::Internal {
                        detail:
                            "The queued message is no longer pending; the queue was left unchanged."
                                .to_string(),
                    },
                    fatal: false,
                },
            );
            return;
        }
        // An explicit send-now on a paused queue is the user's
        // resume: re-enable the drain (the wrap-up after this
        // turn continues with the rest) and clear the banner.
        if !entry.auto_drain {
            entry.auto_drain = true;
            emit(
                &event_tx,
                Event::QueueDrainChanged {
                    session_id: session_id.clone(),
                    auto_drain: true,
                    reason: None,
                },
            );
        }
        if entry.session.is_none() {
            // ZCode send-now semantics: cancel the running turn;
            // the wrap-up starts the (now lane-front) head as a
            // fresh turn. Insert before the first queue-lane item.
            let mut pending = entry.pending.lock().expect("pending lock");
            let Some(pos) = pending.iter().position(|m| m.id == id && !m.held) else {
                return;
            };
            let mut item = pending.remove(pos).expect("position checked");
            item.steer = false;
            insert_queue_front(&mut pending, item);
            drop(pending);
            // Internal promotion cancel: the wrap-up keeps
            // draining (a user stop would pause instead)
            entry.cancel_promote = true;
            if let Some(cancel) = &entry.cancel {
                cancel.cancel();
            }
        } else {
            // Idle: resolve the model before removing the item.
            // A failure keeps it at its current position and
            // leaves the persisted queue intact.
            match resolve_entry(entry) {
                Ok(resolved) => {
                    let item = {
                        let mut pending = entry.pending.lock().expect("pending lock");
                        let Some(pos) = pending.iter().position(|m| m.id == id && !m.held) else {
                            return;
                        };
                        let mut item = pending.remove(pos).expect("position checked");
                        item.steer = false;
                        item
                    };
                    if let Err(message) =
                        start_turn(entry, session_id.clone(), item, &resolved, &event_tx, turns)
                    {
                        insert_queue_front(
                            &mut entry.pending.lock().expect("pending lock"),
                            message,
                        );
                        emit(
                            &event_tx,
                            Event::Error {
                                session_id: Some(session_id.clone()),
                                error: CoreError::Internal {
                                    detail:
                                        "The session was busy; the queued message stays queued."
                                            .to_string(),
                                },
                                fatal: false,
                            },
                        );
                    }
                }
                Err(error) => {
                    emit(
                        &event_tx,
                        Event::Error {
                            session_id: Some(session_id.clone()),
                            error,
                            fatal: false,
                        },
                    );
                }
            }
        }
        let items = queue_snapshot(&entry.pending);
        emit(
            &event_tx,
            Event::QueueChanged {
                session_id: session_id.clone(),
                items,
            },
        );
        let items = steer_snapshot(&entry.pending);
        emit(
            &event_tx,
            Event::SteerChanged {
                session_id: session_id.clone(),
                items,
            },
        );
        persist_pending(&store, &session_id, &entry.pending);
    }
    pub(super) fn resume_queue(
        &mut self,
        event_tx: async_channel::Sender<Event>,
        session_id: String,
        turns: &mut FuturesUnordered<TurnFuture>,
    ) {
        self.retry_terminal_notifications(&event_tx);
        let store = self.store.clone();
        let sessions = &mut self.sessions;
        let Some(entry) = sessions.get_mut(&session_id) else {
            emit(
                &event_tx,
                Event::Error {
                    session_id: Some(session_id),
                    error: CoreError::SessionNotFound,
                    fatal: false,
                },
            );
            return;
        };
        entry.auto_drain = true;
        emit(
            &event_tx,
            Event::QueueDrainChanged {
                session_id: session_id.clone(),
                auto_drain: true,
                reason: None,
            },
        );
        // Idle: the head starts right away (the same drain the
        // wrap-up performs). The model is resolved before the
        // pop; a failure keeps the queue persisted.
        if entry.session.is_some()
            && !start_queued_head(entry, &session_id, &event_tx, turns, &store)
        {
            super::notifications::start_notification_turn(
                entry,
                &session_id,
                &event_tx,
                turns,
                &store,
            );
        }
    }
    pub(super) fn resolve_held_queue(
        &mut self,
        event_tx: async_channel::Sender<Event>,
        session_id: String,
        disposition: pig_protocol::HeldDisposition,
        turns: &mut FuturesUnordered<TurnFuture>,
    ) {
        let store = self.store.clone();
        let sessions = &mut self.sessions;
        let Some(entry) = sessions.get_mut(&session_id) else {
            return;
        };
        let held = {
            let mut pending = entry.pending.lock().expect("pending lock");
            let pos = pending.iter().position(|message| message.held);
            pos.and_then(|pos| pending.remove(pos))
        };
        let Some(mut held) = held else {
            // Stale/duplicate prompt (already resolved): close it
            // idempotently without touching the queue.
            emit(&event_tx, Event::HeldQueueResolved { session_id });
            return;
        };
        match disposition {
            pig_protocol::HeldDisposition::ClearQueueAndSend => {
                // Drop the future-turn backlog, keep steer items
                // and other held messages, then run the held
                // message now when the session is idle.
                entry
                    .pending
                    .lock()
                    .expect("pending lock")
                    .retain(|message| message.steer || message.held);
                held.held = false;
                if entry.session.is_some() {
                    match resolve_entry(entry) {
                        Ok(resolved) => {
                            entry.auto_drain = true;
                            emit(
                                &event_tx,
                                Event::QueueDrainChanged {
                                    session_id: session_id.clone(),
                                    auto_drain: true,
                                    reason: None,
                                },
                            );
                            if let Err(message) = start_turn(
                                entry,
                                session_id.clone(),
                                held,
                                &resolved,
                                &event_tx,
                                turns,
                            ) {
                                insert_queue_front(
                                    &mut entry.pending.lock().expect("pending lock"),
                                    message,
                                );
                                emit(&event_tx, Event::Error {
                                                    session_id: Some(session_id.clone()),
                                                    error: CoreError::Internal {
                                                        detail: "The session was busy; the held message stays queued.".to_string(),
                                                    },
                                                    fatal: false,
                                                });
                            }
                        }
                        Err(error) => {
                            // No model: keep the message persisted
                            // and the queue paused; report typed.
                            insert_queue_front(
                                &mut entry.pending.lock().expect("pending lock"),
                                held,
                            );
                            emit(
                                &event_tx,
                                Event::Error {
                                    session_id: Some(session_id.clone()),
                                    error,
                                    fatal: false,
                                },
                            );
                        }
                    }
                } else {
                    // A turn/compaction owns the session: the
                    // user's explicit send-now intent queues the
                    // message at the front; the wrap-up starts it
                    // (an explicit user action may resume a paused
                    // queue).
                    insert_queue_front(&mut entry.pending.lock().expect("pending lock"), held);
                    entry.auto_drain = true;
                    emit(
                        &event_tx,
                        Event::QueueDrainChanged {
                            session_id: session_id.clone(),
                            auto_drain: true,
                            reason: None,
                        },
                    );
                }
            }
            pig_protocol::HeldDisposition::KeepQueueAndSend => {
                // Append behind the queued backlog; the queue
                // stays paused until Op::ResumeQueue.
                held.held = false;
                push_queue_back(&mut entry.pending.lock().expect("pending lock"), held);
            }
            pig_protocol::HeldDisposition::Cancel => {}
        }
        let items = queue_snapshot(&entry.pending);
        emit(
            &event_tx,
            Event::QueueChanged {
                session_id: session_id.clone(),
                items,
            },
        );
        persist_pending(&store, &session_id, &entry.pending);
        emit(
            &event_tx,
            Event::HeldQueueResolved {
                session_id: session_id.clone(),
            },
        );
        // Several sends may have arrived while the prompt was
        // open: surface the next one instead of dropping it.
        if let Some((text, items)) = held_prompt(entry) {
            emit(
                &event_tx,
                Event::HeldQueuePrompt {
                    session_id,
                    text,
                    items,
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deleting_a_session_stops_queued_subagents() {
        let state = crate::task::SessionToolState::for_test();
        let cancel = CancellationToken::new();
        let task_id = crate::task::register_agent_task_queued(
            &state,
            "queued child".into(),
            cancel.clone(),
            "a-queued".into(),
        );
        let entry = SessionEntry {
            session: None,
            plugin_dispatcher: crate::plugins::PluginToolset::new(
                &crate::plugins::PluginRegistry::shared(std::env::temp_dir()),
                &crate::config::WorkspaceTrust::untrusted(&std::env::temp_dir()),
                None,
                "s-test",
            )
            .dispatcher(),
            state: state.clone(),
            cwd: std::path::PathBuf::new(),
            runtime: None,
            cancel: None,
            retry: None,
            model_override: None,
            reasoning_level: None,
            last_permissions: Default::default(),
            pending: Arc::new(Mutex::new(Default::default())),
            notifications: Arc::new(Mutex::new(Default::default())),
            queue_seq: 0,
            auto_drain: true,
            cancel_promote: false,
            pending_plan: None,
            steerable: false,
            mcp_status: None,
        };
        stop_session_tasks(&entry);
        let tasks = state.tasks.lock().expect("task registry lock");
        let task = tasks
            .iter()
            .find(|task| task.id == task_id)
            .expect("queued task");
        assert_eq!(task.status, pig_protocol::TaskStatus::Killed);
        assert!(cancel.is_cancelled());
    }
}
