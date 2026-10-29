use super::*;

impl Session {
    pub(super) fn emit(
        &mut self,
        build: impl FnOnce(String) -> Event,
        tx: &async_channel::Sender<Event>,
    ) {
        emit_bg(&self.id, tx, build);
    }

    pub(super) fn record(&mut self, record: &RolloutRecord) {
        if self.rollout_failure.is_some() {
            // A previous durable append failed; appending more units would
            // leave the file diverging from memory with no way to repair it.
            return;
        }
        let Some(rollout) = &mut self.rollout else {
            return;
        };
        if let Err(error) = rollout.append(record) {
            let structured = error.into_core(rollout.path());
            tracing::error!(
                session_id = %self.id,
                error = ?structured,
                "rollout append failed; later records for this session are not persisted"
            );
            self.rollout_failure = Some(structured);
        }
    }

    /// Request real durability (`sync_data`) for the records written so far.
    /// Called at turn/compaction/recall boundaries, never per token or per
    /// stream event.
    pub(super) fn sync_rollout(&mut self) {
        if self.rollout_failure.is_some() {
            return;
        }
        let Some(rollout) = &mut self.rollout else {
            return;
        };
        if let Err(error) = rollout.sync() {
            let structured = error.into_core(rollout.path());
            tracing::error!(
                session_id = %self.id,
                error = ?structured,
                "rollout sync failed at a durable boundary"
            );
            self.rollout_failure = Some(structured);
        }
        if self.rollout_failure.is_none() {
            self.flush_delivery_receipts();
            if self.notification_reconcile_required
                && let Err(error) = self.reconcile_notification_history()
            {
                self.notification_delivery_failure = Some(error);
            }
        }
    }

    fn flush_delivery_receipts(&mut self) {
        let pending = self
            .delivery_receipts
            .lock()
            .expect("notification receipt lock")
            .iter()
            .filter(|(_, receipt)| !receipt.confirmed)
            .map(|(id, receipt)| (id.clone(), receipt.channel))
            .collect::<Vec<_>>();
        for (id, channel) in pending {
            let result = self.store.lock().expect("store lock").mark_delivered(
                std::slice::from_ref(&id),
                channel,
                crate::model_io::now_ms(),
            );
            match result {
                Ok(()) => {
                    if let Some(receipt) = self
                        .delivery_receipts
                        .lock()
                        .expect("notification receipt lock")
                        .get_mut(&id)
                    {
                        receipt.confirmed = true;
                    }
                }
                Err(error) => self.notification_delivery_failure = Some(error),
            }
        }
    }

    /// Surface durable-boundary failures without hiding a later outbox error
    /// behind an already-reported rollout error.
    pub(super) fn report_rollout_failure(&mut self, tx: &async_channel::Sender<Event>) {
        if !self.rollout_failure_reported
            && let Some(error) = self.rollout_failure.clone()
        {
            self.rollout_failure_reported = true;
            self.emit(
                |session_id| Event::Error {
                    session_id: Some(session_id),
                    error,
                    fatal: false,
                },
                tx,
            );
        }
        if let Some(error) = self.notification_delivery_failure.take() {
            self.emit(
                |session_id| Event::Error {
                    session_id: Some(session_id),
                    error,
                    fatal: false,
                },
                tx,
            );
        }
    }

    pub(super) fn record_prompt_snapshot(&mut self, text: &str) {
        let snapshot = crate::rollout::PromptSnapshot {
            text: text.to_string(),
            git_snapshot: self.git_snapshot.clone(),
            agents_prompt: self.agents_prompt.clone(),
            skills_prompt: self.skills_prompt.clone(),
            skills_state: self.prompt_skills_state.clone().unwrap_or_else(|| {
                skills_state_with_trust(&self.data_dir, self.trust.admits(&self.cwd))
            }),
            memory_prompt: self.memory_prompt.clone(),
            memory_enabled: self.memory_enabled,
            profiles_summary: self.profiles_summary.clone(),
            date_frozen: self.date_frozen.clone(),
            reminders: crate::rollout::PromptReminderState {
                date: self.date_reminded.clone(),
                agents: self.agents_reminded.clone(),
                memory: self.memory_reminded.clone(),
                permissions: self.permissions_reminded.clone(),
            },
        };
        if self.recorded_prompt.as_ref() == Some(&snapshot) {
            return;
        }
        self.record(&RolloutRecord::PromptSnapshot {
            snapshot: snapshot.clone(),
        });
        self.recorded_prompt = Some(snapshot);
    }

    pub(super) fn touch_index(&mut self) {
        let id = self.id.clone();
        self.store
            .lock()
            .expect("store lock")
            .update_session(&id, |meta| meta.updated_at = now_secs());
    }
}
