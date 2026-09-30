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

        // 系统提示词全部用会话冻结快照（git/AGENTS.md/技能/日期），模式已移入
        // turn_reminder——会话内字节稳定，前缀缓存最大化
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
        // 回合边界 reminder（执行模式/日期跨天/AGENTS.md 变更）：prepend 到用户
        // 消息前——尾部注入不打断 system+历史的前缀缓存，也插不进工具配对中间；
        // 不落 rollout（恢复会话由重新冻结 + 首轮提醒自愈）
        let fresh_agents = prompt::agents_md(&self.data_dir, &self.cwd);
        let reminder = prompt::turn_reminder(
            self.mode,
            &self.date_frozen,
            &mut self.date_reminded,
            &self.agents_prompt,
            &fresh_agents,
            &mut self.agents_reminded,
        );
        let mut user_text = expand_file_references(&self.cwd, &content, &files);
        user_text = format!("{reminder}\n\n{user_text}");
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
        // MCP 懒连接（每会话一次）：读 .pigcode/mcp.json + data_dir/mcp.json，
        // 无配置时得到空 manager，开销可忽略；单 server 失败不影响其他
        if self.mcp.is_none() {
            self.mcp = Some(Arc::new(
                crate::mcp::McpManager::connect_all(&self.cwd, &self.data_dir).await,
            ));
        }
        // 采样前检查水位：超过 context_window - max_output_tokens - 13k 缓冲就先自动 compact
        if let Some(used) = self.last_total_tokens {
            let threshold = config
                .context_window
                .saturating_sub(config.max_output_tokens + 13_000);
            if used > threshold && !self.run_compact(Some(config), true, tx, cancel).await {
                return StepOutcome::Ended;
            }
        }

        let text_item = format!("{turn_id}-text-{step}");
        let reasoning_item = format!("{turn_id}-reason-{step}");
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let api_started = Instant::now();
        let provider_task = tokio::spawn(provider::stream_chat(
            config.clone(),
            self.history.clone(),
            // 根会话工具集 = 内置 + Agent/AgentSwarm + MCP（每步重建：档案与 MCP 工具可增改）
            self.root_schemas(),
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

        let tools = self.root_tools();
        // P0 分组并发：连续「可安全并发」的只读调用切成并发组（JoinSet，上限 8）；
        // 不可并发的调用是同步点——前面的组排干后走下方原有串行路径（拦截语义不变）
        let mask = parallel_mask(
            &tool_calls,
            &tools,
            self.mode,
            config.input_image,
            &self.permissions,
        );
        let mut next_ix = 0usize;
        for (call_ix, call) in tool_calls.iter().enumerate() {
            if call_ix < next_ix {
                continue; // 已随前面的并发组执行完毕
            }
            if mask[call_ix] {
                let mut group_end = call_ix + 1;
                while group_end < tool_calls.len() && mask[group_end] {
                    group_end += 1;
                }
                next_ix = group_end;
                // 取消收尾（补回执 + TurnAborted）在组内完成，false 即回合终止
                if !self
                    .run_parallel_group(&tool_calls, call_ix, group_end, &turn_id, tx, cancel)
                    .await
                {
                    return StepOutcome::Ended;
                }
                continue;
            }
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
                    // 计划确认是一次性弹窗，不参与同键合并决议
                    self.pending
                        .lock()
                        .expect("pending lock")
                        .insert(request_id.clone(), (reply_tx, None));
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
                            // 代理卡元信息随记录持久化：回放经它重建代理卡
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

            // AgentSwarm：批量并行子代理。默认前台阻塞至全部完成；run_in_background
            // 时逐个后台派发、立即返回回执（完成经 <task-notification> 逐个唤醒）。
            // 与 Agent 同点拦截——Plan 拒绝文案由 run_swarm 内部给出；
            // 各子代理的写操作仍各自过审批门。
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

    /// 并发只读组执行（P0）：组内调用经 parallel_mask 判定为只读、当前模式免审批、
    /// 无会话层拦截（Agent/AskUserQuestion/计划模式切换等同步点都在串行路径）。
    /// ToolCallBegin 按原序先发（卡片顺序 = 原始顺序）；ToolCallEnd 随完成即达
    ///（item_id 寻址，TUI find_or_create 容忍乱序，回放由 rollout 记录序重建）；
    /// history/rollout 在组排干后按原 index 补齐，tool_result 配对顺序不乱。
    /// 返回 false = 被取消：在跑任务已 abort 排干、未完成的调用已补「已停止」回执、
    /// 组后剩余调用已补历史回执、TurnAborted 已发（调用方直接 Ended）。
    async fn run_parallel_group(
        &mut self,
        tool_calls: &[ToolCall],
        start: usize,
        end: usize,
        turn_id: &str,
        tx: &async_channel::Sender<Event>,
        cancel: &CancellationToken,
    ) -> bool {
        /// 并发组内同时执行的调用数上限
        const MAX_PARALLEL: usize = 8;

        // 全组成员按原序发 Begin 并预计算卡片信息（summary 随 rollout 持久化）
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

        // 并发执行：只读工具不触碰 ChangeTracker（各任务持一次性实例，debug 断言兜底）；
        // 会话共享态（read_states/todos/tasks）全是 Arc<Mutex>/atomic，本就为前后台共享设计，
        // 并发读安全；写互斥由分组保证（写工具是同步点，组排干后才会执行）。
        // MCP 工具：mask 已保证只读+免审批+无 deny 命中；任务内 clone Arc<McpManager>
        // 按名现取（McpTool clone 即 Arc 克隆，便宜），经 execute_with_extra 执行
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
                    };
                    let mut extra: Vec<Box<dyn tool::Tool>> = match &mcp {
                        Some(mcp) if call.name.starts_with("mcp__") => {
                            mcp.tool_named(&call.name).into_iter().collect()
                        }
                        _ => vec![],
                    };
                    // Skill 只读可并发，走 extra 通道（串行门控同款）
                    if call.name == "Skill" {
                        extra.push(Box::new(tool::SkillTool::new(&cwd, &data_dir)));
                    }
                    let (output, is_error, file_change, edit, images) =
                        tool::execute_with_extra(&call, ctx, &extra).await;
                    debug_assert!(
                        file_change.is_none() && tracker.take_dirty().is_empty(),
                        "并发只读段不产生文件改动: {}",
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
                        // panic/abort：槽位留空，下方收尾统一兜底
                        Some(Err(_)) => {}
                        None => break false,
                    }
                }
                _ = cancel.cancelled() => break true,
            }
        };

        if cancelled {
            // 中止在跑任务并排干：阻塞读会跑完当前 fs 调用后在下一让出点退出，
            // JoinSet 析构兜底 abort，不留孤儿；排干窗口内刚好完成的按正常完成落定
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

        // 统一收尾（严格原序）：history 与 rollout 按原 index 补齐
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
                    // 防御：分类保证只读无改动；未来误标 read_only 的变更工具不丢数据
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
                    // 取消：组内成员都发过 Begin，逐卡落定「已停止」（rest 语义在下方统一处理）
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
                    // 正常路径的空槽 = 任务 panic：补错误回执保持 tool_use 配对完整
                    let note = "工具执行内部错误（任务异常终止）".to_string();
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
            // 组后剩余调用没发过 Begin：只补历史回执保持配对（settle 的 rest 语义）
            for rest in &tool_calls[end..] {
                self.history
                    .push(ChatMsg::tool_result(&rest.id, "已停止".to_string()));
            }
            self.emit(|session_id, seq| Event::TurnAborted { session_id, seq }, tx);
        }
        !cancelled
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

/// 并发只读组内一个调用的预计算卡片信息（Begin 先发，End 随完成即达）
struct ParallelCall {
    call: ToolCall,
    item_id: String,
    summary: String,
}

/// 并发只读组单个调用的执行产物（与 GatedToolOutcome::Executed 同构，
/// 外加 file_change 防御通道——分类保证只读恒为 None，误标时不丢数据）
struct ParallelOutput {
    output: String,
    is_error: bool,
    edit: Option<pig_protocol::EditDiff>,
    images: Vec<crate::provider::ChatImage>,
    file_change: Option<tool::FileChange>,
}

/// 并发只读段排除名单：虽声明 read_only() 但有会话副作用或会话层拦截语义，
/// 一律落回串行同步点。AskUserQuestion/EnterPlanMode/ExitPlanMode/Agent 的
/// 拦截逻辑在 run_step 串行体内，语义不变（天然同步点）。
const PARALLEL_EXCLUDED: &[&str] = &[
    "TodoList",        // 写变体改 todos 并落库推事件
    "TaskStop",        // 停止后台任务，是变更操作
    "AskUserQuestion", // 会话层弹窗拦截
    "EnterPlanMode",   // 模式切换拦截
    "ExitPlanMode",    // 模式切换拦截（带审批弹窗）
    "Agent",           // 子代理委派拦截
];

/// 单调用并发安全判定（保守原则，全部满足才可并发）：
/// 已知工具、read_only、当前模式免审批（read_only 工具在现有审批矩阵下全模式免审批，
/// 仍走 requires_approval 同一判定防矩阵变更后回归）。工作区外访问在本代码库是
/// 硬错误/会话开关门控（resolve_with_access），不产生审批弹窗，敏感文件在工具内部
/// 无条件硬拒——审批弹窗只会来自危险命令/requires_approval 分支，并发组成员按此
/// 分类永远不会进入那两个分支，因此不可能出现两个审批弹窗并发。
/// ReadMediaFile 在模型不支持图片输入时被会话层能力门控拦截，不可并发。
/// MCP 工具：read_only（readOnlyHint）+ 免审批之外还要过项目 deny 规则预检——
/// 并发路径不走 exec_tool_gated_ctx，串行门控里的 deny 判定在此补齐
/// （subject 口径与串行一致 = 工具全名）。McpClient 请求多路复用已核实并发安全
///（stdio：AtomicU64 id + Mutex pending map + stdin 写锁；http：每请求独立 POST）。
fn parallel_safe(
    call: &ToolCall,
    tools: &[Box<dyn tool::Tool>],
    mode: ExecMode,
    input_image: bool,
    permissions: &crate::permissions::PermissionRules,
) -> bool {
    if PARALLEL_EXCLUDED.contains(&call.name.as_str()) {
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
    // MCP 工具的项目 deny 预检（命中 → 落回串行同步点，走完整门控给出拒绝文案）
    if call.name.starts_with("mcp__") && permissions.deny_hit(&call.name, &call.name).is_some() {
        return false;
    }
    true
}

/// 把一个 step 的 tool_calls 切成并发段掩码：true = 可进并发只读组；
/// false = 同步点（写/壳/需审批/会话层拦截/未知工具/deny 命中的 MCP），
/// 排干前组后单独串行执行。
fn parallel_mask(
    calls: &[ToolCall],
    tools: &[Box<dyn tool::Tool>],
    mode: ExecMode,
    input_image: bool,
    permissions: &crate::permissions::PermissionRules,
) -> Vec<bool> {
    calls
        .iter()
        .map(|call| parallel_safe(call, tools, mode, input_image, permissions))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mask_of(names: &[&str], mode: ExecMode, input_image: bool) -> Vec<bool> {
        mask_with(names, &tool::all(), mode, input_image, "无规则")
    }

    /// 带工具集与权限规则的掩码：rules 为 permissions.toml 文本（"无规则" 特例 = 空规则）
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
        let permissions = if rules_toml == "无规则" {
            crate::permissions::PermissionRules::default()
        } else {
            crate::permissions::PermissionRules::parse(rules_toml).expect("规则合法")
        };
        parallel_mask(&calls, tools, mode, input_image, &permissions)
    }

    /// 造一个 MCP 工具（for_test 假连接，只关心 name/read_only 判定）
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
            ExecMode::Plan,
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
        // Read Read | Write | Grep Glob | Bash | Read —— 写/壳把段切开
        let mask = mask_of(
            &["Read", "Read", "Write", "Grep", "Glob", "Bash", "Read"],
            ExecMode::AutoEdit,
            false,
        );
        assert_eq!(mask, [true, true, false, true, true, false, true]);
    }

    #[test]
    fn plan_mode_keeps_reads_parallel() {
        let mask = mask_of(&["Read", "Write"], ExecMode::Plan, false);
        assert_eq!(mask, [true, false]);
    }

    // ---------- MCP 工具进并发组 ----------

    #[test]
    fn mcp_readonly_parallel_safe_in_all_modes() {
        let tools: Vec<Box<dyn tool::Tool>> = tool::all()
            .into_iter()
            .chain(vec![mcp_tool("read", true), mcp_tool("write", false)])
            .collect();
        for mode in [
            ExecMode::ConfirmBeforeEdit,
            ExecMode::AutoEdit,
            ExecMode::Plan,
            ExecMode::FullAccess,
            ExecMode::Yolo,
        ] {
            let mask = mask_with(
                &["mcp__srv__read", "mcp__srv__write"],
                &tools,
                mode,
                false,
                "无规则",
            );
            assert_eq!(
                mask,
                [true, false],
                "{mode:?}: 只读 MCP 可并发，写 MCP 同步点"
            );
        }
    }

    #[test]
    fn mcp_denied_by_project_rule_falls_back_to_serial() {
        let tools: Vec<Box<dyn tool::Tool>> = tool::all()
            .into_iter()
            .chain(vec![mcp_tool("read", true)])
            .collect();
        // deny 规则命中（工具全名 subject，与串行门控同口径）→ 回串行
        let mask = mask_with(
            &["mcp__srv__read"],
            &tools,
            ExecMode::Yolo,
            false,
            "deny = [\"mcp__srv__read(*)\"]",
        );
        assert_eq!(mask, [false], "deny 命中的 MCP 工具不可并发");
    }

    #[test]
    fn mcp_unknown_tool_not_parallel() {
        // 工具集里查不到的 mcp__ 名（未连接/未继承）→ 同步点
        let mask = mask_of(&["mcp__ghost__read"], ExecMode::Yolo, false);
        assert_eq!(mask, [false]);
    }
}
