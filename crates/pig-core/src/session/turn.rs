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
    pub async fn run_turn(
        &mut self,
        content: String,
        files: Vec<String>,
        images: Vec<pig_protocol::PendingImage>,
        config: &ResolvedModel,
        tx: &async_channel::Sender<Event>,
        cancel: CancellationToken,
    ) {
        self.turn_counter += 1;
        self.turn_input = 0;
        self.turn_cache_read = 0;
        self.turn_output = 0;
        self.turn_api_ms = 0;
        self.turn_ttft_ms = 0;
        self.turn_api_steps = 0;
        let turn_id = format!("turn-{}", self.turn_counter);
        let started = Instant::now();
        self.emit(
            |session_id, seq| Event::TurnStarted {
                session_id,
                seq,
                turn_id: turn_id.clone(),
            },
            tx,
        );

        // The system prompt uses session-frozen snapshots throughout (git/
        // AGENTS.md/skills/date); the mode has moved into turn_reminder — bytes
        // stay stable within a session, maximizing prefix cache hits
        let system = ChatMsg::system(prompt::system_prompt(
            &self.cwd,
            true,
            self.git_snapshot.as_deref(),
            &self.date_frozen,
            &self.agents_prompt,
            &self.skills_prompt,
        ));
        if self.history.is_empty() {
            self.history.push(system);
        } else if self.history[0].role == "system" {
            self.history[0] = system;
        }
        // Turn-boundary reminder (exec mode/plan toggle on first turn or on
        // switch, date rollover, AGENTS.md changes): prepended to the user
        // message — injecting at the tail does not break the system+history
        // prefix cache, nor can it land between a tool_use/tool_result pair;
        // with nothing to remind, the user message stays as-is. Not persisted to
        // rollout (a resumed session heals via re-freezing + first-turn reminder)
        let fresh_agents = prompt::agents_md(&self.data_dir, &self.cwd);
        let reminder = prompt::turn_reminder(
            self.mode,
            self.plan_enabled,
            &self.id,
            &mut self.mode_reminded,
            (
                self.state
                    .fs_read_outside
                    .load(std::sync::atomic::Ordering::Relaxed),
                self.state
                    .fs_write_outside
                    .load(std::sync::atomic::Ordering::Relaxed),
            ),
            &mut self.fs_reminded,
            &self.date_frozen,
            &mut self.date_reminded,
            &self.agents_prompt,
            &fresh_agents,
            &mut self.agents_reminded,
        );
        // @-referenced files are injected as pointers (same trade-off as
        // kimi-code): only the path (+ optional line range) is given; the model
        // reads the content on demand with Read — always fresh, a constant one
        // line, no prefix-cache damage
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
        let rollout_text = content.clone();
        let record_files = files.clone();
        // Pasted images (ZCode-style pipeline): compress → persist into the
        // session media dir → rollout records an ImageRef (no base64 stored) +
        // history gets a ChatImage; images that fail to compress are skipped
        // with a note in the text. File names continue the directory sequence
        // (next_media_index): naming by per-message index would be overwritten
        // by later turns
        let mut image_refs: Vec<crate::rollout::ImageRef> = Vec::new();
        let mut chat_images: Vec<crate::provider::ChatImage> = Vec::new();
        if !images.is_empty() {
            let media_dir = crate::rollout::media_dir(&self.data_dir.join("sessions"), &self.id);
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
                            user_text
                                .push_str(&format!("\n[Image {} failed to save: {error}]", ix + 1));
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
                        });
                        chat_images.push(crate::provider::ChatImage {
                            media_type: comp.media_type,
                            data_base64: crate::tool::base64_encode(&comp.bytes),
                            label: Some(format!("Image {}", ix + 1)),
                        });
                    }
                    Err(error) => {
                        user_text
                            .push_str(&format!("\n[Image {} failed to compress: {error}]", ix + 1));
                    }
                }
            }
        }
        // Capability projection: model without image input → keep out of
        // ChatMsg.images and inform via a text placeholder (with the media path)
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
        self.history.push(user_msg);
        // The event carries the clean text + attachment numbers (for UI
        // thumbnails); history/rollout likewise hold the clean text
        let nums = crate::rollout::image_nums(&image_refs);
        self.record(&RolloutRecord::User {
            text: rollout_text.clone(),
            files: record_files.clone(),
            images: image_refs,
        });
        self.emit(
            |session_id, seq| Event::UserMessage {
                session_id,
                seq,
                text: rollout_text.clone(),
                files: record_files.clone(),
                image_nums: nums,
            },
            tx,
        );

        let mut step = 0usize;
        loop {
            step += 1;
            match self
                .run_step(turn_id.clone(), step, config, tx, &cancel)
                .await
            {
                StepOutcome::TextOnly => {
                    let duration_ms = started.elapsed().as_millis() as u64;
                    if self.turn_input + self.turn_cache_read + self.turn_output > 0 {
                        self.store.lock().expect("store lock").record_usage(
                            &self.id,
                            &config.provider_name,
                            &config.model,
                            self.turn_input,
                            self.turn_cache_read,
                            self.turn_output,
                            self.turn_reasoning_output,
                        );
                        // Persist turn stats: replay restores the footer and
                        // session totals (the usage watermark is restored by
                        // StepUsage)
                        self.record(&RolloutRecord::TurnStats {
                            input: self.turn_input,
                            cache_read: self.turn_cache_read,
                            output: self.turn_output,
                            duration_ms,
                            api_ms: self.turn_api_ms,
                            ttft_ms: self.turn_ttft_ms,
                            api_steps: self.turn_api_steps,
                        });
                    }
                    // The turn's changes panel is emitted before the turn-end
                    // event (durable data before boundary events)
                    self.flush_turn_changes(tx);
                    let stats = (self.turn_input + self.turn_cache_read + self.turn_output > 0)
                        .then_some(pig_protocol::TurnUsageStats {
                            input: self.turn_input,
                            cache_read: self.turn_cache_read,
                            output: self.turn_output,
                            duration_ms,
                            api_ms: self.turn_api_ms,
                            ttft_ms: self.turn_ttft_ms,
                            api_steps: self.turn_api_steps,
                        });
                    self.emit(
                        |session_id, seq| Event::TurnComplete {
                            session_id,
                            seq,
                            duration_ms,
                            stats,
                        },
                        tx,
                    );
                    self.touch_index();
                    return;
                }
                StepOutcome::ToolsExecuted => continue,
                StepOutcome::Ended => {
                    // Interrupt/failure wrap-up: changes already made this turn
                    // still produce a panel
                    self.flush_turn_changes(tx);
                    return;
                }
            }
        }
    }

    async fn run_step(
        &mut self,
        turn_id: String,
        step: usize,
        config: &ResolvedModel,
        tx: &async_channel::Sender<Event>,
        cancel: &CancellationToken,
    ) -> StepOutcome {
        // Lazy MCP connection (once per session): reads .pigcode/mcp.json +
        // data_dir/mcp.json; with no config we get an empty manager at
        // negligible cost; a single server failure does not affect the others
        if self.mcp.is_none() {
            self.mcp = Some(Arc::new(
                crate::mcp::McpManager::connect_all(&self.cwd, &self.data_dir).await,
            ));
        }
        // Check the usage watermark before sampling: above context_window -
        // max_output_tokens - 13k of buffer, auto-compact first
        if let Some(used) = self.last_total_tokens {
            let threshold = config
                .context_window
                .saturating_sub(config.max_output_tokens + 13_000);
            if used > threshold && !self.run_compact(Some(config), true, None, tx, cancel).await {
                return StepOutcome::Ended;
            }
        }

        let text_item = format!("{turn_id}-text-{step}");
        let reasoning_item = format!("{turn_id}-reason-{step}");
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let api_started = Instant::now();
        // Model-io trace input projection: snapshot before the request (images
        // recorded as counts, long content truncated). Persisting stores only
        // the delta: take the common prefix against the previous full
        // projection, offset + delta (aligned with ZCode model-io, avoiding
        // re-recording the growing full context per entry in the same session)
        let io_input_full = crate::model_io::project_input(&self.history);
        let io_offset = crate::model_io::common_prefix_len(&io_input_full, &self.io_last_input);
        let io_input: Vec<_> = io_input_full[io_offset..].to_vec();
        let provider_task = tokio::spawn(provider::stream_chat(
            config.clone(),
            self.history.clone(),
            // Root session tool set = built-in + Agent/AgentSwarm + MCP (rebuilt
            // every step: profiles and MCP tools may change)
            self.root_schemas(),
            event_tx,
            cancel.clone(),
        ));

        let mut text = String::new();
        let mut reasoning = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut provider_failed = false;
        // When the first output token (reasoning/text delta) arrives: TTFT =
        // that moment - request sent
        let mut first_token_at: Option<Instant> = None;
        // This step's usage and failure reason (for the persisted trace)
        let mut step_usage = crate::model_io::ModelIoUsage::default();
        let mut step_error: Option<String> = None;

        loop {
            let event = tokio::select! {
                event = event_rx.recv() => event,
                _ = cancel.cancelled() => None,
            };
            match event {
                Some(ProviderEvent::Reasoning(delta)) => {
                    first_token_at.get_or_insert_with(Instant::now);
                    reasoning.push_str(&delta);
                    self.emit(
                        |session_id, seq| Event::ReasoningDelta {
                            session_id,
                            seq,
                            item_id: reasoning_item.clone(),
                            delta,
                        },
                        tx,
                    );
                }
                Some(ProviderEvent::Text(delta)) => {
                    first_token_at.get_or_insert_with(Instant::now);
                    text.push_str(&delta);
                    self.emit(
                        |session_id, seq| Event::TextDelta {
                            session_id,
                            seq,
                            item_id: text_item.clone(),
                            delta,
                        },
                        tx,
                    );
                }
                Some(ProviderEvent::ToolCalls(calls)) => tool_calls = calls,
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
                    });
                    let (input_total, cache_read_total) = (self.input_total, self.cache_read_total);
                    self.emit(
                        |session_id, seq| Event::ContextUsage {
                            session_id,
                            seq,
                            used,
                            total,
                            cache_read_total,
                            input_total,
                        },
                        tx,
                    );
                }
                Some(ProviderEvent::Finished) | None => break,
                Some(ProviderEvent::Failed(error)) => {
                    // Structured error goes straight to the UI (forwarded via
                    // Event::Error); the trace persists a one-line English
                    // version
                    step_error = Some(crate::provider::core_error_en(&error));
                    self.emit(
                        |session_id, seq| Event::Error {
                            session_id: Some(session_id),
                            seq,
                            error,
                        },
                        tx,
                    );
                    provider_failed = true;
                    break;
                }
            }
        }
        // Pure API time: request sent to stream end (including failed
        // requests), excluding tool execution and approval waits; TTFT is up to
        // the first output token (a pure tool_call response has no delta
        // events, TTFT = 0)
        let api_elapsed = api_started.elapsed();
        self.turn_api_ms += api_elapsed.as_millis() as u64;
        self.turn_api_steps += 1;
        let step_ttft_ms = first_token_at
            .map(|at| at.duration_since(api_started).as_millis() as u64)
            .unwrap_or(0)
            .min(api_elapsed.as_millis() as u64);
        self.turn_ttft_ms += step_ttft_ms;

        // Persist the model-io trace (failures/cancellations recorded too): the
        // UI's "view model io" reads this file directly; write failures are
        // non-fatal (same policy as rollout.append, warn-level log per core
        // convention)
        let io_finish = if provider_failed {
            "error"
        } else if cancel.is_cancelled() {
            "cancelled"
        } else if !tool_calls.is_empty() {
            "tool_calls"
        } else {
            "stop"
        };
        let io_record = crate::model_io::ModelIoRecord {
            ts_ms: crate::model_io::now_ms(),
            turn: format!("{turn_id}-s{step}"),
            source: "main".into(),
            provider: config.provider_name.clone(),
            model: config.model.clone(),
            duration_ms: api_elapsed.as_millis() as u64,
            ttft_ms: step_ttft_ms,
            usage: step_usage,
            finish: io_finish.into(),
            error: step_error,
            reasoning: reasoning.clone(),
            text: text.clone(),
            tool_calls: crate::model_io::project_tool_calls(&tool_calls),
            input_offset: io_offset,
            input: io_input,
        };
        if let Err(e) =
            crate::model_io::append(&self.data_dir.join("sessions"), &self.id, &io_record)
        {
            tracing::warn!("Failed to write model io trace (ignored): {e}");
        }
        // This entry's full projection becomes the delta baseline for the next
        // one
        self.io_last_input = io_input_full;

        if provider_failed {
            provider_task.abort();
            return StepOutcome::Ended;
        }
        if cancel.is_cancelled() {
            provider_task.abort();
            self.emit(|session_id, seq| Event::TurnAborted { session_id, seq }, tx);
            return StepOutcome::Ended;
        }
        let _ = provider_task.await;

        if !reasoning.is_empty() {
            self.record(&RolloutRecord::Reasoning {
                text: reasoning.clone(),
            });
        }
        if !text.is_empty() {
            self.emit(
                |session_id, seq| Event::TextDone {
                    session_id,
                    seq,
                    item_id: text_item.clone(),
                    full_text: text.clone(),
                },
                tx,
            );
            self.record(&RolloutRecord::Text { text: text.clone() });
        }
        // Reasoning enters history with the assistant message: Anthropic
        // thinking mode requires it to be sent back
        self.history.push(ChatMsg::assistant(
            text,
            tool_calls.clone(),
            Some(reasoning).filter(|r| !r.is_empty()),
        ));
        if tool_calls.is_empty() {
            return StepOutcome::TextOnly;
        }

        let tools = self.root_tools();
        // P0 grouped concurrency: consecutive "safe to run concurrently"
        // read-only calls form a parallel group (JoinSet, cap 8); non-
        // concurrentable calls are sync points — after the preceding group
        // drains, they take the original serial path below (interception
        // semantics unchanged)
        let mask = parallel_mask(
            &tool_calls,
            &tools,
            self.mode,
            config.input_image,
            &self.permissions,
            &self.state,
            &self.cwd,
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
                    .run_parallel_group(&tool_calls, call_ix, group_end, &turn_id, tx, cancel)
                    .await
                {
                    return StepOutcome::Ended;
                }
                continue;
            }
            let item_id = format!("{}-tool-{}", turn_id, call.id);
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
                    |session_id, seq| Event::ToolCallBegin {
                        session_id,
                        seq,
                        item_id: item_id.clone(),
                        tool: call.name.clone(),
                        input_summary: summary.clone(),
                        detail,
                    },
                    tx,
                );
            }

            let tool_ref = tools.iter().find(|t| t.name() == call.name);
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
                    |session_id, seq| Event::ToolCallBegin {
                        session_id,
                        seq,
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
                    // Plan confirmation is a one-shot popup; it does not join
                    // same-key coalesced decisions
                    self.pending
                        .lock()
                        .expect("pending lock")
                        .insert(request_id.clone(), (reply_tx, None));
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
                        |session_id, seq| Event::ApprovalRequested {
                            session_id,
                            seq,
                            request_id: request_id.clone(),
                            tool: call.name.clone(),
                            detail: plan.to_string(),
                            danger_key: None,
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
                                    rest: &tool_calls[call_ix + 1..],
                                    card: None,
                                    cards: vec![],
                                },
                                tx,
                            );
                            self.emit(|session_id, seq| Event::TurnAborted { session_id, seq }, tx);
                            return StepOutcome::Ended;
                        }
                    };
                    match decision {
                        ApprovalDecision::Allow | ApprovalDecision::AlwaysAllow => {
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
                                |session_id, seq| Event::PlanModeChanged {
                                    session_id,
                                    seq,
                                    enabled: false,
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
                    .push(ChatMsg::tool_result(&call.id, note.clone()));
                self.record(&RolloutRecord::ToolCall {
                    tool: call.name.clone(),
                    summary,
                    // Persist the effective plan (file content when the
                    // argument is omitted): replay restores the plan card's full
                    // text from it
                    arguments: enriched_args,
                    output: note.clone(),
                    is_error,
                    edit: None,
                    agent_card: None,
                    agent_cards: vec![],
                });
                self.emit(
                    |session_id, seq| Event::ToolCallEnd {
                        session_id,
                        seq,
                        item_id,
                        output: note,
                        is_error,
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
                        |session_id, seq| Event::PlanModeChanged {
                            session_id,
                            seq,
                            enabled: true,
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
                    .push(ChatMsg::tool_result(&call.id, note.clone()));
                self.record(&RolloutRecord::ToolCall {
                    tool: call.name.clone(),
                    summary,
                    arguments: call.arguments.clone(),
                    output: note.clone(),
                    is_error,
                    edit: None,
                    agent_card: None,
                    agent_cards: vec![],
                });
                self.emit(
                    |session_id, seq| Event::ToolCallEnd {
                        session_id,
                        seq,
                        item_id,
                        output: note,
                        is_error,
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
                        self.history
                            .push(ChatMsg::tool_result(&call.id, note.clone()));
                        self.record(&RolloutRecord::ToolCall {
                            tool: call.name.clone(),
                            summary,
                            arguments: call.arguments.clone(),
                            output: note.clone(),
                            is_error,
                            edit: None,
                            // Agent card metadata is persisted with the record:
                            // replay rebuilds the agent card from it
                            agent_card: card,
                            agent_cards: cards,
                        });
                        self.emit(
                            |session_id, seq| Event::ToolCallEnd {
                                session_id,
                                seq,
                                item_id,
                                output: note,
                                is_error,
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
                                rest: &tool_calls[call_ix + 1..],
                                card,
                                cards,
                            },
                            tx,
                        );
                        self.emit(|session_id, seq| Event::TurnAborted { session_id, seq }, tx);
                        return StepOutcome::Ended;
                    }
                }
            }

            // AgentSwarm: batch parallel subagents. By default it blocks in the
            // foreground until all finish; with run_in_background it dispatches
            // each to the background and returns receipts immediately
            // (completions wake the parent one by one via <task-notification>).
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
                        self.history
                            .push(ChatMsg::tool_result(&call.id, note.clone()));
                        self.record(&RolloutRecord::ToolCall {
                            tool: call.name.clone(),
                            summary,
                            arguments: call.arguments.clone(),
                            output: note.clone(),
                            is_error,
                            edit: None,
                            agent_card: card,
                            agent_cards: cards,
                        });
                        self.emit(
                            |session_id, seq| Event::ToolCallEnd {
                                session_id,
                                seq,
                                item_id,
                                output: note,
                                is_error,
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
                                rest: &tool_calls[call_ix + 1..],
                                card,
                                cards,
                            },
                            tx,
                        );
                        self.emit(|session_id, seq| Event::TurnAborted { session_id, seq }, tx);
                        return StepOutcome::Ended;
                    }
                }
            }

            // ReadMediaFile capability gate: when the current model lacks image
            // input, guide toward switching models (no execution, no approval
            // popup; always visible in schemas, and calling it triggers the
            // guidance)
            if call.name == "ReadMediaFile" && !config.input_image {
                let note =
                    "The current model does not support image input; switch the model or enable the image-input capability in Settings.".to_string();
                self.history
                    .push(ChatMsg::tool_result(&call.id, note.clone()));
                self.record(&RolloutRecord::ToolCall {
                    tool: call.name.clone(),
                    summary,
                    arguments: call.arguments.clone(),
                    output: note.clone(),
                    is_error: true,
                    edit: None,
                    agent_card: None,
                    agent_cards: vec![],
                });
                self.emit(
                    |session_id, seq| Event::ToolCallEnd {
                        session_id,
                        seq,
                        item_id,
                        output: note,
                        is_error: true,
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
                    .push(ChatMsg::tool_result(&call.id, note.clone()));
                self.record(&RolloutRecord::ToolCall {
                    tool: call.name.clone(),
                    summary,
                    arguments: call.arguments.clone(),
                    output: note.clone(),
                    is_error: true,
                    edit: None,
                    agent_card: None,
                    agent_cards: vec![],
                });
                self.emit(
                    |session_id, seq| Event::ToolCallEnd {
                        session_id,
                        seq,
                        item_id,
                        output: note,
                        is_error: true,
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
                            .push(ChatMsg::tool_result(&call.id, note.clone()));
                        self.record(&RolloutRecord::ToolCall {
                            tool: call.name.clone(),
                            summary,
                            arguments: call.arguments.clone(),
                            output: note.clone(),
                            is_error: true,
                            edit: None,
                            agent_card: None,
                            agent_cards: vec![],
                        });
                        self.emit(
                            |session_id, seq| Event::ToolCallEnd {
                                session_id,
                                seq,
                                item_id,
                                output: note,
                                is_error: true,
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
                    |session_id, seq| Event::QuestionRequested {
                        session_id,
                        seq,
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
                                rest: &tool_calls[call_ix + 1..],
                                card: None,
                                cards: vec![],
                            },
                            tx,
                        );
                        self.emit(|session_id, seq| Event::TurnAborted { session_id, seq }, tx);
                        return StepOutcome::Ended;
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
                    edit: None,
                    agent_card: None,
                    agent_cards: vec![],
                });
                self.emit(
                    |session_id, seq| Event::ToolCallEnd {
                        session_id,
                        seq,
                        item_id,
                        output: note,
                        is_error: false,
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
            // session's own history and rollout)
            match self
                .exec_tool_gated(
                    call,
                    tool_ref.map(|t| t.as_ref()),
                    &item_id,
                    &turn_id,
                    tx,
                    cancel,
                )
                .await
            {
                GatedToolOutcome::Rejected { note } => {
                    self.history
                        .push(ChatMsg::tool_result(&call.id, note.clone()));
                    self.record(&RolloutRecord::ToolCall {
                        tool: call.name.clone(),
                        summary,
                        arguments: call.arguments.clone(),
                        output: note.clone(),
                        is_error: true,
                        edit: None,
                        agent_card: None,
                        agent_cards: vec![],
                    });
                    self.emit(
                        |session_id, seq| Event::ToolCallEnd {
                            session_id,
                            seq,
                            item_id,
                            output: note,
                            is_error: true,
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
                            rest: &tool_calls[call_ix + 1..],
                            card: None,
                            cards: vec![],
                        },
                        tx,
                    );
                    self.emit(|session_id, seq| Event::TurnAborted { session_id, seq }, tx);
                    return StepOutcome::Ended;
                }
                GatedToolOutcome::Executed {
                    output,
                    is_error,
                    edit,
                    images,
                } => {
                    // Images enter the model context via history (Anthropic
                    // blocks / OpenAI split user messages); the rollout ToolCall
                    // record stores only the output text (size summary
                    // included), base64 is not persisted
                    self.history.push(ChatMsg::tool_result_with_images(
                        &call.id,
                        output.clone(),
                        images,
                    ));
                    self.record(&RolloutRecord::ToolCall {
                        tool: call.name.clone(),
                        summary,
                        arguments: call.arguments.clone(),
                        output: output.clone(),
                        is_error,
                        edit: edit.clone(),
                        agent_card: None,
                        agent_cards: vec![],
                    });
                    self.emit(
                        |session_id, seq| Event::ToolCallEnd {
                            session_id,
                            seq,
                            item_id,
                            output,
                            is_error,
                            edit,
                        },
                        tx,
                    );
                }
            }
        }
        StepOutcome::ToolsExecuted
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
    async fn run_parallel_group(
        &mut self,
        tool_calls: &[ToolCall],
        start: usize,
        end: usize,
        turn_id: &str,
        tx: &async_channel::Sender<Event>,
        cancel: &CancellationToken,
    ) -> bool {
        /// Cap on calls executing simultaneously within a parallel group
        const MAX_PARALLEL: usize = 8;

        // Emit Begin for all group members in original order and precompute
        // card info (summary is persisted with the rollout)
        let group = &tool_calls[start..end];
        let mut cards: Vec<ParallelCall> = Vec::with_capacity(group.len());
        for call in group {
            let item_id = format!("{turn_id}-tool-{}", call.id);
            let detail = serde_json::from_str::<serde_json::Value>(&call.arguments)
                .map(|v| serde_json::to_string_pretty(&v).unwrap_or_default())
                .unwrap_or_else(|_| call.arguments.clone());
            let summary = tool::summarize(call);
            self.emit(
                |session_id, seq| Event::ToolCallBegin {
                    session_id,
                    seq,
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
                set.spawn(async move {
                    let mut tracker = ChangeTracker::default();
                    let ctx = ToolContext {
                        cwd: &cwd,
                        tracker: &mut tracker,
                        state: &state,
                        // Group members are fs_outside-free by the mask; the
                        // grant lives on the serial gate path only
                        fs_grant: None,
                    };
                    let mut extra: Vec<Box<dyn tool::Tool>> = match &mcp {
                        Some(mcp) if call.name.starts_with("mcp__") => {
                            mcp.tool_named(&call.name).into_iter().collect()
                        }
                        _ => vec![],
                    };
                    // Skill is read-only and concurrentable, served via the
                    // extra channel (same as the serial gate)
                    if call.name == "Skill" {
                        extra.push(Box::new(tool::SkillTool::new(&cwd, &data_dir)));
                    }
                    let (output, is_error, file_change, edit, images) =
                        tool::execute_with_extra(&call, ctx, &extra).await;
                    debug_assert!(
                        file_change.is_none() && tracker.take_dirty().is_empty(),
                        "concurrent read-only segment produces no file changes: {}",
                        call.name
                    );
                    let images = tool_images_to_chat(&call.arguments, images);
                    (
                        index,
                        ParallelOutput {
                            output,
                            is_error,
                            edit,
                            images,
                            file_change,
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
                        Some(Ok((index, out))) => {
                            self.emit(
                                |session_id, seq| Event::ToolCallEnd {
                                    session_id,
                                    seq,
                                    item_id: cards[index].item_id.clone(),
                                    output: out.output.clone(),
                                    is_error: out.is_error,
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
                if let Ok((index, out)) = joined {
                    self.emit(
                        |session_id, seq| Event::ToolCallEnd {
                            session_id,
                            seq,
                            item_id: cards[index].item_id.clone(),
                            output: out.output.clone(),
                            is_error: out.is_error,
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
                    let ParallelOutput {
                        output,
                        is_error,
                        edit,
                        images,
                        file_change,
                    } = out;
                    self.history.push(ChatMsg::tool_result_with_images(
                        &card.call.id,
                        output.clone(),
                        images,
                    ));
                    self.record(&RolloutRecord::ToolCall {
                        tool: card.call.name.clone(),
                        summary: card.summary.clone(),
                        arguments: card.call.arguments.clone(),
                        output,
                        is_error,
                        edit,
                        agent_card: None,
                        agent_cards: vec![],
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
                            |session_id, seq| Event::FileChanged {
                                session_id,
                                seq,
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
                            rest: &[],
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
                        .push(ChatMsg::tool_result(&card.call.id, note.clone()));
                    self.record(&RolloutRecord::ToolCall {
                        tool: card.call.name.clone(),
                        summary: card.summary.clone(),
                        arguments: card.call.arguments.clone(),
                        output: note.clone(),
                        is_error: true,
                        edit: None,
                        agent_card: None,
                        agent_cards: vec![],
                    });
                    self.emit(
                        |session_id, seq| Event::ToolCallEnd {
                            session_id,
                            seq,
                            item_id: card.item_id.clone(),
                            output: note,
                            is_error: true,
                            edit: None,
                        },
                        tx,
                    );
                }
            }
        }

        if cancelled {
            // Calls after the group never got a Begin: only history receipts
            // are backfilled to keep pairing (settle's rest semantics)
            for rest in &tool_calls[end..] {
                self.history
                    .push(ChatMsg::tool_result(&rest.id, "Stopped".to_string()));
            }
            self.emit(|session_id, seq| Event::TurnAborted { session_id, seq }, tx);
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
    images: Vec<crate::provider::ChatImage>,
    file_change: Option<tool::FileChange>,
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
/// input; not concurrent. MCP tools: besides read_only (readOnlyHint) +
/// approval-free, they must pass a project deny-rule precheck — the concurrent
/// path bypasses exec_tool_gated_ctx, so the serial gate's deny check is
/// duplicated here (subject is the full tool name, same as serial). McpClient
/// request multiplexing is verified concurrency-safe (stdio: AtomicU64 id +
/// Mutex pending map + stdin write lock; http: one independent POST per
/// request).
fn parallel_safe(
    call: &ToolCall,
    tools: &[Box<dyn tool::Tool>],
    mode: ExecMode,
    input_image: bool,
    permissions: &crate::permissions::PermissionRules,
    state: &crate::task::SessionToolState,
    cwd: &std::path::Path,
) -> bool {
    if PARALLEL_EXCLUDED.contains(&call.name.as_str()) {
        return false;
    }
    // Out-of-workspace target with the toggles off needs the approval popup —
    // only the serial gate can request one, so such calls are sync points
    if tool::fs_outside_intent(state, cwd, &call.name, &call.arguments).is_some() {
        return false;
    }
    if call.name == "ReadMediaFile" && !input_image {
        return false;
    }
    let Some(tool_ref) = tools.iter().find(|t| t.name() == call.name) else {
        return false;
    };
    if !tool_ref.read_only() || tool::requires_approval(tool_ref.as_ref(), mode) {
        return false;
    }
    // Project deny precheck for MCP tools (hit → falls back to a serial sync
    // point; the full gate produces the rejection message)
    if call.name.starts_with("mcp__") && permissions.deny_hit(&call.name, &call.name).is_some() {
        return false;
    }
    true
}

/// Slices one step's tool_calls into a parallel-segment mask: true = may join
/// a parallel read-only group; false = sync point (writes/shell/needs
/// approval/session-layer interception/unknown tool/MCP deny hit), executed
/// serially on its own after the preceding group drains.
fn parallel_mask(
    calls: &[ToolCall],
    tools: &[Box<dyn tool::Tool>],
    mode: ExecMode,
    input_image: bool,
    permissions: &crate::permissions::PermissionRules,
    state: &crate::task::SessionToolState,
    cwd: &std::path::Path,
) -> Vec<bool> {
    calls
        .iter()
        .map(|call| parallel_safe(call, tools, mode, input_image, permissions, state, cwd))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Out-of-workspace read targets demote to serial sync points (only the
    /// gate can pop the approval); inside paths, tmp, and the toggle-on state
    /// stay parallel-eligible
    #[test]
    fn parallel_mask_demotes_outside_targets() {
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
        let (wake_notify, _w) = tokio::sync::mpsc::unbounded_channel();
        let state = crate::task::SessionToolState::new(
            "mask-out".to_string(),
            task_notify,
            wake_notify,
            vec![],
        );
        let tools = tool::all();
        let rules = crate::permissions::PermissionRules::default();
        let call = |path: &str| ToolCall {
            id: "c1".to_string(),
            name: "Read".to_string(),
            arguments: format!("{{\"path\": \"{path}\"}}"),
        };
        assert!(
            !parallel_safe(
                &call(&outside_str),
                &tools,
                ExecMode::AutoEdit,
                true,
                &rules,
                &state,
                &ws,
            ),
            "outside target must fall to the serial gate for its approval"
        );
        assert!(
            parallel_safe(
                &call("README.mock.md"),
                &tools,
                ExecMode::AutoEdit,
                true,
                &rules,
                &state,
                &ws,
            ),
            "inside path stays parallel"
        );
        state
            .fs_read_outside
            .store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(
            parallel_safe(
                &call(&outside_str),
                &tools,
                ExecMode::AutoEdit,
                true,
                &rules,
                &state,
                &ws,
            ),
            "toggle on restores parallel eligibility"
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
        let (wake_notify, _w_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = crate::task::SessionToolState::new(
            "mask-test".to_string(),
            task_notify,
            wake_notify,
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
            mode,
            input_image,
            &permissions,
            &state,
            std::path::Path::new("/ws"),
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
            ExecMode::ConfirmBeforeEdit,
            ExecMode::AutoEdit,
            ExecMode::FullAccess,
            ExecMode::Yolo,
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
                "Bash",
                "TodoList",
                "TaskStop",
                "Agent",
                "AskUserQuestion",
                "EnterPlanMode",
                "ExitPlanMode",
                "NoSuchTool",
            ],
            ExecMode::Yolo,
            true,
        );
        assert!(mask.iter().all(|m| !*m), "{mask:?}");
    }

    #[test]
    fn read_media_file_needs_image_capability() {
        assert_eq!(
            mask_of(&["ReadMediaFile"], ExecMode::FullAccess, true),
            [true]
        );
        assert_eq!(
            mask_of(&["ReadMediaFile"], ExecMode::FullAccess, false),
            [false]
        );
    }

    #[test]
    fn segments_split_on_sync_points() {
        // Read Read | Write | Grep Glob | Bash | Read — writes/shell split
        // segments
        let mask = mask_of(
            &["Read", "Read", "Write", "Grep", "Glob", "Bash", "Read"],
            ExecMode::AutoEdit,
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
            ExecMode::ConfirmBeforeEdit,
            ExecMode::AutoEdit,
            ExecMode::FullAccess,
            ExecMode::Yolo,
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
            ExecMode::Yolo,
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
        let mask = mask_of(&["mcp__ghost__read"], ExecMode::Yolo, false);
        assert_eq!(mask, [false]);
    }

    fn pointer_test_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pig-pointer-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
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
