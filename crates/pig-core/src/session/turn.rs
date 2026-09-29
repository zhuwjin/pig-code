use super::*;

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

        let system = ChatMsg::system(prompt::system_prompt(
            &self.cwd,
            true,
            self.mode,
            &self.data_dir,
            &config.model,
            self.git_snapshot.as_deref(),
        ));
        if self.history.is_empty() {
            self.history.push(system);
        } else if self.history[0].role == "system" {
            self.history[0] = system;
        }
        let mut user_text = expand_file_references(&self.cwd, &content, &files);
        if self.history.len() == 1 {
            // 首条消息：种标题（首 30 字符兜底），并异步生成模型标题；
            // 手动重命名过（title_custom）两者都不覆盖
            let title: String = content.chars().take(30).collect();
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
        // user_text 含展开后的文件内容；rollout 只记原文
        let rollout_text = if files.is_empty() {
            content.clone()
        } else {
            format!("{content}\n\n引用文件: {}", files.join(", "))
        };
        let record_files = files.clone();
        // 粘贴图片（ZCode 式管线）：压缩 → 落会话媒体目录 → rollout 记 ImageRef
        //（不存 base64）+ history 进 ChatImage；压缩失败的图跳过并在文本里记 note。
        // 文件名目录内续排（next_media_index）：按消息内序号命名会被后续回合覆盖
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
                            user_text.push_str(&format!("\n[图片 {} 落盘失败: {error}]", ix + 1));
                            continue;
                        }
                        // 压缩附注（kimi-code caption 思路）：缩放/转码改变了图就在
                        // 文本里告知模型，原图落盘供 ReadMediaFile region 看高清局部
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
                            label: Some(format!("图片 {}", ix + 1)),
                        });
                    }
                    Err(error) => {
                        user_text.push_str(&format!("\n[图片 {} 压缩失败: {error}]", ix + 1));
                    }
                }
            }
        }
        let image_count = image_refs.len();
        // 能力投影：模型不支持图片输入 → 不进 ChatMsg.images，文本占位告知（带媒体路径）
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
        // 事件文本带附件链接（UI 渲染缩略图用）；history/rollout 是干净文本
        let display_text = crate::rollout::user_display_text(&rollout_text, &image_refs);
        self.record(&RolloutRecord::User {
            text: rollout_text.clone(),
            files: record_files.clone(),
            images: image_refs,
        });
        self.emit(
            |session_id, seq| Event::UserMessage {
                session_id,
                seq,
                text: display_text,
                files: record_files.clone(),
                image_count,
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
                        );
                        // 回合统计持久化：回放恢复 footer 与会话累计（水位由 StepUsage 恢复）
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
                    // 本轮改动面板先于回合结束事件发出（durable 数据先于边界事件）
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
                    // 中断/失败收尾：本轮已发生的修改也要产出面板
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
        // 采样前检查水位：超过 context_window - max_output_tokens - 13k 缓冲就先自动 compact
        if let Some(used) = self.last_total_tokens {
            let threshold = config
                .context_window
                .saturating_sub(config.max_output_tokens + 13_000);
            if used > threshold {
                if !self.run_compact(Some(config), true, tx, cancel).await {
                    return StepOutcome::Ended;
                }
            }
        }

        let text_item = format!("{turn_id}-text-{step}");
        let reasoning_item = format!("{turn_id}-reason-{step}");
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let api_started = Instant::now();
        let provider_task = tokio::spawn(provider::stream_chat(
            config.clone(),
            self.history.clone(),
            // 根会话工具集 = 内置 + Agent（每步重建：档案文件可在回合间增改）
            tool::schemas_root(&self.cwd, &self.data_dir),
            event_tx,
            cancel.clone(),
        ));

        let mut text = String::new();
        let mut reasoning = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut provider_failed = false;
        // 首个输出 token（思考/正文增量）到达时刻：TTFT = 该时刻 - 请求发出
        let mut first_token_at: Option<Instant> = None;

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
                }) => {
                    self.turn_input += input;
                    self.turn_cache_read += cache_read;
                    self.turn_output += output;
                    self.input_total += input;
                    self.cache_read_total += cache_read;
                    self.last_total_tokens = Some(used);
                    // 每次请求的用量即时落盘（durable 先于事件），回放用最后一条恢复水位
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
                    self.emit(
                        |session_id, seq| Event::Error {
                            session_id: Some(session_id),
                            seq,
                            message: error,
                        },
                        tx,
                    );
                    provider_failed = true;
                    break;
                }
            }
        }
        // 纯 API 耗时：请求发出到流结束（含失败请求），不含工具执行与审批等待；
        // TTFT 到首个输出 token（纯 tool_call 响应没有增量事件，TTFT 记 0）
        let api_elapsed = api_started.elapsed();
        self.turn_api_ms += api_elapsed.as_millis() as u64;
        self.turn_api_steps += 1;
        self.turn_ttft_ms += first_token_at
            .map(|at| at.duration_since(api_started).as_millis() as u64)
            .unwrap_or(0)
            .min(api_elapsed.as_millis() as u64);

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
        // 思考内容随 assistant 消息进历史：Anthropic thinking 模式要求回传
        self.history.push(ChatMsg::assistant(
            text,
            tool_calls.clone(),
            Some(reasoning).filter(|r| !r.is_empty()),
        ));
        if tool_calls.is_empty() {
            return StepOutcome::TextOnly;
        }

        let tools = tool::all();
        for (call_ix, call) in tool_calls.iter().enumerate() {
            let item_id = format!("{}-tool-{}", turn_id, call.id);
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

            let tool_ref = tools.iter().find(|t| t.name() == call.name);
            let read_only = tool_ref.is_some_and(|t| t.read_only());

            // ExitPlanMode：在 Plan 硬拒之前拦截（它是退出计划模式的唯一出口，
            // 强制弹窗请用户确认；复用 ApprovalRequested 通道，UI 无需新组件）。
            if call.name == "ExitPlanMode" {
                let args: serde_json::Value =
                    serde_json::from_str(&call.arguments).unwrap_or_default();
                let plan = args["plan"].as_str().unwrap_or("");
                let (note, is_error);
                if self.mode != ExecMode::Plan {
                    note = "仅在计划模式下可用".to_string();
                    is_error = true;
                } else {
                    let request_id = format!("{}-{turn_id}-exitplan-{item_id}", self.id);
                    let (reply_tx, reply_rx) = oneshot::channel();
                    self.pending
                        .lock()
                        .expect("pending lock")
                        .insert(request_id.clone(), reply_tx);
                    let plan_preview: String = plan.chars().take(500).collect();
                    self.emit(
                        |session_id, seq| Event::ApprovalRequested {
                            session_id,
                            seq,
                            request_id: request_id.clone(),
                            tool: call.name.clone(),
                            detail: format!("模型请求结束计划模式并开始执行\n\n{plan_preview}"),
                        },
                        tx,
                    );
                    let decision = tokio::select! {
                        reply = reply_rx => reply.unwrap_or(ApprovalDecision::Reject),
                        _ = cancel.cancelled() => {
                            self.pending.lock().expect("pending lock").remove(&request_id);
                            self.settle_cancelled_tool(
                                CancelledTool {
                                    call,
                                    summary,
                                    item_id: &item_id,
                                    rest: &tool_calls[call_ix + 1..],
                                    card: None,
                                },
                                tx,
                            );
                            self.emit(|session_id, seq| Event::TurnAborted { session_id, seq }, tx);
                            return StepOutcome::Ended;
                        }
                    };
                    match decision {
                        ApprovalDecision::Allow | ApprovalDecision::AlwaysAllow => {
                            // 恢复 EnterPlanMode 前的模式（没记录则回落「变更前确认」）：
                            // 写穿 store + 发事件让 UI 模式 chip 实时更新
                            let restored = self
                                .pre_plan_mode
                                .take()
                                .unwrap_or(ExecMode::ConfirmBeforeEdit);
                            self.mode = restored;
                            let session_id = self.id.clone();
                            self.store.lock().expect("store lock").update_session(
                                &session_id,
                                |m| {
                                    m.exec_mode = restored;
                                },
                            );
                            self.emit(
                                |session_id, seq| Event::ExecModeChanged {
                                    session_id,
                                    seq,
                                    mode: restored,
                                },
                                tx,
                            );
                            note = format!(
                                "已切换到「{}」模式，请开始执行计划。",
                                exec_mode_label(restored)
                            );
                            is_error = false;
                        }
                        ApprovalDecision::Reject => {
                            note = "用户拒绝退出计划模式，请继续完善计划或回答疑问。".to_string();
                            is_error = true;
                        }
                    }
                }
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

            // EnterPlanMode：进计划是自我收紧（只读化），直接切换不弹窗。
            // 记录 pre_plan_mode，ExitPlanMode 确认后恢复原模式。
            if call.name == "EnterPlanMode" {
                let (note, is_error) = if self.mode == ExecMode::Plan {
                    ("已在计划模式，请继续调研并输出计划。".to_string(), false)
                } else {
                    self.pre_plan_mode = Some(self.mode);
                    self.mode = ExecMode::Plan;
                    let session_id = self.id.clone();
                    self.store
                        .lock()
                        .expect("store lock")
                        .update_session(&session_id, |m| {
                            m.exec_mode = ExecMode::Plan;
                        });
                    self.emit(
                        |session_id, seq| Event::ExecModeChanged {
                            session_id,
                            seq,
                            mode: ExecMode::Plan,
                        },
                        tx,
                    );
                    (
                        "已切换到计划模式。接下来只能使用只读工具调研，计划写好后调用 ExitPlanMode 请用户确认执行。"
                            .to_string(),
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

            // Agent：委派子代理（前台同步）。拦在 ReadMediaFile 门控与 Plan 硬拒之前
            // ——Plan 拒绝文案由 run_subagent 内部给出（比通用硬拒更贴合语义）。
            // 子工具调用不发顶层 ToolCallBegin/End：父时间线只有 Agent 一张卡，
            // 实时进度走 SubagentProgress，审批仍弹（子代理的写操作自己过审批门）。
            if call.name == "Agent" {
                match self
                    .run_subagent(call, &turn_id, &item_id, config, tx, cancel)
                    .await
                {
                    SubagentOutcome::Finished {
                        note,
                        is_error,
                        card,
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
                            // 代理卡元信息随记录持久化：回放经它重建代理卡
                            agent_card: card,
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
                    SubagentOutcome::Cancelled { card } => {
                        self.settle_cancelled_tool(
                            CancelledTool {
                                call,
                                summary,
                                item_id: &item_id,
                                rest: &tool_calls[call_ix + 1..],
                                card,
                            },
                            tx,
                        );
                        self.emit(|session_id, seq| Event::TurnAborted { session_id, seq }, tx);
                        return StepOutcome::Ended;
                    }
                }
            }

            // ReadMediaFile 能力门控：当前模型不支持图片输入时直接引导换模型
            //（不执行、不弹审批；schemas 里始终可见，模型调了就被引导）
            if call.name == "ReadMediaFile" && !config.input_image {
                let note =
                    "当前模型不支持图片输入，请在设置里更换模型或勾选图片输入能力".to_string();
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

            if self.mode == ExecMode::Plan && !read_only {
                let note = "计划模式：修改类工具已被禁止执行。请只输出计划文本，等用户切换到其他模式后再执行。"
                    .to_string();
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

            // AskUserQuestion：结构化提问在会话层拦截执行（工具本身只注册 schema）。
            // read_only，无需审批；Esc 跳过（None）不算错误。
            if call.name == "AskUserQuestion" {
                let args: serde_json::Value =
                    serde_json::from_str(&call.arguments).unwrap_or_default();
                let questions = match tool::parse_questions(&args) {
                    Ok(questions) => questions,
                    Err(error) => {
                        let note = format!("AskUserQuestion 参数非法: {error}");
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
                    // sender 被 drop（回复方消失）按跳过处理
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
                            },
                            tx,
                        );
                        self.emit(|session_id, seq| Event::TurnAborted { session_id, seq }, tx);
                        return StepOutcome::Ended;
                    }
                };
                let note = match &reply {
                    Some(answers) => {
                        let mut text = "用户已回答：\n".to_string();
                        for (ix, question) in questions.iter().enumerate() {
                            let labels = answers
                                .get(ix)
                                .map(|labels| labels.join("、"))
                                .filter(|s| !s.is_empty())
                                .unwrap_or_else(|| "（未选择）".to_string());
                            text.push_str(&format!(
                                "{}. {}：{}\n",
                                ix + 1,
                                question.question,
                                labels
                            ));
                        }
                        text
                    }
                    None => "用户选择不回答，请根据上下文自行决定并继续。".to_string(),
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

            // 通用路径：危险黑名单/项目权限规则/审批门/执行/会话级副作用全部在
            // exec_tool_gated（子代理循环复用同一门控）；本处只收尾
            // history/rollout/ToolCallEnd（父会话自己的历史与 rollout）
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
                    // 图片随 history 进模型上下文（Anthropic blocks / OpenAI 拆 user 消息）；
                    // rollout 的 ToolCall 记录只存 output 文本（尺寸摘要在内），base64 不落盘
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


}

/// @文件引用展开：内容注入 <file> 块；单文件 20KB、总计 100KB 上限。
fn expand_file_references(cwd: &Path, content: &str, files: &[String]) -> String {
    let mut text = content.to_string();
    let mut budget = 100 * 1024;
    for file in files {
        let block = match tool::resolve_checked(cwd, file, false)
            .and_then(|full| std::fs::read_to_string(&full).map_err(|e| e.to_string()))
        {
            Ok(mut file_content) => {
                if file_content.len() > 20 * 1024 {
                    file_content.truncate(20 * 1024);
                    file_content.push_str("\n[文件过大，已截断]");
                }
                if file_content.len() > budget {
                    file_content.truncate(budget);
                    file_content.push_str("\n[引用总量超限，已截断]");
                }
                budget = budget.saturating_sub(file_content.len());
                format!("\n\n<file path=\"{file}\">\n{file_content}\n</file>")
            }
            Err(error) => format!("\n\n[无法读取引用文件 {file}: {error}]"),
        };
        text.push_str(&block);
    }
    text
}
