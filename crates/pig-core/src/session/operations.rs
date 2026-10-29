use super::*;

impl Session {
    pub(crate) async fn select_plugin_tools(
        &mut self,
        config: &ResolvedModel,
        tx: &async_channel::Sender<Event>,
    ) {
        self.plugin_tool_filter = self.plugin_tools.select(config, None, tx).await;
    }

    /// Root session tool set: built-ins + Agent/AgentSwarm + MCP + plugins.
    /// Subagents narrow built-ins and inherit MCP/plugin tools by the same
    /// read-only rule. Dynamic plugin tools use the current request's selection.
    pub(crate) fn root_tools(&self) -> Vec<Box<dyn tool::Tool>> {
        let mut tools =
            tool::all_root_with_summary(&self.cwd, &self.data_dir, &self.profiles_summary);
        if let Some(mcp) = &self.mcp {
            tools.extend(mcp.tools());
        }
        tools.extend(self.plugin_tools.tools_filtered(&self.plugin_tool_filter));
        tools
    }

    /// Root session tool schema set for sampling and context accounting.
    pub(crate) fn root_schemas(&self) -> Vec<serde_json::Value> {
        self.root_tools()
            .iter()
            .map(|tool| tool::schema_with_strict(tool.as_ref()))
            .collect()
    }
}

/// Argument bundle for settle_cancelled_tool: the currently cancelled call +
/// the remaining unexecuted calls of the same response.
pub(super) struct CancelledTool<'a> {
    pub(super) call: &'a pig_provider::ToolCall,
    /// Tool card summary (persisted with the rollout record)
    pub(super) summary: String,
    pub(super) item_id: &'a str,
    pub(super) turn_id: &'a str,
    /// Calls after the current one in the same response (will not execute;
    /// empty receipts are backfilled to keep tool_use pairing)
    pub(super) rest: &'a [pig_provider::ToolCall],
    /// Internal item ids of `rest`, in the same order (parallel to `rest`).
    pub(super) rest_item_ids: &'a [String],
    /// Agent card metadata (the Agent tool's cancellation path; None for
    /// other tools)
    pub(super) card: Option<crate::rollout::AgentCardRecord>,
    /// Batch agent card metadata (the AgentSwarm tool's cancellation path; an
    /// empty list for other tools)
    pub(super) cards: Vec<crate::rollout::AgentCardRecord>,
}

impl Session {
    /// Reconcile cached PDF costs with the window of the model that will
    /// consume the history. A measured page-tree cost is window-independent
    /// and is left untouched; only the bounded fallback for an unprovable
    /// structure is recomputed from its tag, without reading document bytes.
    pub(super) fn reprice_document_estimates(&mut self, context_window: u64) {
        for document in self
            .history
            .iter_mut()
            .flat_map(|message| &mut message.documents)
        {
            if document
                .token_estimate
                .is_some_and(|cost| cost.is_window_fallback())
            {
                document.token_estimate =
                    Some(pig_provider::DocumentTokenEstimate::window_fallback(
                        pig_utils::pdf::unknown_document_token_reserve(context_window),
                        context_window,
                    ));
            }
        }
    }

    pub(super) fn reproject_media(
        &mut self,
        media: tool::MediaCapabilities,
        context_window: u64,
    ) -> Result<(), CoreError> {
        if self.history.is_empty() {
            self.projected_input_image = Some(media.input_image);
            self.projected_input_pdf = Some(media.input_pdf);
            return Ok(());
        }
        let media_dir = pig_utils::paths::media_dir(&self.data_dir.join("sessions"), &self.id);
        let records = Rollout::load(
            &self
                .data_dir
                .join("sessions")
                .join(format!("{}.jsonl", self.id)),
        )?;
        let system = self
            .history
            .first()
            .and_then(|message| message.content.clone())
            .unwrap_or_default();
        let (history, user_ixs) = crate::rollout::rebuild_history_with_media_root(
            &records,
            system,
            Some(media),
            &media_dir,
            Some(context_window),
        );
        self.history = history;
        self.user_ixs = user_ixs;
        self.io_last_input = Vec::new();
        self.usage_anchor = None;
        self.projected_input_image = Some(media.input_image);
        self.projected_input_pdf = Some(media.input_pdf);
        Ok(())
    }

    /// Persist images beside the session rollout. A failure removes them from
    /// the live message and appends the same deterministic degradation the
    /// durable record carries, so live and restart projections never diverge.
    pub(super) fn persist_tool_images(
        &self,
        output: &mut String,
        images: &mut Vec<pig_provider::ChatImage>,
    ) -> Vec<crate::rollout::ImageRef> {
        if images.is_empty() {
            return Vec::new();
        }
        let media_dir = pig_utils::paths::media_dir(&self.data_dir.join("sessions"), &self.id);
        match crate::rollout::persist_chat_images(&media_dir, images) {
            Ok(refs) if refs.len() == images.len() => refs,
            Ok(_) => {
                tracing::warn!("Failed to persist all tool images; degrading the live message");
                let count = images.len();
                images.clear();
                output.push_str(&format!(
                    "\n\n[{count} image(s) could not be saved and were removed from this tool result]"
                ));
                Vec::new()
            }
            Err(error) => {
                tracing::warn!("Failed to persist tool images: {error}");
                let count = images.len();
                images.clear();
                output.push_str(&format!(
                    "\n\n[{count} image(s) could not be saved and were removed from this tool result]"
                ));
                Vec::new()
            }
        }
    }

    /// Persist documents beside the session rollout. A failure removes them
    /// from the live message and appends the same deterministic degradation a
    /// restart would produce, so the two projections never silently diverge.
    pub(super) fn persist_tool_documents(
        &self,
        output: &mut String,
        documents: &mut Vec<pig_provider::ChatDocument>,
        context_window: u64,
    ) -> Vec<crate::rollout::DocumentRef> {
        if documents.is_empty() {
            return Vec::new();
        }
        let media_dir = pig_utils::paths::media_dir(&self.data_dir.join("sessions"), &self.id);
        match crate::rollout::persist_chat_documents(&media_dir, documents, context_window) {
            Ok(refs) if refs.len() == documents.len() => refs,
            Ok(_) => {
                let count = documents.len();
                documents.clear();
                output.push_str(&format!(
                    "\n\n[{count} PDF document(s) could not be saved and were removed from this tool result]"
                ));
                Vec::new()
            }
            Err(error) => {
                tracing::warn!("Failed to persist tool documents: {error}");
                let count = documents.len();
                documents.clear();
                output.push_str(&format!(
                    "\n\n[{count} PDF document(s) could not be saved and were removed from this tool result]"
                ));
                Vec::new()
            }
        }
    }

    /// Recall the last user message: drop it and the whole turn it triggered
    /// from the model context (history truncated at the message's index) and
    /// append the Recall tombstone so resume rebuild/replay reproduces the
    /// exclusion. Usage statistics (TurnStats/StepUsage records, the SQLite
    /// tables) are deliberately untouched, but the in-memory preflight
    /// watermark is re-estimated from the truncated history: the old real
    /// sample measured the recalled request and must not drive the next
    /// preflight into a stale auto-compaction. Rejections (idle/no-message
    /// guards) are the caller's job; this errors only when no recallable user
    /// message exists.
    pub fn recall_last_message(
        &mut self,
        tx: &async_channel::Sender<Event>,
    ) -> Result<(), CoreError> {
        let Some(entry) = self.user_ixs.pop() else {
            return Err(CoreError::NothingToRecall);
        };
        tracing::info!(truncate_ix = ?entry, "recalled the last user message");
        // None = there is no in-memory truncation point: an omitted user
        // survives only in the rollout file, and a promoted compact checkpoint
        // is already kept after the note (recalling it must not leave a bare
        // note that exposes the work it summarized). The tombstone alone keeps
        // the file/reload consistent.
        if let Some(ix) = entry {
            self.history.truncate(ix);
            // The model-io trace's delta baseline no longer matches the
            // history; reset it so the next trace falls back to a full
            // snapshot (the same self-healing as after resume)
            self.io_last_input = Vec::new();
            // The saved real usage measured the request that no longer
            // exists. Re-estimate the preflight fallback from the truncated
            // history; keeping the old high sample would let the next
            // preflight compact solely because of the recalled turn. The
            // next real Usage replaces this estimate.
            self.last_total_tokens = Some(crate::session::estimate::estimate_history_tokens(
                &self.history,
            ));
        }
        // The anchor measured a history that no longer exists.
        self.usage_anchor = None;
        // The recalled message carried the last reminder injection (date,
        // AGENTS.md, memory, permission boundary). It is gone from the model
        // context now, so forget the "already reminded" state and let the next
        // turn re-inject the current reminders.
        self.permissions_reminded = None;
        self.date_reminded.clear();
        self.agents_reminded.clear();
        self.memory_reminded.clear();
        self.notification_reconcile_required = true;
        self.record(&RolloutRecord::Recall);
        self.sync_rollout();
        self.emit(|session_id| Event::MessageRecalled { session_id }, tx);
        self.report_rollout_failure(tx);
        Ok(())
    }

    /// Unified wrap-up for mid-tool cancellation (user hits stop): the
    /// current call gets a "Stopped" receipt — history (tool_use/tool_result
    /// pairing, so a dangling pair does not get rejected by the API on the
    /// next request), rollout (replay after restart rebuilds the card instead
    /// of it vanishing), ToolCallEnd (settles the live card); calls later in
    /// the same response will not execute, and receive receipts too so
    /// pairing stays complete.
    pub(super) fn stopped_tool_result(call_id: &str) -> ChatMsg {
        ChatMsg::stopped_tool_result(call_id, "Stopped".to_string())
    }

    pub(super) fn settle_cancelled_tool(
        &mut self,
        cancelled: CancelledTool<'_>,
        tx: &async_channel::Sender<Event>,
    ) {
        let rest = cancelled.rest;
        self.history
            .push(Self::stopped_tool_result(&cancelled.call.id));
        self.record(&RolloutRecord::ToolCall {
            tool: cancelled.call.name.clone(),
            summary: cancelled.summary,
            arguments: cancelled.call.arguments.clone(),
            output: "Stopped".to_string(),
            is_error: true,
            stopped: true,
            edit: None,
            agent_card: cancelled.card,
            agent_cards: cancelled.cards,
            images: vec![],
            documents: vec![],
            delivered_notification_ids: vec![],
        });
        let item_id = cancelled.item_id.to_string();
        self.emit(
            |session_id| Event::ToolCallEnd {
                session_id,
                item_id,
                output: "Stopped".to_string(),
                is_error: true,
                stopped: true,
                edit: None,
            },
            tx,
        );
        for (rest_ix, call) in rest.iter().enumerate() {
            let summary = tool::summarize(call);
            let detail = serde_json::from_str::<serde_json::Value>(&call.arguments)
                .map(|value| serde_json::to_string_pretty(&value).unwrap_or_default())
                .unwrap_or_else(|_| call.arguments.clone());
            self.history.push(Self::stopped_tool_result(&call.id));
            self.record(&RolloutRecord::ToolCall {
                tool: call.name.clone(),
                summary: summary.clone(),
                arguments: call.arguments.clone(),
                output: "Stopped".to_string(),
                is_error: true,
                stopped: true,
                edit: None,
                agent_card: None,
                agent_cards: vec![],
                images: vec![],
                documents: vec![],
                delivered_notification_ids: vec![],
            });
            let item_id = cancelled
                .rest_item_ids
                .get(rest_ix)
                .cloned()
                .unwrap_or_else(|| format!("{}-tool-{}", cancelled.turn_id, call.id));
            self.emit(
                |session_id| Event::ToolCallBegin {
                    session_id,
                    item_id: item_id.clone(),
                    tool: call.name.clone(),
                    input_summary: summary,
                    detail,
                },
                tx,
            );
            self.emit(
                |session_id| Event::ToolCallEnd {
                    session_id,
                    item_id,
                    output: "Stopped".to_string(),
                    is_error: true,
                    stopped: true,
                    edit: None,
                },
                tx,
            );
        }
    }

    /// Turn wrap-up: produce "changes this turn" (persisted to rollout +
    /// pushed to the UI message-stream panel); nothing is emitted if there
    /// are no changes.
    pub(super) fn flush_turn_changes(&mut self, tx: &async_channel::Sender<Event>) {
        // Backstop for baselines inserted after the last tool completion (for
        // example a cancelled tool or a background child's final write); the
        // shared dirty queue makes this exactly-once regardless of which
        // driver gets here.
        persist_tracker_originals(&mut self.tracker, &self.store, &self.id);
        let changes = self.tracker.take_turn_changes(&self.cwd);
        if changes.is_empty() {
            return;
        }
        let files: Vec<pig_protocol::EditDiff> = changes.into_iter().map(Into::into).collect();
        self.record(&RolloutRecord::TurnChanges {
            files: files.clone(),
        });
        self.emit(
            |session_id| Event::TurnFileChanges { session_id, files },
            tx,
        );
    }

    pub fn set_permissions(&mut self, permissions: PermissionSettings) {
        self.permissions = permissions;
    }

    /// Canonical permission state (sandbox / approval policy / reviewer).
    pub fn permissions(&self) -> PermissionSettings {
        self.permissions
    }

    pub fn set_plan_mode(&mut self, enabled: bool) {
        self.plan_enabled = enabled;
    }

    pub fn set_model(&mut self, selection: ModelSelection) {
        self.model_override = Some(selection);
    }

    /// The session's single resolved runtime view. `None` = config not loaded
    /// (inheritance-only child resolution and no explicit provider resolution).
    pub(crate) fn runtime_config(&self) -> Option<&crate::config::RuntimeConfig> {
        self.runtime.as_deref()
    }

    /// Shared handle to the same stored runtime for child/swarm resolution,
    /// which must hold it across later mutable borrows of the session.
    pub(crate) fn runtime_config_cloned(&self) -> Option<Arc<crate::config::RuntimeConfig>> {
        self.runtime.clone()
    }

    /// Effective config snapshot for non-model consumers (memory, hooks,
    /// model-io retention). Always derived from the stored runtime, never a
    /// second copy.
    pub(crate) fn config_snapshot(&self) -> Option<&AppConfig> {
        self.runtime.as_deref().map(|runtime| runtime.config())
    }

    pub fn revert_file(&mut self, path: &str, tx: &async_channel::Sender<Event>) {
        // Path resolution errors (original English) and revert errors
        // uniformly go through the CoreError channel
        let result = tool::resolve_checked(&self.cwd, path, false)
            .map_err(|e| CoreError::Internal { detail: e })
            .and_then(|full| self.tracker.revert(&full).map(|()| full));
        match result {
            Ok(full) => {
                // After a revert neither the change nor the baseline matters
                // anymore: clear the store (change row + original snapshot
                // row)
                {
                    let store = self.store.lock().expect("store lock");
                    store.delete_file_change(&self.id, path);
                    store.delete_file_original(&self.id, &full.to_string_lossy());
                }
                self.emit(
                    |session_id| Event::FileReverted {
                        session_id,
                        path: path.to_string(),
                    },
                    tx,
                );
            }
            Err(error) => self.emit(
                |session_id| Event::Error {
                    session_id: Some(session_id),
                    error,
                    fatal: false,
                },
                tx,
            ),
        }
    }
}
