use pig_protocol::{DeliveryChannel, Event};

use super::runner::entry::NotificationMailbox;
use super::*;

impl Session {
    pub(super) fn drain_ready_notifications(
        &mut self,
        mailbox: &NotificationMailbox,
        tx: &async_channel::Sender<Event>,
    ) -> usize {
        let ids = mailbox
            .lock()
            .expect("notification lock")
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        if ids.is_empty() {
            return 0;
        }
        if self.rollout_failure.is_some() {
            self.note_notification_write_failure(ids.as_slice());
            self.report_rollout_failure(tx);
            return 0;
        }
        let pending = self
            .store
            .lock()
            .expect("store lock")
            .pending_notifications(&self.agent.id);
        let mut notifications = match pending {
            Ok(notifications) => notifications,
            Err(error) => {
                self.emit(
                    |session_id| Event::Error {
                        session_id: Some(session_id),
                        error,
                        fatal: false,
                    },
                    tx,
                );
                return 0;
            }
        };
        notifications.retain(|notification| {
            !self
                .delivery_receipts
                .lock()
                .expect("notification receipt lock")
                .contains_key(&notification.id)
                && ids.contains(&notification.id)
        });
        if notifications.is_empty() {
            self.refresh_notification_queue(mailbox, tx);
            return 0;
        }
        let count = notifications.len();
        let model_text = crate::task::notification::render_notification_batch(&notifications);
        let sent_at_ms = crate::model_io::now_ms();
        self.record(&RolloutRecord::TaskNotificationBatch {
            notifications: notifications.clone(),
            model_text: model_text.clone(),
            sent_at_ms,
        });
        if self.rollout_failure.is_some() {
            self.note_notification_write_failure(ids.as_slice());
            self.report_rollout_failure(tx);
            self.refresh_notification_queue(mailbox, tx);
            return 0;
        }
        self.agent.history.push(ChatMsg::user(model_text.clone()));
        self.delivery_receipts
            .lock()
            .expect("notification receipt lock")
            .extend(notifications.iter().map(|notification| {
                (
                    notification.id.clone(),
                    crate::task::notification::DeliveryReceipt {
                        channel: DeliveryChannel::Automatic,
                        confirmed: false,
                    },
                )
            }));
        self.emit(
            |session_id| Event::TaskNotificationBatch {
                session_id,
                notifications,
                model_text,
                sent_at_ms,
            },
            tx,
        );
        self.sync_rollout();
        self.note_notification_write_failure(ids.as_slice());
        self.report_rollout_failure(tx);
        self.refresh_notification_queue(mailbox, tx);
        count
    }

    fn note_notification_write_failure(&mut self, ids: &[String]) {
        let Some(error) = self.rollout_failure.clone() else {
            return;
        };
        if let Err(error) = self
            .store
            .lock()
            .expect("store lock")
            .record_notification_error(ids, &format!("{error:?}"))
        {
            self.notification_delivery_failure = Some(error);
        }
    }

    pub(super) fn restore_delivery_receipts(&mut self, receipts: HashMap<String, DeliveryChannel>) {
        let mut current = self
            .delivery_receipts
            .lock()
            .expect("notification receipt lock");
        current.clear();
        current.extend(receipts.into_iter().map(|(id, channel)| {
            (
                id,
                crate::task::notification::DeliveryReceipt {
                    channel,
                    confirmed: true,
                },
            )
        }));
    }

    pub(super) fn reconcile_notification_history(&mut self) -> Result<(), CoreError> {
        let records = Rollout::load(
            &self
                .data_dir
                .join("sessions")
                .join(format!("{}.jsonl", self.agent.id)),
        )?;
        let receipts = crate::rollout::notification_delivery_receipts(&records);
        self.store
            .lock()
            .expect("store lock")
            .reconcile_with_history(
                &self.agent.id,
                &receipts
                    .iter()
                    .map(|(id, channel)| (id.clone(), *channel))
                    .collect::<Vec<_>>(),
            )?;
        self.restore_delivery_receipts(receipts);
        self.notification_reconcile_required = false;
        Ok(())
    }

    fn refresh_notification_queue(
        &mut self,
        mailbox: &NotificationMailbox,
        tx: &async_channel::Sender<Event>,
    ) {
        let pending = self
            .store
            .lock()
            .expect("store lock")
            .pending_notifications(&self.agent.id);
        let mut notifications = match pending {
            Ok(notifications) => notifications,
            Err(error) => {
                self.emit(
                    |session_id| Event::Error {
                        session_id: Some(session_id),
                        error,
                        fatal: false,
                    },
                    tx,
                );
                return;
            }
        };
        notifications.retain(|notification| {
            !self
                .delivery_receipts
                .lock()
                .expect("notification receipt lock")
                .contains_key(&notification.id)
        });
        *mailbox.lock().expect("notification lock") = notifications
            .iter()
            .map(|notification| notification.id.clone())
            .collect();
        self.emit(
            |session_id| Event::TaskNotificationQueueChanged {
                session_id,
                notifications,
            },
            tx,
        );
    }
}
