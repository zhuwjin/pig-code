use std::collections::BTreeSet;

use futures_util::future::BoxFuture;
use pig_protocol::Event;

use super::{AgentBackendError, AgentBackendResult, AgentExecutionCapability, AgentRunIdentity};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AgentExecutionIdentity {
    pub run: AgentRunIdentity,
    pub attempt: u64,
    pub backend_epoch: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentExecutionCursor {
    pub attempt: u64,
    pub backend_epoch: String,
    pub sequence: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AgentExecutionInput {
    pub message_id: String,
    pub payload: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentExecutionStop {
    Graceful { reason: String },
    Force { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentExecutionReceipt {
    pub cursor: AgentExecutionCursor,
}

#[derive(Clone, Debug)]
pub enum AgentExecutionUpdate {
    Output {
        sequence: u64,
        event: Event,
    },
    Settled {
        cursor: AgentExecutionCursor,
        result: AgentBackendResult,
    },
    Disconnected {
        cursor: AgentExecutionCursor,
        detail: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentExecutionRelease {
    pub identity: AgentExecutionIdentity,
    pub cursor: AgentExecutionCursor,
    pub released_at_ms: u64,
}

pub trait AgentExecutionHandle: Send {
    fn identity(&self) -> &AgentExecutionIdentity;
    fn capabilities(&self) -> BTreeSet<AgentExecutionCapability>;
    fn send(
        &mut self,
        input: AgentExecutionInput,
    ) -> BoxFuture<'_, Result<AgentExecutionReceipt, AgentBackendError>>;
    fn stop(
        &mut self,
        stop: AgentExecutionStop,
    ) -> BoxFuture<'_, Result<AgentExecutionReceipt, AgentBackendError>>;
    fn detach(&mut self) -> BoxFuture<'_, Result<AgentExecutionReceipt, AgentBackendError>>;
    fn next(&mut self) -> BoxFuture<'_, Result<AgentExecutionUpdate, AgentBackendError>>;
    fn release(&mut self) -> BoxFuture<'_, Result<AgentExecutionRelease, AgentBackendError>>;
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::future::Future;
    use std::sync::{Arc, Mutex, MutexGuard};
    use std::task::Poll;

    use futures_util::future::poll_fn;
    use tokio::sync::Notify;

    use super::*;

    #[derive(Default)]
    struct FakeState {
        inputs: Vec<AgentExecutionInput>,
        stops: Vec<AgentExecutionStop>,
        updates: VecDeque<AgentExecutionUpdate>,
        cursor: u64,
        detached: Option<AgentExecutionReceipt>,
        quiescent: bool,
        release: Option<AgentExecutionRelease>,
    }

    #[derive(Clone)]
    struct FakeController {
        state: Arc<Mutex<FakeState>>,
        notify: Arc<Notify>,
    }

    impl FakeController {
        fn lock(&self) -> MutexGuard<'_, FakeState> {
            self.state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        }

        fn enqueue(&self, update: AgentExecutionUpdate) {
            self.lock().updates.push_back(update);
            self.notify.notify_waiters();
        }

        fn quiesce(&self) {
            self.lock().quiescent = true;
            self.notify.notify_waiters();
        }
    }

    struct FakeExecutionHandle {
        identity: AgentExecutionIdentity,
        state: Arc<Mutex<FakeState>>,
        notify: Arc<Notify>,
    }

    impl FakeExecutionHandle {
        fn new(identity: AgentExecutionIdentity) -> Self {
            Self {
                identity,
                state: Arc::new(Mutex::new(FakeState::default())),
                notify: Arc::new(Notify::new()),
            }
        }

        fn controller(&self) -> FakeController {
            FakeController {
                state: self.state.clone(),
                notify: self.notify.clone(),
            }
        }

        fn lock(&self) -> MutexGuard<'_, FakeState> {
            self.state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        }

        fn cursor(&self, sequence: u64) -> AgentExecutionCursor {
            AgentExecutionCursor {
                attempt: self.identity.attempt,
                backend_epoch: self.identity.backend_epoch.clone(),
                sequence,
            }
        }

        fn check_attached(&self, state: &FakeState) -> Result<(), AgentBackendError> {
            if state.detached.is_some() {
                return Err(AgentBackendError::Disconnected(
                    "execution is detached".to_string(),
                ));
            }
            Ok(())
        }
    }

    impl AgentExecutionHandle for FakeExecutionHandle {
        fn identity(&self) -> &AgentExecutionIdentity {
            &self.identity
        }

        fn capabilities(&self) -> BTreeSet<AgentExecutionCapability> {
            [
                AgentExecutionCapability::Steer,
                AgentExecutionCapability::Stop,
                AgentExecutionCapability::Detach,
            ]
            .into_iter()
            .collect()
        }

        fn send(
            &mut self,
            input: AgentExecutionInput,
        ) -> BoxFuture<'_, Result<AgentExecutionReceipt, AgentBackendError>> {
            Box::pin(async move {
                let mut state = self.lock();
                self.check_attached(&state)?;
                state.inputs.push(input);
                Ok(AgentExecutionReceipt {
                    cursor: self.cursor(state.cursor),
                })
            })
        }

        fn stop(
            &mut self,
            stop: AgentExecutionStop,
        ) -> BoxFuture<'_, Result<AgentExecutionReceipt, AgentBackendError>> {
            Box::pin(async move {
                let mut state = self.lock();
                self.check_attached(&state)?;
                state.stops.push(stop);
                Ok(AgentExecutionReceipt {
                    cursor: self.cursor(state.cursor),
                })
            })
        }

        fn detach(&mut self) -> BoxFuture<'_, Result<AgentExecutionReceipt, AgentBackendError>> {
            Box::pin(async move {
                let mut state = self.lock();
                if let Some(receipt) = &state.detached {
                    return Ok(receipt.clone());
                }
                let receipt = AgentExecutionReceipt {
                    cursor: self.cursor(state.cursor),
                };
                state.detached = Some(receipt.clone());
                Ok(receipt)
            })
        }

        fn next(&mut self) -> BoxFuture<'_, Result<AgentExecutionUpdate, AgentBackendError>> {
            Box::pin(async move {
                loop {
                    let notified = self.notify.notified();
                    let update = {
                        let mut state = self.lock();
                        self.check_attached(&state)?;
                        state.updates.pop_front().map(|update| {
                            match &update {
                                AgentExecutionUpdate::Output { sequence, .. } => {
                                    state.cursor = *sequence;
                                }
                                AgentExecutionUpdate::Settled { cursor, .. }
                                | AgentExecutionUpdate::Disconnected { cursor, .. } => {
                                    state.cursor = cursor.sequence;
                                }
                            }
                            update
                        })
                    };
                    if let Some(update) = update {
                        return Ok(update);
                    }
                    notified.await;
                }
            })
        }

        fn release(&mut self) -> BoxFuture<'_, Result<AgentExecutionRelease, AgentBackendError>> {
            Box::pin(async move {
                loop {
                    let notified = self.notify.notified();
                    {
                        let mut state = self.lock();
                        self.check_attached(&state)?;
                        if let Some(release) = &state.release {
                            return Ok(release.clone());
                        }
                        if state.quiescent {
                            let release = AgentExecutionRelease {
                                identity: self.identity.clone(),
                                cursor: self.cursor(state.cursor),
                                released_at_ms: 0,
                            };
                            state.release = Some(release.clone());
                            return Ok(release);
                        }
                    }
                    notified.await;
                }
            })
        }
    }

    fn identity(attempt: u64) -> AgentExecutionIdentity {
        AgentExecutionIdentity {
            run: AgentRunIdentity {
                session_id: "session-1".into(),
                agent_id: "agent-1".into(),
                run_id: "run-1".into(),
            },
            attempt,
            backend_epoch: format!("epoch-{attempt}"),
        }
    }

    #[tokio::test]
    async fn stop_is_not_release_and_release_waits_for_quiescence() {
        let mut handle = FakeExecutionHandle::new(identity(2));
        let controller = handle.controller();
        handle
            .stop(AgentExecutionStop::Graceful {
                reason: "user stop".into(),
            })
            .await
            .expect("graceful stop accepted");
        handle
            .stop(AgentExecutionStop::Force {
                reason: "grace elapsed".into(),
            })
            .await
            .expect("force stop accepted");
        assert_eq!(controller.lock().stops.len(), 2);
        assert!(!controller.lock().quiescent);

        let mut release = Box::pin(handle.release());
        let pending =
            poll_fn(|context| Poll::Ready(release.as_mut().poll(context).is_pending())).await;
        assert!(pending, "release completed before execution quiesced");
        controller.quiesce();
        let released = release.await.expect("release after quiescence");
        assert_eq!(released.identity.attempt, 2);
        let repeated = handle.release().await.expect("idempotent release");
        assert_eq!(repeated, released);
    }

    #[tokio::test]
    async fn detach_preserves_execution_and_rejects_later_control() {
        let mut handle = FakeExecutionHandle::new(identity(1));
        let controller = handle.controller();
        controller.enqueue(AgentExecutionUpdate::Output {
            sequence: 7,
            event: Event::TextDelta {
                session_id: "session-1".into(),
                item_id: "item-1".into(),
                delta: "partial".into(),
            },
        });
        let update = handle.next().await.expect("output update");
        assert!(matches!(
            update,
            AgentExecutionUpdate::Output { sequence: 7, .. }
        ));
        let detached = handle.detach().await.expect("detach");
        assert_eq!(detached.cursor.sequence, 7);
        assert!(controller.lock().stops.is_empty());
        assert!(!controller.lock().quiescent);
        assert!(matches!(
            handle
                .stop(AgentExecutionStop::Graceful {
                    reason: "too late".into(),
                })
                .await,
            Err(AgentBackendError::Disconnected(_))
        ));
        let repeated = handle.detach().await.expect("idempotent detach");
        assert_eq!(repeated, detached);
    }

    #[tokio::test]
    async fn output_settlement_and_release_share_one_cursor() {
        let mut handle = FakeExecutionHandle::new(identity(4));
        let controller = handle.controller();
        controller.enqueue(AgentExecutionUpdate::Output {
            sequence: 10,
            event: Event::TextDelta {
                session_id: "session-1".into(),
                item_id: "item-1".into(),
                delta: "done".into(),
            },
        });
        let terminal_cursor = AgentExecutionCursor {
            attempt: 4,
            backend_epoch: "epoch-4".into(),
            sequence: 11,
        };
        controller.enqueue(AgentExecutionUpdate::Settled {
            cursor: terminal_cursor.clone(),
            result: AgentBackendResult {
                status: pig_protocol::AgentRunStatus::Completed,
                stop_reason: "completed".into(),
                text: "done".into(),
                turns: 1,
                queued: false,
                usage: (1, 2, 3),
            },
        });
        handle.next().await.expect("output");
        let settled = handle.next().await.expect("settlement");
        assert!(matches!(
            settled,
            AgentExecutionUpdate::Settled { cursor, .. } if cursor == terminal_cursor
        ));
        controller.quiesce();
        let released = handle.release().await.expect("release");
        assert_eq!(released.cursor, terminal_cursor);
        assert_eq!(released.identity, identity(4));
    }
}
