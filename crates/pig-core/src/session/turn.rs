use super::*;

/// Title seed from the first message: the first non-empty line, trimmed,
/// capped at 30 chars. Multi-line/tabbed content must not leak `\n`/`\t` into
/// the stored title — the sidebar measures titles with gpui's `shape_line`,
/// which debug_assert-panics on embedded newlines (2026-10-08 crash:
/// expanding a workspace whose session title carried the first message's raw
/// newlines killed the app).
fn seed_title(content: &str) -> String {
    content
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .chars()
        .take(30)
        .collect()
}

impl Session {
    /// Lazy MCP connection (once per session): reads `.pigcode/mcp.json` +
    /// `data_dir/mcp.json` under the workspace-trust decision (project servers
    /// are repository content); with no config we get an empty manager at
    /// negligible cost; a single server failure does not affect the others.
    /// A normal step and a manual compaction share this snapshot so both price
    /// and send the same tool schemas.
    pub(crate) async fn ensure_mcp_connected(&mut self) {
        if self.mcp.is_none() {
            self.mcp = Some(Arc::new(
                crate::mcp::McpManager::connect_all_with_trust(
                    &self.cwd,
                    &self.data_dir,
                    &self.trust,
                )
                .await,
            ));
        }
    }

    /// Steer boundary: move every steer-lane item of the pending list into
    /// the conversation as a real user message (rollout record + UserMessage
    /// event — the timeline, resume replay, and the next model request all
    /// see it; steer injections skip UserPromptSubmit hooks). Returns the
    /// number of injected messages. Safe to call at any step boundary; the
    /// manager may concurrently push more items (the lock arbitrates)
    fn drain_steer(
        &mut self,
        pending: &PendingMailbox,
        tx: &async_channel::Sender<Event>,
    ) -> usize {
        // One item per boundary (ZCode semantics): each steer deserves its
        // own model round of reaction, and the earliest steer wins even when
        // queue-lane items sit ahead of it in the shared list
        let drained: Vec<PendingMessage> = {
            let mut list = pending.lock().expect("pending lock");
            let Some(pos) = list.iter().position(|m| m.steer) else {
                return 0;
            };
            vec![list.remove(pos).expect("position checked")]
        };
        for message in drained {
            let history_text = pointer_file_references(&self.cwd, &message.text, &message.files);
            // The steered message is admitted now: its send time is this instant
            // (the queue arrival is not recorded, the same as an immediate send)
            let sent_at_ms = crate::model_io::now_ms();
            self.record(&RolloutRecord::User {
                text: message.text.clone(),
                files: message.files.clone(),
                images: vec![],
                sent_at_ms,
                sent_text: Some(history_text.clone()),
                sent_images: vec![],
                model_text: None,
                sent_input_image: None,
            });
            self.history.push(ChatMsg::user(history_text.clone()));
            self.user_ixs.push(Some(self.history.len() - 1));
            self.emit(
                |session_id| Event::UserMessage {
                    session_id,
                    text: history_text,
                    files: message.files.clone(),
                    image_nums: vec![],
                    sent_at_ms,
                },
                tx,
            );
        }
        // Authoritative snapshot of the remaining steer lane
        let items = pending
            .lock()
            .expect("pending lock")
            .iter()
            .filter(|m| m.steer)
            .map(|m| pig_protocol::QueueItem {
                id: m.id.clone(),
                text: m.text.clone(),
                permissions: m.permissions,
                images: m.images.clone(),
            })
            .collect();
        self.emit(|session_id| Event::SteerChanged { session_id, items }, tx);
        super::persist_pending(&self.store, &self.id, pending);
        1
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn run_turn(
        &mut self,
        content: String,
        files: Vec<String>,
        images: Vec<pig_protocol::PendingImage>,
        config: &ResolvedModel,
        tx: &async_channel::Sender<Event>,
        control: pig_provider::CallControl,
        pending: PendingMailbox,
        notifications: super::runner::entry::NotificationMailbox,
        notification_only: bool,
    ) -> TurnOutcome {
        let cancel = control.cancel_token();
        // ---- SessionStart hooks (once per Session instance, at the first
        // turn; the turn's cancel token covers the run). Nothing to block at
        // session start — a block decision is logged; contexts stash for the
        // message below ----
        if !self.session_start_hook_ran {
            self.session_start_hook_ran = true;
            if self.hooks.is_active() {
                let payload = crate::hooks::HookPayload::session_start(
                    &self.id,
                    &self.cwd,
                    self.permissions,
                    self.session_start_source,
                )
                .with_transcript(&self.history);
                let run = self.hook_run_ctx(tx, cancel);
                let effects = crate::hooks::run_event(
                    &self.hooks,
                    crate::hooks::HookEvent::SessionStart,
                    payload,
                    &run,
                )
                .await;
                if effects.blocked.is_some() {
                    tracing::warn!(
                        "SessionStart hook reported a block; nothing to block at session start, continuing"
                    );
                }
                self.hook_context_stash.extend(effects.additional_contexts);
            }
        }
        // ---- UserPromptSubmit hooks (every real user message; notification
        // turns are exempt). A block stops the turn before it starts: the
        // message is never delivered ----
        if self.hooks.is_active() && !notification_only {
            let payload = crate::hooks::HookPayload::user_prompt(
                &self.id,
                &self.cwd,
                self.permissions,
                &content,
            )
            .with_transcript(&self.history);
            let run = self.hook_run_ctx(tx, cancel);
            let effects = crate::hooks::run_event(
                &self.hooks,
                crate::hooks::HookEvent::UserPromptSubmit,
                payload,
                &run,
            )
            .await;
            if let Some(reason) = effects.blocked {
                self.emit(
                    |session_id| Event::Error {
                        session_id: Some(session_id),
                        error: CoreError::HookBlocked { detail: reason },
                        fatal: true,
                    },
                    tx,
                );
                // The turn never started: nothing failed, the queue may drain
                return TurnOutcome::Completed;
            }
            self.hook_context_stash.extend(effects.additional_contexts);
        }
        self.turn_counter += 1;
        // Unique across restarts (wall-clock nanos + counter): StepUsage and
        // TurnStats carry it so SQLite usage insertion is idempotent and crash
        // recovery reuses the same identity.
        self.usage_turn_id = Some(format!(
            "t{}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0),
            self.turn_counter
        ));
        self.turn_input = 0;
        self.turn_cache_read = 0;
        self.turn_output = 0;
        self.turn_api_ms = 0;
        self.turn_ttft_ms = 0;
        self.turn_ttft_steps = 0;
        self.turn_api_steps = 0;
        self.turn_tool_ms = 0;
        self.overflow_recovered = false;
        // A model switch invalidates the request-shape baseline; compact,
        // media reprojection and recall clear it at their own sites.
        if self
            .usage_anchor
            .as_ref()
            .is_some_and(|anchor| anchor.model_key != model_key(config))
        {
            self.usage_anchor = None;
        }
        let turn_id = format!("turn-{}", self.turn_counter);
        let started = Instant::now();
        self.emit(
            |session_id| Event::TurnStarted {
                session_id,
                turn_id: turn_id.clone(),
            },
            tx,
        );

        // A model switch can keep the same media capabilities while changing
        // the context window. Reprice only the window-derived PDF fallbacks so
        // the preflight, request and composition use the new window; measured
        // page-tree costs are never rescanned.
        self.reprice_document_estimates(config.context_window);
        let media = tool::MediaCapabilities::new(config.input_image, config.input_pdf);
        if (self.projected_input_image != Some(media.input_image)
            || self.projected_input_pdf != Some(media.input_pdf))
            && let Err(error) = self.reproject_media(media, config.context_window)
        {
            self.emit(
                |session_id| Event::Error {
                    session_id: Some(session_id),
                    error,
                    fatal: true,
                },
                tx,
            );
            return TurnOutcome::Failed;
        }
        let skills_state = skills_state_with_trust(&self.data_dir, self.trust.admits(&self.cwd));
        let memory_enabled = self.memory_enabled;
        let prompt_text = match (
            &self.prompt_text,
            self.prompt_memory_enabled,
            &self.prompt_skills_state,
        ) {
            (Some(text), Some(memory), Some(state))
                if memory == memory_enabled && state == &skills_state =>
            {
                text.clone()
            }
            _ => {
                if self.prompt_skills_state.as_deref() != Some(&skills_state) {
                    self.skills_prompt = crate::plugins::skills_section_with_trust(
                        &self.cwd,
                        &self.data_dir,
                        self.trust.admits(&self.cwd),
                    );
                }
                let text = prompt::system_prompt(
                    &self.cwd,
                    true,
                    self.git_snapshot.as_deref(),
                    &self.date_frozen,
                    &self.agents_prompt,
                    &self.memory_prompt,
                    &self.skills_prompt,
                );
                self.prompt_text = Some(text.clone());
                self.prompt_memory_enabled = Some(memory_enabled);
                self.prompt_skills_state = Some(skills_state);
                text
            }
        };
        let system = ChatMsg::system(prompt_text.clone());
        let system_changed = self
            .history
            .first()
            .is_some_and(|message| message.content.as_deref() != Some(prompt_text.as_str()));
        if self.history.is_empty() {
            self.history.push(system);
        } else if self.history[0].role == "system" {
            self.history[0] = system;
        }
        if system_changed {
            // The old usage baseline described the previous rendered system
            // prompt; do not project it onto the new one.
            self.usage_anchor = None;
        }
        // Turn-boundary reminder (exec mode/plan toggle on first turn or on
        // switch, date rollover, AGENTS.md/memory changes): prepended to the
        // user message — injecting at the tail does not break the
        // system+history prefix cache, nor can it land between a
        // tool_use/tool_result pair; the actual user text and reminder state are
        // persisted below so resume does not have to reconstruct them.
        let fresh_agents = prompt::agents_md(&self.data_dir, &self.cwd);
        // The memory fresh copy is computed whenever memory was enabled at
        // session start (the frozen flag, not the frozen section — the
        // /remember-mid-session case needs the reminder exactly when the
        // frozen section is still empty). A session that started with memory
        // off never computes one: no injection, even after a stray
        // /remember wrote the file
        let fresh_memory = if self.memory_enabled {
            let root = crate::memory::workspace_memory_dir(&self.data_dir, &self.cwd);
            crate::memory::memory_section_raw(
                &root,
                &crate::memory::read_index(&root.join("MEMORY.md")),
            )
        } else {
            String::new()
        };
        let reminder = if notification_only {
            None
        } else {
            prompt::turn_reminder(
                self.permissions,
                self.plan_enabled,
                &self.id,
                &self.cwd,
                &self.state,
                &mut self.permissions_reminded,
                &self.date_frozen,
                &mut self.date_reminded,
                &self.agents_prompt,
                &fresh_agents,
                &mut self.agents_reminded,
                &self.memory_prompt,
                &fresh_memory,
                &mut self.memory_reminded,
            )
        };
        // @-referenced files are injected as pointers (same trade-off as
        // kimi-code): only the path (+ optional line range) is given; the model
        // reads the content on demand with Read — always fresh, a constant one
        // line, no prefix-cache damage
        if !notification_only {
            let mut user_text = pointer_file_references(&self.cwd, &content, &files);
            if let Some(reminder) = reminder {
                user_text = format!("{reminder}\n\n{user_text}");
            }
            if self.history.len() == 1 {
                // First message: seed the title (first 30 chars as fallback) and
                // generate a model title async; if manually renamed (title_custom),
                // neither overwrites
                let title = seed_title(&content);
                let id = self.id.clone();
                self.store
                    .lock()
                    .expect("store lock")
                    .update_session(&id, |meta| {
                        if !meta.title_custom {
                            meta.title = title;
                        }
                        meta.updated_at = now_secs();
                    });
                spawn_title_generation(&self.store, &self.id, &content, config, tx);
            }
            // Rollout stores the original text only (files go in a separate field;
            // the "referenced files" suffix is gone — the UI renders inline chips
            // from files, and on resume the model side converts them to pointer
            // lines via rebuild_history)
            // Pasted images (ZCode-style pipeline): compress → persist into the
            // session media dir → rollout records an ImageRef (no base64 stored) +
            // history gets a ChatImage; images that fail to compress are skipped
            // with a note in the text. File names continue the directory sequence
            // (next_media_index): naming by per-message index would be overwritten
            // by later turns
            let mut image_refs: Vec<crate::rollout::ImageRef> = Vec::new();
            let mut chat_images: Vec<pig_provider::ChatImage> = Vec::new();
            if !images.is_empty() {
                let media_dir =
                    pig_utils::paths::media_dir(&self.data_dir.join("sessions"), &self.id);
                let mut next = crate::rollout::next_media_index(&media_dir);
                for (ix, pending) in images.iter().enumerate() {
                    match crate::tool::compress_image_for_model(&pending.bytes, &pending.mime) {
                        Ok(comp) => {
                            let ext = if comp.media_type == "image/png" {
                                "png"
                            } else {
                                "jpg"
                            };
                            let file = media_dir.join(format!("{next}.{ext}"));
                            if let Err(error) = std::fs::create_dir_all(&media_dir)
                                .and_then(|()| std::fs::write(&file, &comp.bytes))
                            {
                                user_text.push_str(&format!(
                                    "\n[Image {} failed to save: {error}]",
                                    ix + 1
                                ));
                                continue;
                            }
                            // Compression note (kimi-code caption idea): if scaling/
                            // re-encoding changed the image, tell the model in the
                            // text; the original is persisted for ReadMediaFile
                            // region close-ups
                            if let Some(note) =
                                compression_note(ix + 1, pending, &comp, &media_dir, next)
                            {
                                user_text.push_str(&note);
                            }
                            next += 1;
                            image_refs.push(crate::rollout::ImageRef {
                                path: file,
                                media_type: comp.media_type.clone(),
                                width: comp.width,
                                height: comp.height,
                                label: Some(format!("Image {}", ix + 1)),
                            });
                            chat_images.push(pig_provider::ChatImage {
                                media_type: comp.media_type,
                                data_base64: crate::tool::base64_encode(&comp.bytes),
                                label: Some(format!("Image {}", ix + 1)),
                            });
                        }
                        Err(error) => {
                            user_text.push_str(&format!(
                                "\n[Image {} failed to compress: {error}]",
                                ix + 1
                            ));
                        }
                    }
                }
            }
            // Capability projection: model without image input → keep out of
            // ChatMsg.images and inform via a text placeholder (with the media path)
            let model_text = user_text.clone();
            let media_paths: Vec<std::path::PathBuf> =
                image_refs.iter().map(|r| r.path.clone()).collect();
            project_images(
                &mut user_text,
                &mut chat_images,
                &media_paths,
                config.input_image,
            );
            let mut user_msg = ChatMsg::user(std::mem::take(&mut user_text));
            user_msg.images = chat_images;
            let sent_text = user_msg.content.clone();
            let sent_images = if user_msg.images.is_empty() {
                Vec::new()
            } else {
                image_refs.clone()
            };
            self.history.push(user_msg);
            self.user_ixs.push(Some(self.history.len() - 1));
            // The event and the clean rollout fields carry display text; sent_* preserves the model-facing form.
            let nums = crate::rollout::image_nums(&image_refs);
            // The turn's first user message was accepted just before this point;
            // the stamp follows the record so a reload prints the same time
            let sent_at_ms = crate::model_io::now_ms();
            self.record(&RolloutRecord::User {
                text: content.clone(),
                files: files.clone(),
                images: image_refs,
                sent_at_ms,
                sent_text,
                sent_images,
                model_text: Some(model_text),
                sent_input_image: Some(config.input_image),
            });
            self.emit(
                |session_id| Event::UserMessage {
                    session_id,
                    text: content.clone(),
                    files: files.clone(),
                    image_nums: nums,
                    sent_at_ms,
                },
                tx,
            );
        }
        self.record_prompt_snapshot(&prompt_text);
        // Stashed hook contexts (SessionStart/UserPromptSubmit) enter history
        // right after the user message as model-visible HookContext records
        // (resume rebuilds them); the UI timeline shows nothing extra
        for context in std::mem::take(&mut self.hook_context_stash) {
            self.record(&RolloutRecord::HookContext {
                text: context.clone(),
            });
            self.history.push(ChatMsg::user(context));
        }
        // The user message, prompt snapshot and hook contexts must be durable
        // (real sync, not flush) before any model request sees them.
        self.sync_rollout();
        self.report_rollout_failure(tx);

        let mut step = 0usize;
        // Stop-hook continuations this turn (cap 3, ZCode semantics: the 4th
        // hook still runs but no longer continues)
        let mut stop_continuations = 0usize;
        // Anthropic pause_turn continuations this turn (cap 3).
        let mut pause_continuations = 0usize;
        // The next step follows a paused step and must compact without
        // dropping the paused assistant/server replay.
        let mut pause_continuation = false;
        // Index of the paused assistant message. A steer drained at the next
        // boundary starts a later segment; preserve-latest must retain this
        // segment (and therefore the server replay) plus that steer, never the
        // steer alone.
        let mut pause_pin: Option<usize> = None;
        loop {
            // ---- Steer boundary: inject everything the pending list
            // collected during the previous step. The previous step's tool
            // results are all settled here, so a user message can never land
            // between a tool_use and its tool_result ----
            self.drain_steer(&pending, tx);
            self.drain_ready_notifications(&notifications, tx);
            step += 1;
            match self
                .run_step(
                    turn_id.clone(),
                    step,
                    config,
                    pause_continuation,
                    pause_pin,
                    tx,
                    &control,
                )
                .await
            {
                StepOutcome::TextOnly { finish_reason } => {
                    // The paused step has now been answered; a later steer or
                    // Stop-hook continuation is a new request, not a pause
                    // continuation.
                    pause_continuation = false;
                    pause_pin = None;
                    // Incomplete responses end at a safe boundary rather than
                    // triggering automatic continuation through steer or Stop hooks.
                    let steered = self.drain_steer(&pending, tx) > 0;
                    let notified = self.drain_ready_notifications(&notifications, tx) > 0;
                    if finish_reason.is_none() && (steered || notified) {
                        continue;
                    }
                    if finish_reason.is_none() && self.hooks.is_active() {
                        let last_assistant = self
                            .history
                            .iter()
                            .rev()
                            .find(|msg| msg.role == "assistant" && msg.content.is_some())
                            .and_then(|msg| msg.content.clone())
                            .unwrap_or_default();
                        let payload = crate::hooks::HookPayload::stop(
                            &self.id,
                            &self.cwd,
                            self.permissions,
                            &last_assistant,
                            stop_continuations > 0,
                        )
                        .with_transcript(&self.history);
                        let run = self.hook_run_ctx(tx, cancel);
                        let effects = crate::hooks::run_event(
                            &self.hooks,
                            crate::hooks::HookEvent::Stop,
                            payload,
                            &run,
                        )
                        .await;
                        let contexts = effects.additional_contexts;
                        if effects.stop_signal && !contexts.is_empty() && stop_continuations < 3 {
                            stop_continuations += 1;
                            for context in contexts {
                                self.record(&RolloutRecord::HookContext {
                                    text: context.clone(),
                                });
                                self.history.push(ChatMsg::user(context));
                            }
                            continue;
                        }
                    }
                    let duration_ms = started.elapsed().as_millis() as u64;
                    let stats = self.flush_turn_stats(config, duration_ms, finish_reason);
                    // The turn's changes panel is emitted before the turn-end
                    // event (durable data before boundary events)
                    self.flush_turn_changes(tx);
                    self.sync_rollout();
                    self.report_rollout_failure(tx);
                    self.emit(
                        |session_id| Event::TurnComplete {
                            session_id,
                            duration_ms,
                            stats,
                            finish_reason,
                        },
                        tx,
                    );
                    self.touch_index();
                    return TurnOutcome::Completed;
                }
                StepOutcome::Paused => {
                    if pause_continuations >= MAX_PAUSE_CONTINUATIONS {
                        tracing::warn!(
                            limit = MAX_PAUSE_CONTINUATIONS,
                            "pause_turn continuation limit reached; failing the turn"
                        );
                        let error = pause_limit_error();
                        self.emit(
                            |session_id| Event::Error {
                                session_id: Some(session_id),
                                error,
                                fatal: true,
                            },
                            tx,
                        );
                        let duration_ms = started.elapsed().as_millis() as u64;
                        let _ = self.flush_turn_stats(config, duration_ms, None);
                        self.flush_turn_changes(tx);
                        self.sync_rollout();
                        self.report_rollout_failure(tx);
                        return TurnOutcome::Failed;
                    }
                    pause_continuations += 1;
                    pause_continuation = true;
                    // Pin the paused user segment: its last message is the
                    // paused assistant, or its completed local tool round
                    // when the pause also requested tools. Compaction retains
                    // the whole containing segment (server replay included).
                    pause_pin = Some(self.history.len().saturating_sub(1));
                    continue;
                }
                StepOutcome::ToolsExecuted => {
                    pause_continuation = false;
                    pause_pin = None;
                    continue;
                }
                StepOutcome::Ended { aborted } => {
                    // Interrupt/failure wrap-up: the completed steps' usage
                    // still counts toward the statistics (the rollout
                    // TurnStats doubles as the crash-recovery marker). The UI
                    // keeps its interrupted state (TurnAborted was already
                    // emitted), so the returned footer stats are dropped here
                    let duration_ms = started.elapsed().as_millis() as u64;
                    let _ = self.flush_turn_stats(config, duration_ms, None);
                    // Changes already made this turn still produce a panel
                    self.flush_turn_changes(tx);
                    self.sync_rollout();
                    self.report_rollout_failure(tx);
                    return if aborted {
                        TurnOutcome::Aborted
                    } else {
                        TurnOutcome::Failed
                    };
                }
            }
        }
    }

    /// Mint a unique internal tool item id. The provider's call id is kept for
    /// diagnostics, but the monotonic sequence (and the turn id) make the id
    /// unique even when the provider repeats a call id in one response.
    /// Approval/request ids derive from this value, so duplicates cannot alias.
    fn next_tool_item_id(&mut self, turn_id: &str, call_id: &str) -> String {
        self.tool_item_seq += 1;
        format!("{turn_id}-tool-{}-{call_id}", self.tool_item_seq)
    }

    /// Claim one item id per call in execution order. Streaming input cards
    /// are matched back to their call by provider id in provider-index order
    /// (FIFO, so duplicate ids keep their own preview); calls without a
    /// preview get a fresh id. Claimed previews are removed from `live` so the
    /// discard pass only sees broken/never-final calls.
    fn claim_tool_item_ids(
        &mut self,
        turn_id: &str,
        calls: &[ToolCall],
        live: &mut std::collections::HashMap<usize, (String, String)>,
    ) -> Vec<String> {
        let mut available: Vec<(usize, String, String)> = live
            .iter()
            .map(|(index, (id, item_id))| (*index, id.clone(), item_id.clone()))
            .collect();
        available.sort_by_key(|(index, _, _)| *index);
        let mut by_id: std::collections::HashMap<&str, VecDeque<usize>> =
            std::collections::HashMap::new();
        for (slot, (_, id, _)) in available.iter().enumerate() {
            by_id.entry(id.as_str()).or_default().push_back(slot);
        }
        let mut claimed = vec![false; available.len()];
        let mut item_ids = Vec::with_capacity(calls.len());
        for call in calls {
            let slot = by_id
                .get_mut(call.id.as_str())
                .and_then(VecDeque::pop_front);
            match slot {
                Some(slot) => {
                    claimed[slot] = true;
                    item_ids.push(available[slot].2.clone());
                }
                None => item_ids.push(self.next_tool_item_id(turn_id, &call.id)),
            }
        }
        for (slot, (index, _, _)) in available.iter().enumerate() {
            if claimed[slot] {
                live.remove(index);
            }
        }
        item_ids
    }

    /// Adds one entered step's model time and first-token wait to the turn
    /// totals. Called exactly once per step on every exit that opened a request
    /// cycle — success, failure and cancellation alike, so an interrupted step
    /// still shows up in the turn's model time, TTFT and speed.
    fn charge_step_time(&mut self, started: Instant, first_token: Option<Instant>) {
        let elapsed_ms = started.elapsed().as_millis() as u64;
        self.turn_api_ms += elapsed_ms;
        self.turn_ttft_ms += first_token
            .map(|at| at.duration_since(started).as_millis() as u64)
            .unwrap_or(0)
            .min(elapsed_ms);
        if first_token.is_some() {
            self.turn_ttft_steps += 1;
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_step(
        &mut self,
        turn_id: String,
        step: usize,
        config: &ResolvedModel,
        preserve_latest: bool,
        pause_pin: Option<usize>,
        tx: &async_channel::Sender<Event>,
        control: &pig_provider::CallControl,
    ) -> StepOutcome {
        let cancel = control.cancel_token();
        let media = tool::MediaCapabilities::new(config.input_image, config.input_pdf);
        // Root session tool set = built-in + Agent/AgentSwarm + MCP + plugins.
        // Schemas are built before preflight because they are part of the
        // request budget.
        self.ensure_mcp_connected().await;
        self.select_plugin_tools(config, tx).await;
        let schemas = self.root_schemas();
        // Tool schemas are priced separately from the history (they are not
        // surface nodes); the figure stays cached for the post-reply refresh
        let tools_tokens = super::estimate::estimate_schemas_tokens(&schemas);
        // Context preflight: project the next request from the latest usage
        // anchor (plus messages/schema growth) and compact before sampling.
        let projected = self.projected_context_tokens(config, tools_tokens);
        let trigger = compact_trigger(config);
        if projected > trigger {
            match self
                .run_compact_with_current_tools(
                    Some(config),
                    CompactTrigger::Watermark,
                    None,
                    preserve_latest,
                    pause_pin,
                    tx,
                    cancel,
                )
                .await
            {
                CompactOutcome::Cancelled => return StepOutcome::Ended { aborted: true },
                CompactOutcome::Completed | CompactOutcome::Skipped | CompactOutcome::Failed => {}
            }
        }

        let text_item = format!("{turn_id}-text-{step}");
        let reasoning_item = format!("{turn_id}-reason-{step}");
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut replay;
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut finish_reason;
        let mut incomplete;
        // Live pending-tool cards keyed by the provider's stream slot. Chat
        // slots are logical call IDs; Anthropic/Responses retain wire indices.
        let mut live_tool_inputs: std::collections::HashMap<usize, (String, String)> =
            std::collections::HashMap::new();
        // The current attempt's real usage. A stale previous value must never
        // be attached to the new history length.
        let attempt_usage: Option<u64>;
        // Internal tool ids of the successful attempt's calls, in call order.
        // Allocated before the streaming-input map is drained so an executed
        // card keeps the id of its live input preview.
        let mut step_item_ids: Vec<String> = Vec::new();
        // The step's request-cycle clock: TTFT and model time both run from
        // here (the harness's `step/start` basis). Pre-request maintenance —
        // MCP connect, schema building, a preflight compaction — happens above
        // and is not model time.
        let step_started = Instant::now();
        // At most one reactive recovery per turn: a durable overflow
        // compaction plus a retry of this same step.
        // Earliest first token across the step's attempts: a retry keeps the
        // first attempt's token time (the harness's rule); the per-attempt
        // value inside the loop only feeds the wire trace.
        let mut step_first_token_at: Option<Instant> = None;
        'attempt: loop {
            text.clear();
            reasoning.clear();
            replay = None;
            tool_calls.clear();
            finish_reason = pig_protocol::FinishReason::Stop;
            live_tool_inputs.clear();
            // Whether any text/reasoning/tool/replay content reached the UI or
            // history for this attempt. A retry is only safe when false.
            let mut forwarded_content = false;
            let mut failure: Option<CoreError> = None;
            let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
            let api_started = Instant::now();
            let api_started_ms = crate::model_io::now_ms();
            // Model-io trace input projection: snapshot before the request
            // (media recorded as counts; only tool-result content is capped
            // unless full retention is on). Persisting stores only the delta:
            // take the common prefix against the previous full projection,
            // offset + delta (aligned with ZCode model-io)
            let io_full_retention = self
                .config_snapshot()
                .is_some_and(|c| c.model_io_full_retention);
            let io_input_full = crate::model_io::project_input(
                &self.history,
                (!io_full_retention).then_some(crate::model_io::TOOL_RESULT_MAX_CHARS),
            );
            let io_offset = crate::model_io::common_prefix_len(&io_input_full, &self.io_last_input);
            let io_input: Vec<_> = io_input_full[io_offset..].to_vec();
            // Composition of the prompt this request is about to send (the
            // context panel's rows)
            let breakdown =
                super::estimate::context_breakdown_with_tools(&self.history, tools_tokens);
            self.emit(
                |session_id| Event::ContextBreakdownChanged {
                    session_id,
                    breakdown,
                },
                tx,
            );
            let mut request_config = config.clone();
            // Stable session-routing key; retries and restarts reuse it because
            // it depends only on the session id.
            if pig_provider::prompt_cache_key_supported(&config.base_url) {
                request_config.prompt_cache_key =
                    Some(pig_provider::session_prompt_cache_key(&self.id));
            }
            let provider_task = tokio::spawn(pig_provider::stream_chat(
                request_config,
                self.history.clone(),
                schemas.clone(),
                event_tx,
                control.clone(),
            ));

            // When the first output token (reasoning/text delta) arrives: TTFT
            // = that moment - request sent
            let mut first_token_at: Option<Instant> = None;
            // This attempt's usage and failure reason (for the persisted trace)
            let mut step_usage = crate::model_io::ModelIoUsage::default();
            let mut step_error: Option<String> = None;
            let mut api_tracker = crate::api_usage::ApiCallTracker::default();

            loop {
                let event = tokio::select! {
                    event = event_rx.recv() => event,
                    _ = cancel.cancelled() => None,
                };
                if let Some(event) = &event {
                    api_tracker.observe(event);
                }
                match event {
                    Some(ProviderEvent::Reasoning(delta)) => {
                        let at = Instant::now();
                        first_token_at.get_or_insert(at);
                        step_first_token_at.get_or_insert(at);
                        forwarded_content = true;
                        reasoning.push_str(&delta);
                        self.emit(
                            |session_id| Event::ReasoningDelta {
                                session_id,
                                item_id: reasoning_item.clone(),
                                delta,
                            },
                            tx,
                        );
                    }
                    Some(ProviderEvent::Text(delta)) => {
                        let at = Instant::now();
                        first_token_at.get_or_insert(at);
                        step_first_token_at.get_or_insert(at);
                        forwarded_content = true;
                        text.push_str(&delta);
                        self.emit(
                            |session_id| Event::TextDelta {
                                session_id,
                                item_id: text_item.clone(),
                                delta,
                            },
                            tx,
                        );
                    }
                    // Live pending-tool cards (arguments still streaming):
                    // forward as live-only ToolInput events under a unique
                    // per-invocation item id. The execution pass claims the
                    // same id back by provider id, so the UI's pending card
                    // becomes the executing card in place even when the
                    // provider repeats a call id.
                    Some(ProviderEvent::ToolCallStart { index, id, name }) => {
                        // A streamed tool call is model output like any other:
                        // its name/first arguments settle the first-token time
                        // (harness parity — a pure tool-call step still has a
                        // TTFT).
                        let at = Instant::now();
                        first_token_at.get_or_insert(at);
                        step_first_token_at.get_or_insert(at);
                        forwarded_content = true;
                        let item_id = live_tool_inputs
                            .get(&index)
                            .map(|(_, item_id)| item_id.clone())
                            .unwrap_or_else(|| self.next_tool_item_id(&turn_id, &id));
                        self.emit(
                            |session_id| Event::ToolInputStart {
                                session_id,
                                item_id: item_id.clone(),
                                tool: name,
                            },
                            tx,
                        );
                        live_tool_inputs.insert(index, (id, item_id));
                    }
                    Some(ProviderEvent::ToolCallDelta { index, delta }) => {
                        if !delta.is_empty() {
                            let at = Instant::now();
                            first_token_at.get_or_insert(at);
                            step_first_token_at.get_or_insert(at);
                        }
                        if let Some((_, item_id)) = live_tool_inputs.get(&index) {
                            let item_id = item_id.clone();
                            self.emit(
                                |session_id| Event::ToolInputDelta {
                                    session_id,
                                    item_id,
                                    delta,
                                },
                                tx,
                            );
                        }
                    }
                    Some(ProviderEvent::AssistantReplay(snapshot)) => {
                        if !snapshot.items.is_empty() {
                            forwarded_content = true;
                        }
                        replay = Some(snapshot);
                    }
                    Some(ProviderEvent::ToolCalls(calls)) => {
                        forwarded_content = true;
                        tool_calls = calls;
                    }
                    Some(ProviderEvent::ResponseMetadata(_)) => {}
                    // Retry status is live-rendered; api_tracker independently
                    // counts it and drops the previous attempt's response metadata.
                    Some(ProviderEvent::Retrying(notice)) => {
                        let status = retry_status(&notice);
                        self.emit(|session_id| Event::RetryStatus { session_id, status }, tx);
                    }
                    Some(ProviderEvent::Usage {
                        input,
                        cache_read,
                        output,
                        used,
                        total,
                        reasoning_output,
                    }) => {
                        self.turn_input += input;
                        self.turn_cache_read += cache_read;
                        self.turn_output += output;
                        self.turn_reasoning_output += reasoning_output;
                        self.input_total += input;
                        self.cache_read_total += cache_read;
                        self.last_total_tokens = Some(used);
                        step_usage = crate::model_io::ModelIoUsage {
                            input,
                            cache_read,
                            output,
                            used,
                            total,
                            reasoning_output,
                        };
                        // Per-request usage is persisted immediately (durable before
                        // events); replay restores the watermark from the last
                        // record
                        self.record(&RolloutRecord::StepUsage {
                            input,
                            cache_read,
                            output,
                            used,
                            turn_id: self.usage_turn_id.clone(),
                        });
                        let (input_total, cache_read_total) =
                            (self.input_total, self.cache_read_total);
                        self.emit(
                            |session_id| Event::ContextUsage {
                                session_id,
                                used,
                                total,
                                cache_read_total,
                                input_total,
                            },
                            tx,
                        );
                    }
                    Some(ProviderEvent::Finished(reason)) => {
                        finish_reason = reason;
                        break;
                    }
                    None => {
                        if cancel.is_cancelled() {
                            break;
                        }
                        let error = pig_protocol::CoreError::Internal {
                            detail: "Provider event stream ended without a finish reason"
                                .to_string(),
                        };
                        step_error = Some(pig_provider::core_error_en(&error));
                        // The typed error only reaches the UI after the reactive
                        // overflow decision (retry or final failure).
                        failure = Some(error);
                        break;
                    }
                    Some(ProviderEvent::Failed(error)) => {
                        step_error = Some(pig_provider::core_error_en(&error));
                        failure = Some(error);
                        break;
                    }
                }
            }
            // Request-scoped span for the wire trace (an attempt is one
            // request); the step-scoped figures are added once after the
            // attempt loop so a retry cannot count the step twice.
            let api_elapsed = api_started.elapsed();
            self.turn_api_steps += 1;
            let request_ttft_ms = first_token_at
                .map(|at| at.duration_since(api_started).as_millis() as u64)
                .unwrap_or(0)
                .min(api_elapsed.as_millis() as u64);

            // Persist the model-io trace (failures/cancellations recorded too): the
            // UI's "view model io" reads this file directly. This debug trace is
            // best-effort; unlike the durable rollout, its write failures only
            // warn and do not stop the turn.
            incomplete = finish_reason.is_incomplete();
            let io_finish = if failure.is_some() {
                "error"
            } else if cancel.is_cancelled() {
                "cancelled"
            } else {
                finish_reason.as_str()
            };
            let mut api_record = crate::api_usage::ApiUsageRecord::new(
                &self.id,
                "main",
                config,
                api_started_ms,
                api_elapsed.as_millis() as u64,
            );
            api_record.turn_id = Some(turn_id.clone());
            api_record.step = Some(step as u32);
            api_record.ttft_ms = first_token_at.is_some().then_some(request_ttft_ms);
            api_record.finish = io_finish.to_string();
            api_record.error = step_error.clone();
            api_tracker.apply_to(&mut api_record);
            self.store
                .lock()
                .expect("store lock")
                .record_api_usage(&api_record);
            let attempt_used = step_usage.used;

            // A successful attempt owns the final calls: claim one internal
            // item id per invocation, reusing the streaming preview's id by
            // provider call id (FIFO) so the card does not split. Failed,
            // cancelled and incomplete attempts leave the previews to the
            // discard pass below.
            if failure.is_none() && !cancel.is_cancelled() && !incomplete {
                step_item_ids =
                    self.claim_tool_item_ids(&turn_id, &tool_calls, &mut live_tool_inputs);
            }

            let mut io_record = crate::model_io::ModelIoRecord {
                ts_ms: crate::model_io::now_ms(),
                turn: format!("{turn_id}-s{step}"),
                source: "main".into(),
                provider: config.provider_name.clone(),
                model: config.model.clone(),
                duration_ms: api_elapsed.as_millis() as u64,
                ttft_ms: request_ttft_ms,
                usage: step_usage,
                finish: io_finish.into(),
                error: step_error,
                reasoning: reasoning.clone(),
                text: text.clone(),
                tool_calls: crate::model_io::project_tool_calls(&tool_calls),
                input_offset: io_offset,
                input: io_input,
                file_reset: false,
            };
            if let Err(e) = crate::model_io::append(
                &self.data_dir.join("sessions"),
                &self.id,
                &mut io_record,
                &io_input_full,
                crate::model_io::MAX_SESSION_BYTES,
                io_full_retention,
            ) {
                tracing::warn!("Failed to write model io trace (ignored): {e}");
            }
            // This entry's full projection becomes the delta baseline for the next
            // one
            self.io_last_input = io_input_full;

            for (_, (id, item_id)) in live_tool_inputs.drain() {
                let discard = failure.is_some()
                    || incomplete
                    || cancel.is_cancelled()
                    || !tool_calls.iter().any(|call| call.id == id);
                if discard {
                    self.emit(
                        |session_id| Event::ToolInputDiscard {
                            session_id,
                            item_id,
                        },
                        tx,
                    );
                }
            }
            if incomplete {
                tool_calls.clear();
                if let Some(replay) = replay.as_mut() {
                    replay.remove_tool_calls();
                }
            }

            if let Some(error) = failure.as_ref() {
                provider_task.abort();
                let overflow = matches!(error, CoreError::ContextWindowExceeded { .. });
                if overflow && !forwarded_content && !self.overflow_recovered {
                    tracing::warn!(
                        step,
                        "context window exceeded before any content was forwarded; compacting and retrying this step once"
                    );
                    // Compare the same estimate before and after: the provider's
                    // reported usage can be lower than the conservative estimate,
                    // so it cannot prove that this rewrite actually freed room.
                    let used_before =
                        super::estimate::estimate_history_tokens(&self.history) + tools_tokens;
                    match self
                        .run_compact_with_current_tools(
                            Some(config),
                            CompactTrigger::Overflow,
                            None,
                            preserve_latest,
                            pause_pin,
                            tx,
                            cancel,
                        )
                        .await
                    {
                        CompactOutcome::Completed => {
                            let used_after = self.last_total_tokens.unwrap_or(u64::MAX);
                            if used_after < used_before {
                                self.overflow_recovered = true;
                                continue 'attempt;
                            }
                            tracing::warn!(
                                used_before,
                                used_after,
                                "overflow compaction did not shrink the context; surfacing the error"
                            );
                        }
                        CompactOutcome::Cancelled => {
                            self.charge_step_time(step_started, step_first_token_at);
                            return StepOutcome::Ended { aborted: true };
                        }
                        CompactOutcome::Skipped | CompactOutcome::Failed => {
                            tracing::warn!(
                                "context overflow cannot be compacted (no compactable prefix); surfacing the error"
                            );
                        }
                    }
                }
                self.emit(
                    |session_id| Event::Error {
                        session_id: Some(session_id),
                        error: error.clone(),
                        fatal: true,
                    },
                    tx,
                );
                self.charge_step_time(step_started, step_first_token_at);
                return StepOutcome::Ended { aborted: false };
            }
            if cancel.is_cancelled() {
                provider_task.abort();
                self.emit(|session_id| Event::TurnAborted { session_id }, tx);
                self.charge_step_time(step_started, step_first_token_at);
                return StepOutcome::Ended { aborted: true };
            }
            let _ = provider_task.await;
            attempt_usage = (attempt_used > 0).then_some(attempt_used);
            break 'attempt;
        }

        // Step-scoped model time and TTFT, added once per step (a retry cannot
        // double count; failed and cancelled exits charge from their own
        // branches).
        self.charge_step_time(step_started, step_first_token_at);

        // Some Responses streams deliver the assistant message only as the
        // authoritative `output_item.done` item (no text deltas). The durable
        // replay already carries that text; use it so the live answer, the
        // TextDone boundary and its sources are not lost.
        if text.is_empty()
            && let Some(replay) = replay.as_ref()
        {
            text = replay
                .items
                .iter()
                .filter_map(|item| match item {
                    pig_provider::AssistantReplayItem::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
        }
        let reasoning = Some(reasoning).filter(|reasoning| !reasoning.is_empty());
        // The durable assistant record is written before the TextDone boundary:
        // a crash after TextDone can never lose a reply the UI already showed.
        self.record(&RolloutRecord::Assistant {
            text: text.clone(),
            reasoning: reasoning.clone(),
            replay: replay.clone(),
            tool_calls: tool_calls.iter().map(ToolCall::to_wire).collect(),
        });
        if !text.is_empty() {
            self.emit(
                |session_id| Event::TextDone {
                    session_id,
                    item_id: text_item.clone(),
                    full_text: text.clone(),
                },
                tx,
            );
        }
        // Normalized hosted-search sources come from the durable replay, after
        // the assistant record is written. The text item id is the same one
        // already carried by TextDone.
        let sources = replay
            .as_ref()
            .map(pig_provider::replay_sources)
            .unwrap_or_default();
        if !text.is_empty() && !sources.is_empty() {
            self.emit(
                |session_id| Event::Sources {
                    session_id,
                    item_id: text_item.clone(),
                    sources,
                },
                tx,
            );
        }
        self.history.push(ChatMsg::assistant_with_replay(
            text,
            tool_calls.clone(),
            reasoning,
            replay,
        ));
        // The next request's preflight baseline is this persisted reply step.
        // Only this attempt's real Usage may be attached to the new history
        // length; a missing/zero Usage clears the anchor and preflight falls
        // back to the conservative estimate.
        self.usage_anchor = attempt_usage.map(|used| UsageAnchor {
            model_key: model_key(config),
            history_len: self.history.len(),
            tools_tokens,
            used,
            system_fingerprint: system_fingerprint(&self.history),
        });
        // The reply is part of the surface the next request will send: refresh
        // the composition now (the provider sample cannot see it yet)
        let breakdown = super::estimate::context_breakdown_with_tools(&self.history, tools_tokens);
        self.emit(
            |session_id| Event::ContextBreakdownChanged {
                session_id,
                breakdown,
            },
            tx,
        );
        let paused = finish_reason == pig_protocol::FinishReason::Pause;
        if tool_calls.is_empty() {
            if paused {
                return StepOutcome::Paused;
            }
            return StepOutcome::TextOnly {
                finish_reason: incomplete.then_some(finish_reason),
            };
        }

        let item_ids = step_item_ids;
        debug_assert_eq!(
            item_ids.len(),
            tool_calls.len(),
            "every final call owns one internal item id"
        );
        let tools = self.root_tools();
        // P0 grouped concurrency: consecutive "safe to run concurrently"
        // read-only calls form a parallel group (JoinSet, cap 8); non-
        // concurrentable calls are sync points — after the preceding group
        // drains, they take the original serial path below (interception
        // semantics unchanged)
        let mask = parallel_mask(
            &tool_calls,
            &tools,
            self.permissions,
            media,
            &self.permission_rules,
            &self.state,
            &self.cwd,
            self.hooks.is_active(),
        );
        let mut next_ix = 0usize;
        for (call_ix, call) in tool_calls.iter().enumerate() {
            if call_ix < next_ix {
                continue; // already executed with an earlier parallel group
            }
            if mask[call_ix] {
                let mut group_end = call_ix + 1;
                while group_end < tool_calls.len() && mask[group_end] {
                    group_end += 1;
                }
                next_ix = group_end;
                // Cancellation wrap-up (receipts + TurnAborted) happens inside
                // the group; false means the turn ends
                if !self
                    .run_parallel_group(
                        &tool_calls,
                        &item_ids,
                        call_ix,
                        group_end,
                        &turn_id,
                        media,
                        config.context_window,
                        self.trust.admits(&self.cwd),
                        tx,
                        cancel,
                    )
                    .await
                {
                    return StepOutcome::Ended { aborted: true };
                }
                continue;
            }
            let item_id = item_ids[call_ix].clone();
            let summary = tool::summarize(call);
            // ExitPlanMode's begin is emitted inside the interception block
            // (detail rewritten with the effective plan — when the argument is
            // omitted, core reads the plan file; the UI plan card and replay
            // restore share one data source)
            if call.name != "ExitPlanMode" {
                let detail = serde_json::from_str::<serde_json::Value>(&call.arguments)
                    .map(|v| serde_json::to_string_pretty(&v).unwrap_or_default())
                    .unwrap_or_else(|_| call.arguments.clone());
                self.emit(
                    |session_id| Event::ToolCallBegin {
                        session_id,
                        item_id: item_id.clone(),
                        tool: call.name.clone(),
                        input_summary: summary.clone(),
                        detail,
                    },
                    tx,
                );
            }

            let tool_ref = tools.iter().find(|t| t.name() == call.name.as_str());
            let read_only = tool_ref.is_some_and(|t| t.read_only());

            // ExitPlanMode: intercepted before the plan-mode hard rejection (it
            // is the only way out of plan mode; force a popup for user
            // confirmation; reuses the ApprovalRequested channel so the UI
            // needs no new component). kimi semantics: the plan argument is
            // optional — when omitted, core reads the plan file
            if call.name == "ExitPlanMode" {
                let args: serde_json::Value =
                    serde_json::from_str(&call.arguments).unwrap_or_default();
                let mut plan = args["plan"].as_str().unwrap_or("").to_string();
                if plan.trim().is_empty() {
                    plan = read_plan_file(&self.cwd, &self.id).unwrap_or_default();
                }
                // begin is rewritten with the effective plan (replay restores
                // the same full text via the rollout arguments)
                let enriched_args = serde_json::json!({ "plan": plan }).to_string();
                let detail = serde_json::to_string_pretty(
                    &serde_json::from_str::<serde_json::Value>(&enriched_args).unwrap_or_default(),
                )
                .unwrap_or_default();
                self.emit(
                    |session_id| Event::ToolCallBegin {
                        session_id,
                        item_id: item_id.clone(),
                        tool: call.name.clone(),
                        input_summary: summary.clone(),
                        detail,
                    },
                    tx,
                );
                let (note, is_error);
                if !self.plan_enabled {
                    note = "Only available in plan mode.".to_string();
                    is_error = true;
                } else if plan.trim().is_empty() {
                    // Same as kimi exitPlanModeTool: when the plan file is empty
                    // or missing, no approval popup — guide the model to write
                    // the plan file first
                    note = format!(
                        "The plan file is empty or missing: write the plan to `.pigcode/plans/plan-{}.md` with Write first, then call ExitPlanMode.",
                        self.id
                    );
                    is_error = true;
                } else {
                    let request_id = format!("{}-{turn_id}-exitplan-{item_id}", self.id);
                    let (reply_tx, reply_rx) = oneshot::channel();
                    // Plan confirmation is a one-shot per-request popup; it is
                    // never coalesced with another approval
                    self.pending
                        .lock()
                        .expect("pending lock")
                        .insert(request_id.clone(), reply_tx);
                    // kimi semantics: persist the plan before requesting
                    // approval (rewriting the same content on approval is
                    // idempotent; after rejection the file stays, and the next
                    // ExitPlanMode overwrites it once revised)
                    write_plan_file(&self.cwd, &self.id, &plan);
                    // The full plan goes into the popup (the kimi plan approval
                    // panel has its own title; detail = the plain plan full
                    // text — truncation would hide the full text from the user
                    // before approval)
                    self.emit(
                        |session_id| Event::ApprovalRequested {
                            session_id,
                            request_id: request_id.clone(),
                            tool: call.name.clone(),
                            detail: plan.to_string(),
                            danger_key: None,
                            requested_write_roots: Vec::new(),
                            available_decisions: vec![
                                ApprovalDecision::Allow,
                                ApprovalDecision::Reject,
                            ],
                        },
                        tx,
                    );
                    let (decision, feedback) = tokio::select! {
                            reply = reply_rx => reply.unwrap_or((ApprovalDecision::Reject, None)),
                            _ = cancel.cancelled() => {
                                self.pending.lock().expect("pending lock").remove(&request_id);
                                self.settle_cancelled_tool(
                                    CancelledTool {
                                        call,
                                        summary,
                                        item_id: &item_id,
                                        turn_id: &turn_id,
                                        rest: &tool_calls[call_ix + 1..],
                                        rest_item_ids: &item_ids[call_ix + 1..],
                                        card: None,
                                        cards: vec![],
                                    },
                                    tx,
                                );
                                self.emit(|session_id| Event::TurnAborted { session_id }, tx);
                    return StepOutcome::Ended { aborted: true };
                            }
                        };
                    match decision {
                        ApprovalDecision::Allow
                        | ApprovalDecision::ApprovedForProject
                        | ApprovalDecision::ApprovedForSession => {
                            // Approved = turn off the plan toggle + persist the
                            // plan (already written before the popup; idempotent
                            // overwrite here). Exiting only flips the plan
                            // toggle — the exec mode is an independent dimension
                            // and stays as-is
                            write_plan_file(&self.cwd, &self.id, &plan);
                            self.plan_enabled = false;
                            let session_id = self.id.clone();
                            self.store.lock().expect("store lock").update_session(
                                &session_id,
                                |m| {
                                    m.plan_enabled = false;
                                },
                            );
                            self.emit(
                                |session_id| Event::PlanModeChanged {
                                    session_id,
                                    enabled: false,
                                    deferred: false,
                                },
                                tx,
                            );
                            note = "Plan approved; plan mode is now off. Start executing the plan."
                                .to_string();
                            is_error = false;
                        }
                        ApprovalDecision::Reject => {
                            // kimi Revise: a rejection may carry feedback the
                            // model uses to revise and resubmit
                            note = match feedback.filter(|f| !f.trim().is_empty()) {
                                Some(f) => format!(
                                    "The user declined to exit plan mode. Feedback: {}\nRevise the plan accordingly and resubmit.",
                                    f.trim()
                                ),
                                None => {
                                    "The user declined to exit plan mode. Continue refining the plan or answer open questions.".to_string()
                                }
                            };
                            is_error = true;
                        }
                    }
                }
                self.history
                    .push(ChatMsg::tool_result(&call.id, note.clone()).with_error(is_error));
                self.record(&RolloutRecord::ToolCall {
                    tool: call.name.clone(),
                    summary,
                    // Persist the effective plan (file content when the
                    // argument is omitted): replay restores the plan card's full
                    // text from it
                    arguments: enriched_args,
                    output: note.clone(),
                    is_error,
                    stopped: false,
                    edit: None,
                    agent_card: None,
                    agent_cards: vec![],
                    images: vec![],
                    documents: vec![],
                    delivered_notification_ids: vec![],
                });
                self.emit(
                    |session_id| Event::ToolCallEnd {
                        session_id,
                        item_id,
                        output: note,
                        is_error,
                        stopped: false,
                        edit: None,
                    },
                    tx,
                );
                continue;
            }

            // EnterPlanMode: entering plan mode is self-tightening (read-only),
            // so switch directly without a popup. The plan toggle is orthogonal
            // to the exec mode — only plan_enabled flips; the permission tier
            // stays.
            if call.name == "EnterPlanMode" {
                let (note, is_error) = if self.plan_enabled {
                    (
                        "Already in plan mode; continue researching and write the plan."
                            .to_string(),
                        false,
                    )
                } else {
                    self.plan_enabled = true;
                    let session_id = self.id.clone();
                    self.store
                        .lock()
                        .expect("store lock")
                        .update_session(&session_id, |m| {
                            m.plan_enabled = true;
                        });
                    self.emit(
                        |session_id| Event::PlanModeChanged {
                            session_id,
                            enabled: true,
                            deferred: false,
                        },
                        tx,
                    );
                    (
                        format!(
                            "Plan mode is on. Research with read-only tools from here; when the plan is ready, write it with Write to the plan file `.pigcode/plans/plan-{}.md` (the only writable path), then call ExitPlanMode to ask the user to confirm execution.",
                            self.id
                        ),
                        false,
                    )
                };
                self.history
                    .push(ChatMsg::tool_result(&call.id, note.clone()).with_error(is_error));
                self.record(&RolloutRecord::ToolCall {
                    tool: call.name.clone(),
                    summary,
                    arguments: call.arguments.clone(),
                    output: note.clone(),
                    is_error,
                    stopped: false,
                    edit: None,
                    agent_card: None,
                    agent_cards: vec![],
                    images: vec![],
                    documents: vec![],
                    delivered_notification_ids: vec![],
                });
                self.emit(
                    |session_id| Event::ToolCallEnd {
                        session_id,
                        item_id,
                        output: note,
                        is_error,
                        stopped: false,
                        edit: None,
                    },
                    tx,
                );
                continue;
            }

            // Agent: delegate a subagent (synchronous, foreground).
            // Intercepted before the ReadMediaFile gate and the plan hard
            // rejection — the plan rejection message comes from inside
            // run_subagent (more fitting than the generic hard rejection).
            // Child tool calls emit no top-level ToolCallBegin/End: the parent
            // timeline has only one Agent card; live progress goes through
            // SubagentProgress, approvals still pop up (the subagent's writes
            // pass the approval gate themselves).
            if call.name == "Agent" {
                match self
                    .run_subagent(call, &turn_id, &item_id, config, tx, cancel)
                    .await
                {
                    SubagentOutcome::Finished {
                        note,
                        is_error,
                        card,
                        cards,
                    } => {
                        self.history.push(
                            ChatMsg::tool_result(&call.id, note.clone()).with_error(is_error),
                        );
                        self.record(&RolloutRecord::ToolCall {
                            tool: call.name.clone(),
                            summary,
                            arguments: call.arguments.clone(),
                            output: note.clone(),
                            is_error,
                            stopped: false,
                            edit: None,
                            // Agent card metadata is persisted with the record:
                            // replay rebuilds the agent card from it
                            agent_card: card,
                            agent_cards: cards,
                            images: vec![],
                            documents: vec![],
                            delivered_notification_ids: vec![],
                        });
                        self.emit(
                            |session_id| Event::ToolCallEnd {
                                session_id,
                                item_id,
                                output: note,
                                is_error,
                                stopped: false,
                                edit: None,
                            },
                            tx,
                        );
                        continue;
                    }
                    SubagentOutcome::Cancelled { card, cards } => {
                        self.settle_cancelled_tool(
                            CancelledTool {
                                call,
                                summary,
                                item_id: &item_id,
                                turn_id: &turn_id,
                                rest: &tool_calls[call_ix + 1..],
                                rest_item_ids: &item_ids[call_ix + 1..],
                                card,
                                cards,
                            },
                            tx,
                        );
                        self.emit(|session_id| Event::TurnAborted { session_id }, tx);
                        return StepOutcome::Ended { aborted: true };
                    }
                }
            }

            // AgentSwarm: batch parallel subagents. By default it blocks in the
            // foreground until all finish; with run_in_background it dispatches
            // each to the background and returns receipts immediately
            // (terminal facts reach the parent through TaskNotificationBatch).
            // Intercepted at the same point as Agent — the plan rejection
            // message comes from inside run_swarm; each subagent's writes still
            // pass the approval gate.
            if call.name == "AgentSwarm" {
                match self.run_swarm(call, &item_id, config, tx, cancel).await {
                    SubagentOutcome::Finished {
                        note,
                        is_error,
                        card,
                        cards,
                    } => {
                        self.history.push(
                            ChatMsg::tool_result(&call.id, note.clone()).with_error(is_error),
                        );
                        self.record(&RolloutRecord::ToolCall {
                            tool: call.name.clone(),
                            summary,
                            arguments: call.arguments.clone(),
                            output: note.clone(),
                            is_error,
                            stopped: false,
                            edit: None,
                            agent_card: card,
                            agent_cards: cards,
                            images: vec![],
                            documents: vec![],
                            delivered_notification_ids: vec![],
                        });
                        self.emit(
                            |session_id| Event::ToolCallEnd {
                                session_id,
                                item_id,
                                output: note,
                                is_error,
                                stopped: false,
                                edit: None,
                            },
                            tx,
                        );
                        continue;
                    }
                    SubagentOutcome::Cancelled { card, cards } => {
                        self.settle_cancelled_tool(
                            CancelledTool {
                                call,
                                summary,
                                item_id: &item_id,
                                turn_id: &turn_id,
                                rest: &tool_calls[call_ix + 1..],
                                rest_item_ids: &item_ids[call_ix + 1..],
                                card,
                                cards,
                            },
                            tx,
                        );
                        self.emit(|session_id| Event::TurnAborted { session_id }, tx);
                        return StepOutcome::Ended { aborted: true };
                    }
                }
            }

            // ReadMediaFile capability gate: when the current model supports
            // neither image nor PDF input, guide toward switching models (no
            // execution, no approval popup; always visible in schemas, and
            // calling it triggers the guidance). A one-sided capability is
            // projected per file after execution.
            if call.name == "ReadMediaFile" && !media.any() {
                let note =
                    "The current model supports neither image nor PDF input; switch the model or enable the media-input capability in Settings.".to_string();
                self.history
                    .push(ChatMsg::tool_result(&call.id, note.clone()).with_error(true));
                self.record(&RolloutRecord::ToolCall {
                    tool: call.name.clone(),
                    summary,
                    arguments: call.arguments.clone(),
                    output: note.clone(),
                    is_error: true,
                    stopped: false,
                    edit: None,
                    agent_card: None,
                    agent_cards: vec![],
                    images: vec![],
                    documents: vec![],
                    delivered_notification_ids: vec![],
                });
                self.emit(
                    |session_id| Event::ToolCallEnd {
                        session_id,
                        item_id,
                        output: note,
                        is_error: true,
                        stopped: false,
                        edit: None,
                    },
                    tx,
                );
                continue;
            }

            // Plan-mode hard rejection (allowlist-based, stricter than
            // kimi-code's blocklist): read-only tools + plan file writes (kimi
            // writesOnlyPlanFile) pass; all other modifying tools are rejected
            // outright — independent of the permission tier, "full access +
            // plan" is rejected too
            if self.plan_enabled
                && !read_only
                && !tool::is_plan_file_write(&self.cwd, &call.arguments)
            {
                let note = format!(
                    "Plan mode: modifying tools are disabled (the only exception is writing the plan file `.pigcode/plans/plan-{}.md`). Research with read-only tools, write the plan into the plan file, then call ExitPlanMode to ask the user for confirmation.",
                    self.id
                );
                self.history
                    .push(ChatMsg::tool_result(&call.id, note.clone()).with_error(true));
                self.record(&RolloutRecord::ToolCall {
                    tool: call.name.clone(),
                    summary,
                    arguments: call.arguments.clone(),
                    output: note.clone(),
                    is_error: true,
                    stopped: false,
                    edit: None,
                    agent_card: None,
                    agent_cards: vec![],
                    images: vec![],
                    documents: vec![],
                    delivered_notification_ids: vec![],
                });
                self.emit(
                    |session_id| Event::ToolCallEnd {
                        session_id,
                        item_id,
                        output: note,
                        is_error: true,
                        stopped: false,
                        edit: None,
                    },
                    tx,
                );
                continue;
            }

            // AskUserQuestion: structured questions are intercepted and
            // executed at the session layer (the tool itself only registers a
            // schema). Read-only, no approval needed; an Esc skip (None) is not
            // an error.
            if call.name == "AskUserQuestion" {
                let args: serde_json::Value =
                    serde_json::from_str(&call.arguments).unwrap_or_default();
                let questions = match tool::parse_questions(&args) {
                    Ok(questions) => questions,
                    Err(error) => {
                        let note = format!("Invalid AskUserQuestion arguments: {error}");
                        self.history
                            .push(ChatMsg::tool_result(&call.id, note.clone()).with_error(true));
                        self.record(&RolloutRecord::ToolCall {
                            tool: call.name.clone(),
                            summary,
                            arguments: call.arguments.clone(),
                            output: note.clone(),
                            is_error: true,
                            stopped: false,
                            edit: None,
                            agent_card: None,
                            agent_cards: vec![],
                            images: vec![],
                            documents: vec![],
                            delivered_notification_ids: vec![],
                        });
                        self.emit(
                            |session_id| Event::ToolCallEnd {
                                session_id,
                                item_id,
                                output: note,
                                is_error: true,
                                stopped: false,
                                edit: None,
                            },
                            tx,
                        );
                        continue;
                    }
                };
                let request_id = format!("{}-{turn_id}-question-{item_id}", self.id);
                let (reply_tx, reply_rx) = oneshot::channel();
                self.pending_questions
                    .lock()
                    .expect("pending questions lock")
                    .insert(request_id.clone(), reply_tx);
                self.emit(
                    |session_id| Event::QuestionRequested {
                        session_id,
                        request_id: request_id.clone(),
                        questions: questions.clone(),
                    },
                    tx,
                );
                let reply = tokio::select! {
                    // sender dropped (replier gone) is treated as skip
                    reply = reply_rx => reply.unwrap_or(None),
                    _ = cancel.cancelled() => {
                        self.pending_questions
                            .lock()
                            .expect("pending questions lock")
                            .remove(&request_id);
                        self.settle_cancelled_tool(
                            CancelledTool {
                                call,
                                summary,
                                item_id: &item_id,
                                turn_id: &turn_id,
                                        rest: &tool_calls[call_ix + 1..],
                                        rest_item_ids: &item_ids[call_ix + 1..],
                                card: None,
                                cards: vec![],
                            },
                            tx,
                        );
                        self.emit(|session_id| Event::TurnAborted { session_id }, tx);
                return StepOutcome::Ended { aborted: true };
                    }
                };
                let note = match &reply {
                    Some(answers) => {
                        let mut text = "The user answered:\n".to_string();
                        for (ix, question) in questions.iter().enumerate() {
                            let labels = answers
                                .get(ix)
                                .map(|labels| labels.join(", "))
                                .filter(|s| !s.is_empty())
                                .unwrap_or_else(|| "(no selection)".to_string());
                            text.push_str(&format!(
                                "{}. {}: {}\n",
                                ix + 1,
                                question.question,
                                labels
                            ));
                        }
                        text
                    }
                    None => "The user chose not to answer; decide from context and continue."
                        .to_string(),
                };
                self.history
                    .push(ChatMsg::tool_result(&call.id, note.clone()));
                self.record(&RolloutRecord::ToolCall {
                    tool: call.name.clone(),
                    summary,
                    arguments: call.arguments.clone(),
                    output: note.clone(),
                    is_error: false,
                    stopped: false,
                    edit: None,
                    agent_card: None,
                    agent_cards: vec![],
                    images: vec![],
                    documents: vec![],
                    delivered_notification_ids: vec![],
                });
                self.emit(
                    |session_id| Event::ToolCallEnd {
                        session_id,
                        item_id,
                        output: note,
                        is_error: false,
                        stopped: false,
                        edit: None,
                    },
                    tx,
                );
                continue;
            }

            // Generic path: danger blocklist/project permission rules/approval
            // gate/execution/session-level side effects all live in
            // exec_tool_gated (the subagent loop reuses the same gate); this
            // spot only finalizes history/rollout/ToolCallEnd (the parent
            // session's own history and rollout). The span covers the gate —
            // approval wait included — plus execution, the same basis as the
            // harness's per-call tool time.
            let tool_started = Instant::now();
            let gated = self
                .exec_tool_gated(
                    call,
                    tool_ref.map(|t| t.as_ref()),
                    &item_id,
                    &turn_id,
                    tx,
                    cancel,
                )
                .await;
            self.turn_tool_ms += tool_started.elapsed().as_millis() as u64;
            match gated {
                GatedToolOutcome::Rejected { note } => {
                    self.history
                        .push(ChatMsg::tool_result(&call.id, note.clone()).with_error(true));
                    self.record(&RolloutRecord::ToolCall {
                        tool: call.name.clone(),
                        summary,
                        arguments: call.arguments.clone(),
                        output: note.clone(),
                        is_error: true,
                        stopped: false,
                        edit: None,
                        agent_card: None,
                        agent_cards: vec![],
                        images: vec![],
                        documents: vec![],
                        delivered_notification_ids: vec![],
                    });
                    self.emit(
                        |session_id| Event::ToolCallEnd {
                            session_id,
                            item_id,
                            output: note,
                            is_error: true,
                            stopped: false,
                            edit: None,
                        },
                        tx,
                    );
                }
                GatedToolOutcome::Cancelled => {
                    self.settle_cancelled_tool(
                        CancelledTool {
                            call,
                            summary,
                            item_id: &item_id,
                            turn_id: &turn_id,
                            rest: &tool_calls[call_ix + 1..],
                            rest_item_ids: &item_ids[call_ix + 1..],
                            card: None,
                            cards: vec![],
                        },
                        tx,
                    );
                    self.emit(|session_id| Event::TurnAborted { session_id }, tx);
                    return StepOutcome::Ended { aborted: true };
                }
                GatedToolOutcome::Executed {
                    mut output,
                    is_error,
                    edit,
                    mut images,
                    mut documents,
                    delivered_notification_ids,
                } => {
                    let image_refs = self.persist_tool_images(&mut output, &mut images);
                    let document_refs = self.persist_tool_documents(
                        &mut output,
                        &mut documents,
                        config.context_window,
                    );
                    let mut message =
                        ChatMsg::tool_result_with_images(&call.id, output.clone(), images)
                            .with_error(is_error);
                    message.documents = documents;
                    self.history.push(message);
                    self.delivery_receipts
                        .lock()
                        .expect("notification receipt lock")
                        .extend(delivered_notification_ids.iter().map(|id| {
                            (
                                id.clone(),
                                crate::task::notification::DeliveryReceipt {
                                    channel: pig_protocol::DeliveryChannel::TaskWait,
                                    confirmed: false,
                                },
                            )
                        }));
                    self.record(&RolloutRecord::ToolCall {
                        tool: call.name.clone(),
                        summary,
                        arguments: call.arguments.clone(),
                        output: output.clone(),
                        is_error,
                        stopped: false,
                        edit: edit.clone(),
                        agent_card: None,
                        agent_cards: vec![],
                        images: image_refs,
                        documents: document_refs,
                        delivered_notification_ids,
                    });
                    self.emit(
                        |session_id| Event::ToolCallEnd {
                            session_id,
                            item_id,
                            output,
                            is_error,
                            stopped: false,
                            edit,
                        },
                        tx,
                    );
                }
            }
        }
        if paused {
            StepOutcome::Paused
        } else {
            StepOutcome::ToolsExecuted
        }
    }

    /// Executes a parallel read-only group (P0): calls in the group are judged
    /// read-only by parallel_mask, approval-free in the current mode, and free
    /// of session-layer interception (sync points such as Agent/AskUserQuestion/
    /// plan mode toggles all live on the serial path). ToolCallBegin is emitted
    /// first in original order (card order = original order); ToolCallEnd
    /// arrives as each call completes (addressed by item_id; the TUI's
    /// find_or_create tolerates out-of-order arrival, and replay rebuilds from
    /// the rollout record order); history/rollout are backfilled by original
    /// index once the group drains, so tool_result pairing order is preserved.
    /// Returns false = cancelled: running tasks have been aborted and drained,
    /// unfinished calls have received "Stopped" receipts, calls after the group
    /// have history receipts, and TurnAborted has been sent (the caller goes
    /// straight to Ended).
    #[allow(clippy::too_many_arguments)]
    async fn run_parallel_group(
        &mut self,
        tool_calls: &[ToolCall],
        item_ids: &[String],
        start: usize,
        end: usize,
        turn_id: &str,
        media: tool::MediaCapabilities,
        context_window: u64,
        trusted: bool,
        tx: &async_channel::Sender<Event>,
        cancel: &CancellationToken,
    ) -> bool {
        /// Cap on calls executing simultaneously within a parallel group
        const MAX_PARALLEL: usize = 8;

        // Emit Begin for all group members in original order and precompute
        // card info (summary is persisted with the rollout). Item ids were
        // claimed per invocation, so duplicate provider call ids stay distinct.
        let group = &tool_calls[start..end];
        let mut cards: Vec<ParallelCall> = Vec::with_capacity(group.len());
        for (offset, call) in group.iter().enumerate() {
            let item_id = item_ids[start + offset].clone();
            let detail = serde_json::from_str::<serde_json::Value>(&call.arguments)
                .map(|v| serde_json::to_string_pretty(&v).unwrap_or_default())
                .unwrap_or_else(|_| call.arguments.clone());
            let summary = tool::summarize(call);
            self.emit(
                |session_id| Event::ToolCallBegin {
                    session_id,
                    item_id: item_id.clone(),
                    tool: call.name.clone(),
                    input_summary: summary.clone(),
                    detail,
                },
                tx,
            );
            cards.push(ParallelCall {
                call: call.clone(),
                item_id,
                summary,
            });
        }

        // Concurrent execution: read-only tools never touch the ChangeTracker
        // (each task holds a one-shot instance, with a debug assertion as
        // backstop); session-shared state (read_states/todos/tasks) is all
        // Arc<Mutex>/atomic, designed for foreground/background sharing from the
        // start, so concurrent reads are safe; write exclusion is guaranteed by
        // grouping (write tools are sync points, executed only after the group
        // drains). MCP tools: the mask already guarantees read-only +
        // approval-free + no deny hit; the task clones Arc<McpManager> and
        // fetches by name (an McpTool clone is just an Arc clone, cheap),
        // executed via execute_with_extra
        self.plugin_tools.dispatcher().set_event_sender(tx.clone());
        let mut set: tokio::task::JoinSet<(usize, ParallelOutput)> = tokio::task::JoinSet::new();
        let mut slots: Vec<Option<ParallelOutput>> = Vec::new();
        slots.resize_with(cards.len(), || None);
        let mut next = 0usize;
        let cancelled = loop {
            while next < cards.len() && set.len() < MAX_PARALLEL {
                let index = next;
                let call = cards[index].call.clone();
                let cwd = self.cwd.clone();
                let data_dir = self.data_dir.clone();
                let state = self.state.clone();
                let mcp = self.mcp.clone();
                let plugin_tools = self.plugin_tools.clone();
                let plugin_filter = self.plugin_tool_filter.clone();
                let sandbox_profile =
                    crate::task::sandbox_profile_for(self.permissions, self.plan_enabled);
                set.spawn(async move {
                    let caps = media;
                    let mut tracker = ChangeTracker::default();
                    let ctx = ToolContext {
                        cwd: &cwd,
                        tracker: &mut tracker,
                        state: &state,
                        // Group members are fs_outside-free by the mask; the
                        // grant lives on the serial gate path only
                        fs_grant: None,
                        sandbox_override: Default::default(),
                        sandbox_profile,
                        additional_permissions: Default::default(),
                    };
                    let mut extra: Vec<Box<dyn tool::Tool>> = match &mcp {
                        Some(mcp) if call.name.starts_with("mcp__") => {
                            mcp.tool_named(&call.name).into_iter().collect()
                        }
                        _ => vec![],
                    };
                    extra.extend(plugin_tools.tools_filtered(&plugin_filter));
                    // Skill is read-only and concurrentable, served via the
                    // extra channel (same as the serial gate); SendMessage is
                    // root-only + stateless + registry-backed, same treatment
                    if call.name == "Skill" {
                        extra.push(Box::new(tool::SkillTool::new_trusted(
                            &cwd, &data_dir, trusted,
                        )));
                    }
                    if call.name == "SendMessage" {
                        extra.push(Box::new(tool::SendMessageTool));
                    }
                    let tool_started = Instant::now();
                    let (mut output, is_error, file_change, edit, mut media) =
                        tool::execute_with_extra(&call, ctx, &extra).await;
                    let elapsed_ms = tool_started.elapsed().as_millis() as u64;
                    debug_assert!(
                        file_change.is_none() && tracker.take_dirty().is_empty(),
                        "concurrent read-only segment produces no file changes: {}",
                        call.name
                    );
                    project_tool_media(&call.arguments, &mut output, &mut media, caps);
                    let images = tool_images_to_chat(&call.arguments, media.images);
                    let documents = tool_documents_to_chat(media.documents);
                    (
                        index,
                        ParallelOutput {
                            output,
                            is_error,
                            edit,
                            images,
                            image_refs: vec![],
                            documents,
                            document_refs: vec![],
                            file_change,
                            elapsed_ms,
                        },
                    )
                });
                next += 1;
            }
            if set.is_empty() {
                break false;
            }
            tokio::select! {
                joined = set.join_next() => {
                    match joined {
                        Some(Ok((index, mut out))) => {
                            // Persist media before the completion event so the
                            // live event and the rollout output never diverge
                            out.image_refs =
                                self.persist_tool_images(&mut out.output, &mut out.images);
                            out.document_refs = self.persist_tool_documents(
                                &mut out.output,
                                &mut out.documents,
                                context_window,
                            );
                            self.emit(
                                |session_id| Event::ToolCallEnd {
                                    session_id,
                                    item_id: cards[index].item_id.clone(),
                                    output: out.output.clone(),
                                    is_error: out.is_error,
                                    stopped: false,
                                    edit: out.edit.clone(),
                                },
                                tx,
                            );
                            slots[index] = Some(out);
                        }
                        // panic/abort: leave the slot empty; the wrap-up below
                        // backstops it
                        Some(Err(_)) => {}
                        None => break false,
                    }
                }
                _ = cancel.cancelled() => break true,
            }
        };

        if cancelled {
            // Abort running tasks and drain: a blocking read finishes the
            // current fs call then exits at the next yield point; the JoinSet
            // destructor backstops with abort so no orphans remain; those that
            // happen to finish within the drain window settle as normal
            // completions
            set.abort_all();
            while let Some(joined) = set.join_next().await {
                if let Ok((index, mut out)) = joined {
                    // Same ordering as the normal completion path
                    out.image_refs = self.persist_tool_images(&mut out.output, &mut out.images);
                    out.document_refs = self.persist_tool_documents(
                        &mut out.output,
                        &mut out.documents,
                        context_window,
                    );
                    self.emit(
                        |session_id| Event::ToolCallEnd {
                            session_id,
                            item_id: cards[index].item_id.clone(),
                            output: out.output.clone(),
                            is_error: out.is_error,
                            stopped: false,
                            edit: out.edit.clone(),
                        },
                        tx,
                    );
                    slots[index] = Some(out);
                }
            }
        }

        // Unified wrap-up (strictly original order): history and rollout are
        // backfilled by original index
        for (index, card) in cards.iter().enumerate() {
            match slots[index].take() {
                Some(out) => {
                    self.turn_tool_ms += out.elapsed_ms;
                    let ParallelOutput {
                        output,
                        is_error,
                        edit,
                        images,
                        documents,
                        image_refs,
                        document_refs,
                        file_change,
                        ..
                    } = out;
                    let mut message =
                        ChatMsg::tool_result_with_images(&card.call.id, output.clone(), images)
                            .with_error(is_error);
                    message.documents = documents;
                    self.history.push(message);
                    self.record(&RolloutRecord::ToolCall {
                        tool: card.call.name.clone(),
                        summary: card.summary.clone(),
                        arguments: card.call.arguments.clone(),
                        output,
                        is_error,
                        stopped: false,
                        edit,
                        agent_card: None,
                        agent_cards: vec![],
                        images: image_refs,
                        documents: document_refs,
                        delivered_notification_ids: vec![],
                    });
                    // Defense in depth: classification guarantees read-only
                    // means no changes; a future mutating tool mislabeled
                    // read_only loses no data
                    if let Some(change) = file_change {
                        {
                            let store = self.store.lock().expect("store lock");
                            if change.additions == 0 && change.deletions == 0 {
                                store.delete_file_change(&self.id, &change.path);
                            } else {
                                store.upsert_file_change(
                                    &self.id,
                                    &change.path,
                                    &change.unified_diff,
                                    change.additions,
                                    change.deletions,
                                );
                            }
                        }
                        self.emit(
                            |session_id| Event::FileChanged {
                                session_id,
                                path: change.path,
                                unified_diff: change.unified_diff,
                                additions: change.additions,
                                deletions: change.deletions,
                            },
                            tx,
                        );
                    }
                }
                None if cancelled => {
                    // Cancelled: every group member already got a Begin, so
                    // settle each card as "Stopped" (rest semantics handled
                    // uniformly below)
                    self.settle_cancelled_tool(
                        CancelledTool {
                            call: &card.call,
                            summary: card.summary.clone(),
                            item_id: &card.item_id,
                            turn_id,
                            rest: &[],
                            rest_item_ids: &[],
                            card: None,
                            cards: vec![],
                        },
                        tx,
                    );
                }
                None => {
                    // Empty slot on the normal path = task panic: backfill an
                    // error receipt to keep tool_use pairing complete
                    let note = "Internal tool execution error (the task terminated unexpectedly)"
                        .to_string();
                    self.history
                        .push(ChatMsg::tool_result(&card.call.id, note.clone()).with_error(true));
                    self.record(&RolloutRecord::ToolCall {
                        tool: card.call.name.clone(),
                        summary: card.summary.clone(),
                        arguments: card.call.arguments.clone(),
                        output: note.clone(),
                        is_error: true,
                        stopped: false,
                        edit: None,
                        agent_card: None,
                        agent_cards: vec![],
                        images: vec![],
                        documents: vec![],
                        delivered_notification_ids: vec![],
                    });
                    self.emit(
                        |session_id| Event::ToolCallEnd {
                            session_id,
                            item_id: card.item_id.clone(),
                            output: note,
                            is_error: true,
                            stopped: false,
                            edit: None,
                        },
                        tx,
                    );
                }
            }
        }

        if cancelled {
            for (offset, rest) in tool_calls[end..].iter().enumerate() {
                let item_id = item_ids[end + offset].clone();
                let summary = tool::summarize(rest);
                let detail = serde_json::from_str::<serde_json::Value>(&rest.arguments)
                    .map(|value| serde_json::to_string_pretty(&value).unwrap_or_default())
                    .unwrap_or_else(|_| rest.arguments.clone());
                self.emit(
                    |session_id| Event::ToolCallBegin {
                        session_id,
                        item_id: item_id.clone(),
                        tool: rest.name.clone(),
                        input_summary: summary.clone(),
                        detail,
                    },
                    tx,
                );
                self.settle_cancelled_tool(
                    CancelledTool {
                        call: rest,
                        summary,
                        item_id: &item_id,
                        turn_id,
                        rest: &[],
                        rest_item_ids: &[],
                        card: None,
                        cards: vec![],
                    },
                    tx,
                );
            }
            self.emit(|session_id| Event::TurnAborted { session_id }, tx);
        }
        !cancelled
    }
}

/// Pointer line for @-referenced files: injects only the path and size — no
/// content read, no reading guidance (kimi-code TUI's minimal form: @ is an
/// attention guide; "call tools proactively when needed" is a standing system-
/// prompt instruction and the tool list is right there, so per-file hints are
/// noise). Paths still go through resolve_checked validation (inside the
/// workspace, non-sensitive); images are annotated with their kind only.
pub(crate) fn pointer_file_references(cwd: &Path, content: &str, files: &[String]) -> String {
    let mut text = content.to_string();
    for file in files {
        let block = match tool::resolve_checked(cwd, file, false) {
            Ok(full) => {
                let size = std::fs::metadata(&full).ok().map(|m| m.len());
                let kind = if is_image_path(file) { ", image" } else { "" };
                match size {
                    Some(bytes) => {
                        format!("\n\n[Referenced file {file} ({}{kind})]", human_size(bytes))
                    }
                    None => format!("\n\n[Referenced file {file}{kind}]"),
                }
            }
            Err(error) => format!("\n\n[Cannot reference file {file}: {error}]"),
        };
        text.push_str(&block);
    }
    text
}

/// Reads the plan file (when ExitPlanMode's plan argument is omitted): None if
/// missing or unreadable
fn read_plan_file(cwd: &Path, session_id: &str) -> Option<String> {
    std::fs::read_to_string(
        cwd.join(".pigcode")
            .join("plans")
            .join(format!("plan-{session_id}.md")),
    )
    .ok()
}

/// Persists the plan (same as ZCode plan-file-continuity): when ExitPlanMode
/// is approved, atomically write the full plan to
/// `<cwd>/.pigcode/plans/plan-<session_id>.md` (tmp+rename). Failure only logs
/// and does not block execution — the plan already lives in the conversation
/// history and rollout
fn write_plan_file(cwd: &Path, session_id: &str, plan: &str) {
    let dir = cwd.join(".pigcode").join("plans");
    let path = dir.join(format!("plan-{session_id}.md"));
    let tmp = dir.join(format!("plan-{session_id}.md.tmp"));
    let result = std::fs::create_dir_all(&dir)
        .and_then(|()| std::fs::write(&tmp, plan))
        .and_then(|()| std::fs::rename(&tmp, &path));
    if let Err(error) = result {
        tracing::warn!("failed to persist plan {}: {error}", path.display());
    }
}

fn is_image_path(path: &str) -> bool {
    let Some((_, ext)) = path.rsplit_once('.') else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp"
    )
}

fn human_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / 1024.0 / 1024.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

/// Precomputed card info for one call in a parallel read-only group (Begin sent
/// first, End arrives on completion)
struct ParallelCall {
    call: ToolCall,
    item_id: String,
    summary: String,
}

/// Execution product of one call in a parallel read-only group (same shape as
/// GatedToolOutcome::Executed, plus a defensive file_change channel —
/// classification guarantees read-only is always None; if mislabeled, no data
/// is lost)
struct ParallelOutput {
    output: String,
    is_error: bool,
    edit: Option<pig_protocol::EditDiff>,
    images: Vec<pig_provider::ChatImage>,
    /// Refs persisted before the completion event is emitted
    image_refs: Vec<crate::rollout::ImageRef>,
    documents: Vec<pig_provider::ChatDocument>,
    /// Refs persisted before the completion event is emitted
    document_refs: Vec<crate::rollout::DocumentRef>,
    file_change: Option<tool::FileChange>,
    /// This call's own span (concurrent calls overlap, so the turn sums them
    /// the way the harness sums per-call tool spans rather than using the
    /// segment's wall time)
    elapsed_ms: u64,
}

/// Exclusion list for the parallel read-only segment: tools that declare
/// read_only() but have session side effects or session-layer interception
/// semantics all fall back to serial sync points. The interception logic for
/// AskUserQuestion/EnterPlanMode/ExitPlanMode/Agent lives in run_step's serial
/// body with unchanged semantics (natural sync points).
const PARALLEL_EXCLUDED: &[&str] = &[
    "TodoList",        // write variant mutates todos, persists and pushes events
    "TaskStop",        // stops a background task; a mutating operation
    "AskUserQuestion", // session-layer popup interception
    "EnterPlanMode",   // mode-switch interception
    "ExitPlanMode",    // mode-switch interception (with approval popup)
    "Agent",           // subagent delegation interception
];

/// Concurrency-safety check for a single call (conservative: all must hold):
/// known tool, read_only, approval-free in the current mode (read-only tools
/// are approval-free in all modes under the current approval matrix, but still
/// go through the same requires_approval check to guard against regressions if
/// the matrix changes). Out-of-workspace access in this codebase is a hard
/// error / session-toggle gate (resolve_with_access) and produces no approval
/// popup; sensitive files are unconditionally hard-rejected inside tools —
/// approval popups can only come from the dangerous-command/requires_approval
/// branches, and group members classified here never enter those two branches,
/// so two concurrent approval popups are impossible. ReadMediaFile is
/// intercepted by the session-layer capability gate when the model lacks image
/// input; not concurrent. MCP and plugin tools also require a project deny
/// precheck because the concurrent path bypasses exec_tool_gated_ctx. The
/// subject is the full tool name, same as the serial gate. McpClient
/// request multiplexing is verified concurrency-safe (stdio: AtomicU64 id +
/// Mutex pending map + stdin write lock; http: one independent POST per
/// request).
#[allow(clippy::too_many_arguments)]
fn parallel_safe(
    call: &ToolCall,
    tools: &[Box<dyn tool::Tool>],
    permissions: pig_protocol::PermissionSettings,
    media: tool::MediaCapabilities,
    permission_rules: &crate::permissions::PermissionRules,
    state: &crate::task::SessionToolState,
    cwd: &std::path::Path,
    hooks_active: bool,
) -> bool {
    if PARALLEL_EXCLUDED.contains(&call.name.as_str()) || call.name == "TaskWait" {
        return false;
    }
    // Lifecycle hooks are user shell commands with ordering semantics; the
    // serial gate owns their PreToolUse/PermissionRequest/PostToolUse chains
    // (a hook may rewrite the input or block, changing what executes). Until
    // the parallel dispatcher can run the same preflight, any active hook set
    // forces the serial path — hooks are always observed, never skipped.
    if hooks_active {
        return false;
    }
    // An out-of-workspace WRITE needs the approval popup — only the serial gate
    // can request one, so such calls are sync points. Reads are never gated,
    // with one exception: a read tool whose target sits inside a credential
    // store is auto-detected by the gate, so it must reach it.
    if tool::fs_outside_intent(state, cwd, &call.name, &call.arguments).is_some() {
        return false;
    }
    if let Some(target) = tool::file_tool_target(cwd, call)
        && crate::task::credential_grant(state, &target) != crate::task::CredentialGrant::Outside
    {
        return false;
    }
    if call.name == "ReadMediaFile" && !media.any() {
        return false;
    }
    let Some(tool_ref) = tools.iter().find(|t| t.name() == call.name.as_str()) else {
        return false;
    };
    if !tool_ref.read_only() || tool::requires_approval(tool_ref.as_ref(), permissions) {
        return false;
    }
    // Project deny precheck for runtime tools (hit → falls back to a serial
    // sync point; the full gate produces the rejection message).
    if (call.name.starts_with("mcp__") || tool_ref.is_plugin())
        && permission_rules.deny_hit(&call.name, &call.name).is_some()
    {
        return false;
    }
    true
}

/// Slices one step's tool_calls into a parallel-segment mask: true = may join
/// a parallel read-only group; false = sync point (writes/shell/needs
/// approval/session-layer interception/unknown tool/MCP deny hit/hooks
/// active), executed serially on its own after the preceding group drains.
#[allow(clippy::too_many_arguments)]
fn parallel_mask(
    calls: &[ToolCall],
    tools: &[Box<dyn tool::Tool>],
    permissions: pig_protocol::PermissionSettings,
    media: tool::MediaCapabilities,
    permission_rules: &crate::permissions::PermissionRules,
    state: &crate::task::SessionToolState,
    cwd: &std::path::Path,
    hooks_active: bool,
) -> Vec<bool> {
    calls
        .iter()
        .map(|call| {
            parallel_safe(
                call,
                tools,
                permissions,
                media,
                permission_rules,
                state,
                cwd,
                hooks_active,
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pig_protocol::ExecMode;

    /// Title seed: the first non-empty line, trimmed, capped at 30 chars. Raw
    /// `\n`/`\t` must never reach the stored title — the sidebar measures
    /// titles with gpui's `shape_line`, which debug_assert-panics on embedded
    /// newlines (2026-10-08 crash: expanding a workspace whose session title
    /// carried the multi-line message's raw newlines killed the app).
    #[test]
    fn seed_title_takes_first_line_without_control_chars() {
        // The real crash message's head: multi-line, tab-separated fields
        assert_eq!(
            seed_title("产成品入库接口\n基本信息\n项目\t内容\n接口名称\t产成品入库生单接口"),
            "产成品入库接口"
        );
        // Leading blank lines are skipped
        assert_eq!(seed_title("\n\n  \nsecond line\n"), "second line");
        // Single-line messages keep the old behavior (first 30 chars)
        let long = "字".repeat(45);
        assert_eq!(seed_title(&long), "字".repeat(30));
        assert_eq!(seed_title("你好"), "你好");
        // Whitespace-only content seeds empty (placeholder renders)
        assert_eq!(seed_title(" \n\t "), "");
    }

    /// Out-of-workspace WRITE targets demote to serial sync points (only the
    /// gate can pop the approval). Reads outside the workspace are not gated —
    /// full-disk read is the sandbox policy — so they stay parallel-eligible.
    #[test]
    fn parallel_mask_demotes_outside_writes_only() {
        let marker = format!("pig-mask-out-{}", std::process::id());
        let ws = std::env::temp_dir().join(format!("ws-{marker}"));
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(&ws).unwrap();
        let outside = std::env::temp_dir()
            .parent()
            .unwrap()
            .join(format!("outside-{marker}"));
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&outside).unwrap();
        let outside_str = outside.display().to_string().replace("\\", "/");
        let (task_notify, _t) = tokio::sync::mpsc::unbounded_channel();
        let state =
            crate::task::SessionToolState::new("mask-out".to_string(), task_notify, vec![], vec![]);
        let tools = tool::all();
        let rules = crate::permissions::PermissionRules::default();
        let call = |name: &str, path: &str| ToolCall {
            id: "c1".to_string(),
            name: name.to_string(),
            arguments: format!("{{\"path\": \"{path}\"}}"),
        };
        assert!(
            !parallel_safe(
                &call("Write", &format!("{outside_str}/new.txt")),
                &tools,
                ExecMode::Auto.permissions(),
                tool::MediaCapabilities::new(true, false),
                &rules,
                &state,
                &ws,
                false,
            ),
            "an outside WRITE must fall to the serial gate for its approval"
        );
        assert!(
            parallel_safe(
                &call("Read", "README.mock.md"),
                &tools,
                ExecMode::Auto.permissions(),
                tool::MediaCapabilities::new(true, false),
                &rules,
                &state,
                &ws,
                false,
            ),
            "an inside read stays parallel-eligible"
        );
    }

    fn mask_of(names: &[&str], mode: ExecMode, input_image: bool) -> Vec<bool> {
        mask_with(names, &tool::all(), mode, input_image, "no-rules")
    }

    /// Mask with tool set and permission rules: rules is permissions.toml text
    /// ("no-rules" is the special case = empty rules)
    fn mask_with(
        names: &[&str],
        tools: &[Box<dyn tool::Tool>],
        mode: ExecMode,
        input_image: bool,
        rules_toml: &str,
    ) -> Vec<bool> {
        let calls: Vec<ToolCall> = names
            .iter()
            .map(|name| ToolCall {
                id: format!("id-{name}"),
                name: name.to_string(),
                arguments: "{}".to_string(),
            })
            .collect();
        let (task_notify, _t_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = crate::task::SessionToolState::new(
            "mask-test".to_string(),
            task_notify,
            vec![],
            vec![],
        );
        let permissions = if rules_toml == "no-rules" {
            crate::permissions::PermissionRules::default()
        } else {
            crate::permissions::PermissionRules::parse(rules_toml).expect("rules should parse")
        };
        parallel_mask(
            &calls,
            tools,
            mode.permissions(),
            tool::MediaCapabilities::new(input_image, false),
            &permissions,
            &state,
            std::path::Path::new("/ws"),
            false,
        )
    }

    /// Build an MCP tool (for_test fake connection; only the name/read_only
    /// classification matters)
    fn mcp_tool(tool_name: &str, read_only: bool) -> Box<dyn tool::Tool> {
        let spec = crate::mcp::McpToolSpec {
            name: tool_name.to_string(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
            annotations: crate::mcp::McpToolAnnotations {
                read_only_hint: Some(read_only),
                ..Default::default()
            },
        };
        Box::new(crate::mcp::McpTool::new(
            "srv",
            spec,
            crate::mcp::McpClient::for_test("srv"),
        ))
    }

    #[test]
    fn readonly_calls_parallel_safe_in_all_modes() {
        for mode in [
            ExecMode::ReadOnly,
            ExecMode::Auto,
            ExecMode::Auto,
            ExecMode::Full,
        ] {
            let mask = mask_of(
                &["Read", "Grep", "Glob", "FetchURL", "TaskList", "TaskOutput"],
                mode,
                false,
            );
            assert!(mask.iter().all(|m| *m), "{mode:?}: {mask:?}");
        }
    }

    #[test]
    fn writes_shell_and_intercepted_tools_are_sync_points() {
        let mask = mask_of(
            &[
                "Write",
                "Edit",
                "ExecCommand",
                "TodoList",
                "TaskStop",
                "Agent",
                "AskUserQuestion",
                "EnterPlanMode",
                "ExitPlanMode",
                "NoSuchTool",
            ],
            ExecMode::Full,
            true,
        );
        assert!(mask.iter().all(|m| !*m), "{mask:?}");
    }

    #[test]
    fn read_media_file_needs_image_capability() {
        assert_eq!(mask_of(&["ReadMediaFile"], ExecMode::Auto, true), [true]);
        assert_eq!(mask_of(&["ReadMediaFile"], ExecMode::Auto, false), [false]);
    }

    #[test]
    fn segments_split_on_sync_points() {
        // Read Read | Write | Grep Glob | Bash | Read — writes/shell split
        // segments
        let mask = mask_of(
            &[
                "Read",
                "Read",
                "Write",
                "Grep",
                "Glob",
                "ExecCommand",
                "Read",
            ],
            ExecMode::Auto,
            false,
        );
        assert_eq!(mask, [true, true, false, true, true, false, true]);
    }

    // ---------- MCP tools joining parallel groups ----------

    #[test]
    fn mcp_readonly_parallel_safe_in_all_modes() {
        let tools: Vec<Box<dyn tool::Tool>> = tool::all()
            .into_iter()
            .chain(vec![mcp_tool("read", true), mcp_tool("write", false)])
            .collect();
        for mode in [
            ExecMode::ReadOnly,
            ExecMode::Auto,
            ExecMode::Auto,
            ExecMode::Full,
        ] {
            let mask = mask_with(
                &["mcp__srv__read", "mcp__srv__write"],
                &tools,
                mode,
                false,
                "no-rules",
            );
            assert_eq!(
                mask,
                [true, false],
                "{mode:?}: read-only MCP tools run concurrently, writing MCP is a sync point"
            );
        }
    }

    #[test]
    fn mcp_denied_by_project_rule_falls_back_to_serial() {
        let tools: Vec<Box<dyn tool::Tool>> = tool::all()
            .into_iter()
            .chain(vec![mcp_tool("read", true)])
            .collect();
        // deny rule hit (full tool name as subject, same as the serial gate) →
        // back to serial
        let mask = mask_with(
            &["mcp__srv__read"],
            &tools,
            ExecMode::Full,
            false,
            "deny = [\"mcp__srv__read(*)\"]",
        );
        assert_eq!(
            mask,
            [false],
            "MCP tools matched by deny cannot run concurrently"
        );
    }

    #[test]
    fn mcp_unknown_tool_not_parallel() {
        // An mcp__ name absent from the tool set (not connected/not
        // inherited) → sync point
        let mask = mask_of(&["mcp__ghost__read"], ExecMode::Full, false);
        assert_eq!(mask, [false]);
    }

    fn pointer_test_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pig-pointer-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn parallel_group_executes_plugin_tools() {
        let dir = std::env::temp_dir().join(format!(
            "pig-parallel-plugin-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let data_dir = dir.join("data");
        let package_root = data_dir.join("plugins/demo");
        std::fs::create_dir_all(&package_root).expect("plugin fixture");
        std::fs::write(
            package_root.join("gpui-shell.json"),
            r#"{"id":"demo","name":"Demo","version":"1.0.0","entry":"main.js"}"#,
        )
        .expect("plugin identity");
        std::fs::write(
            package_root.join("pig-code.json"),
            r#"{
              "tools": [{
                "name": "DemoRead",
                "description": "Read demo data",
                "parameters": {"type": "object"},
                "read_only": true
              }],
              "actions": ["provide_tools"]
            }"#,
        )
        .expect("plugin contributions");
        std::fs::write(
            package_root.join("main.js"),
            "export default class Demo {}\n",
        )
        .expect("plugin entry");
        let mut registry = crate::plugins::PluginRegistry::new(data_dir.clone());
        let snapshot = registry.sync(vec![pig_protocol::PluginPackageSource {
            root: package_root,
            source: pig_protocol::PluginSource::User,
        }]);
        registry
            .set_enabled("demo", &snapshot.packages[0].digest, true)
            .expect("enable plugin");

        let sessions_dir = data_dir.join("sessions");
        std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
        let store = Arc::new(Mutex::new(
            crate::store::Store::open(&data_dir).expect("store"),
        ));
        let (task_tx, _) = tokio::sync::mpsc::unbounded_channel();
        let meta = SessionMeta {
            id: "s-plugin".into(),
            title: String::new(),
            title_custom: false,
            cwd: dir.clone(),
            created_at: 1,
            updated_at: 1,
            pinned: false,
            archived: false,
            provider_id: None,
            model_id: None,
            reasoning_level: None,
            permissions: PermissionSettings::default(),
            plan_enabled: false,
        };
        let mut session = Session::create(
            meta,
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashMap::new())),
            store,
            &sessions_dir,
            data_dir.clone(),
            task_tx,
            None,
        )
        .expect("session");
        let dispatcher = session.plugin_tools.dispatcher();
        let (tx, rx) = async_channel::unbounded();
        let cancel = CancellationToken::new();
        let running = tokio::spawn(async move {
            session
                .run_parallel_group(
                    &[ToolCall {
                        id: "call-plugin".into(),
                        name: "DemoRead".into(),
                        arguments: "{}".into(),
                    }],
                    &["item-plugin".into()],
                    0,
                    1,
                    "turn-plugin",
                    tool::MediaCapabilities::new(true, true),
                    128_000,
                    false,
                    &tx,
                    &cancel,
                )
                .await
        });
        let output = loop {
            match rx.recv().await.expect("plugin event") {
                Event::PluginToolCall { call } => {
                    dispatcher.resolve(pig_protocol::PluginToolResult {
                        request_id: call.request_id,
                        session_id: call.session_id,
                        package: call.package,
                        digest: call.digest,
                        result: Ok("parallel-ok".into()),
                    });
                }
                Event::ToolCallEnd { output, .. } => break output,
                _ => {}
            }
        };
        assert_eq!(output, "parallel-ok");
        assert!(running.await.expect("parallel group"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Pointer line: path + size only (images annotated with kind), no reading
    /// guidance, no file content
    #[test]
    fn pointer_references_never_inline_content() {
        let dir = pointer_test_dir("basic");
        std::fs::write(dir.join("a.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join("big.rs"), "x".repeat(2048)).unwrap();
        std::fs::write(dir.join("logo.png"), b"\x89PNG").unwrap();

        // Default: path + size, no guidance, no content
        let out = pointer_file_references(&dir, "take a look at this", &["a.rs".into()]);
        assert!(out.contains("[Referenced file a.rs (13 B)]"), "{out}");
        assert!(!out.contains("Read"), "no read guidance expected: {out}");
        assert!(!out.contains("fn main"), "must not inline content: {out}");

        // Large file: pointer only as well (the model reads it with paged
        // Read)
        let out = pointer_file_references(&dir, "view lines 10000 to 10050", &["big.rs".into()]);
        assert!(out.contains("[Referenced file big.rs (2.0 KB)]"), "{out}");
        assert!(!out.contains("xxx"), "must not inline content: {out}");

        // Image: kind annotation only
        let out = pointer_file_references(&dir, "view the image", &["logo.png".into()]);
        assert!(out.contains("(4 B, image)]"), "{out}");

        // Outside the workspace/missing: error line
        let out = pointer_file_references(&dir, "x", &["../etc/passwd".into()]);
        assert!(
            out.contains("[Cannot reference file ../etc/passwd"),
            "{out}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
