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
        // 回合边界 reminder（执行模式/计划开关首轮或切换、日期跨天、AGENTS.md 变更）：
        // prepend 到用户消息前——尾部注入不打断 system+历史的前缀缓存，也插不进
        // 工具配对中间；无可提醒内容时用户消息保持原样。不落 rollout（恢复会话
        // 由重新冻结 + 首轮提醒自愈）
        let fresh_agents = prompt::agents_md(&self.data_dir, &self.cwd);
        let reminder = prompt::turn_reminder(
            self.mode,
            self.plan_enabled,
            &self.id,
            &mut self.mode_reminded,
            &self.date_frozen,
            &mut self.date_reminded,
            &self.agents_prompt,
            &fresh_agents,
            &mut self.agents_reminded,
        );
        // @引用文件按指针形态注入（kimi-code 同款取舍）：只给路径（+可选行范围），
        // 内容由模型按需用 Read 现读——永远新鲜、恒定一行、不伤前缀缓存
        let mut user_text = pointer_file_references(&self.cwd, &content, &files);
        if let Some(reminder) = reminder {
            user_text = format!("{reminder}\n\n{user_text}");
        }
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
        // rollout 只记原文（files 另存字段；「引用文件」后缀已废除——
        // UI 用 files 渲染内联 chip，模型侧 resume 经 rebuild_history 转指针行）
        let rollout_text = content.clone();
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
            if used > threshold && !self.run_compact(Some(config), true, None, tx, cancel).await {
                return StepOutcome::Ended;
            }
        }

        let text_item = format!("{turn_id}-text-{step}");
        let reasoning_item = format!("{turn_id}-reason-{step}");
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let api_started = Instant::now();
        // 调用轨迹输入投影：请求前拍快照（图片只记张数，长内容截断）。
        // 落盘只存增量：与上一条的完整投影取公共前缀，offset + delta（对齐
        // ZCode model-io，避免同一会话完整上下文梯度逐条重复）
        let io_input_full = crate::model_io::project_input(&self.history);
        let io_offset = crate::model_io::common_prefix_len(&io_input_full, &self.io_last_input);
        let io_input: Vec<_> = io_input_full[io_offset..].to_vec();
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
        // 本步用量与失败原因（轨迹落盘用）
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
                }) => {
                    self.turn_input += input;
                    self.turn_cache_read += cache_read;
                    self.turn_output += output;
                    self.input_total += input;
                    self.cache_read_total += cache_read;
                    self.last_total_tokens = Some(used);
                    step_usage = crate::model_io::ModelIoUsage {
                        input,
                        cache_read,
                        output,
                        used,
                        total,
                    };
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
                            message: error.clone(),
                        },
                        tx,
                    );
                    step_error = Some(error);
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
        let step_ttft_ms = first_token_at
            .map(|at| at.duration_since(api_started).as_millis() as u64)
            .unwrap_or(0)
            .min(api_elapsed.as_millis() as u64);
        self.turn_ttft_ms += step_ttft_ms;

        // 调用轨迹落盘（失败/取消也记）：UI「查看调用轨迹」直读该文件；
        // 写失败非致命（与 rollout.append 同口径，eprintln 走 core 惯例）
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
            eprintln!("写入调用轨迹失败（忽略）: {e}");
        }
        // 本条完整投影成为下一条的增量基准
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
            let summary = tool::summarize(call);
            // ExitPlanMode 的 begin 在拦截块内发（detail 用生效 plan 重写——
            // 参数缺省时 core 读计划文件，UI 计划卡与回放恢复同数据源）
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

            // ExitPlanMode：在计划硬拒之前拦截（它是退出计划模式的唯一出口，
            // 强制弹窗请用户确认；复用 ApprovalRequested 通道，UI 无需新组件）。
            // kimi 语义：plan 参数可选——缺省时 core 读计划文件
            if call.name == "ExitPlanMode" {
                let args: serde_json::Value =
                    serde_json::from_str(&call.arguments).unwrap_or_default();
                let mut plan = args["plan"].as_str().unwrap_or("").to_string();
                if plan.trim().is_empty() {
                    plan = read_plan_file(&self.cwd, &self.id).unwrap_or_default();
                }
                // begin 用生效 plan 重写（回放经 rollout arguments 恢复同一全文）
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
                    note = "仅在计划模式下可用".to_string();
                    is_error = true;
                } else if plan.trim().is_empty() {
                    // kimi exitPlanModeTool 同款：计划文件为空/缺失时不弹审批，
                    // 引导模型先写计划文件
                    note = format!(
                        "计划文件为空或不存在：请先用 Write 把计划写入 `.pigcode/plans/plan-{}.md`，再调用 ExitPlanMode。",
                        self.id
                    );
                    is_error = true;
                } else {
                    let request_id = format!("{}-{turn_id}-exitplan-{item_id}", self.id);
                    let (reply_tx, reply_rx) = oneshot::channel();
                    // 计划确认是一次性弹窗，不参与同键合并决议
                    self.pending
                        .lock()
                        .expect("pending lock")
                        .insert(request_id.clone(), (reply_tx, None));
                    // kimi 语义：计划先落盘再弹审批（批准时重写同内容幂等；
                    // 拒绝后文件保留，修订后下一次 ExitPlanMode 覆盖）
                    write_plan_file(&self.cwd, &self.id, &plan);
                    // 完整计划进弹窗（kimi 计划审批面板自带标题，detail = 纯计划
                    // 全文——截断会让用户批准前看不到全文）
                    self.emit(
                        |session_id, seq| Event::ApprovalRequested {
                            session_id,
                            seq,
                            request_id: request_id.clone(),
                            tool: call.name.clone(),
                            detail: plan.to_string(),
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
                            // 批准 = 关计划开关 + 计划落盘（弹窗前已写，这里幂等覆盖）
                            // 退出只翻转计划开关——执行模式是独立维度，原样保留
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
                            note = "计划已批准，计划模式已关闭，请按计划开始执行。".to_string();
                            is_error = false;
                        }
                        ApprovalDecision::Reject => {
                            // kimi Revise：拒绝可携带反馈意见，模型据此修订重提
                            note = match feedback.filter(|f| !f.trim().is_empty()) {
                                Some(f) => format!(
                                    "用户拒绝退出计划模式。反馈意见：{}\n请据此修订计划并重新提交。",
                                    f.trim()
                                ),
                                None => {
                                    "用户拒绝退出计划模式，请继续完善计划或回答疑问。".to_string()
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
                    // 落生效 plan（参数缺省时为文件内容）：回放经它恢复计划卡全文
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

            // EnterPlanMode：进计划是自我收紧（只读化），直接切换不弹窗。
            // 计划开关与执行模式正交——只翻转 plan_enabled，模式档不动。
            if call.name == "EnterPlanMode" {
                let (note, is_error) = if self.plan_enabled {
                    ("已在计划模式，请继续调研并输出计划。".to_string(), false)
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
                            "已开启计划模式。接下来用只读工具调研，计划写好后用 Write 写入计划文件 `.pigcode/plans/plan-{}.md`（唯一可写路径），再调用 ExitPlanMode 请用户确认执行。",
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

            // 计划硬拒（白名单口径，比 kimi-code 黑名单更严）：只读工具 +
            // 计划文件写（kimi writesOnlyPlanFile）放行，其余修改类直接拒——
            // 与权限档无关，「完全访问 + 计划」也照拒
            if self.plan_enabled
                && !read_only
                && !tool::is_plan_file_write(&self.cwd, &call.arguments)
            {
                let note = format!(
                    "计划模式：修改类工具已被禁止执行（唯一例外是写计划文件 `.pigcode/plans/plan-{}.md`）。请用只读工具调研，把计划写入计划文件后经 ExitPlanMode 请用户确认。",
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

/// @引用文件的指针行：只注入路径与大小，不读内容、不带读取指引
///（kimi-code TUI 的极简形态：@ 是注意力引导；「需要时主动调工具」在系统
/// 提示词里是常驻指令，工具清单也在，逐条重复提示是噪音）。
/// 路径仍经 resolve_checked 校验（工作区内、非敏感）；图片只标注类型。
pub(crate) fn pointer_file_references(cwd: &Path, content: &str, files: &[String]) -> String {
    let mut text = content.to_string();
    for file in files {
        let block = match tool::resolve_checked(cwd, file, false) {
            Ok(full) => {
                let size = std::fs::metadata(&full).ok().map(|m| m.len());
                let kind = if is_image_path(file) { "，图片" } else { "" };
                match size {
                    Some(bytes) => {
                        format!("\n\n[引用文件 {file}（{}{kind}）]", human_size(bytes))
                    }
                    None => format!("\n\n[引用文件 {file}{kind}]"),
                }
            }
            Err(error) => format!("\n\n[无法引用文件 {file}: {error}]"),
        };
        text.push_str(&block);
    }
    text
}

/// 读计划文件（ExitPlanMode 的 plan 参数缺省时）：不存在/读失败为 None
fn read_plan_file(cwd: &Path, session_id: &str) -> Option<String> {
    std::fs::read_to_string(
        cwd.join(".pigcode")
            .join("plans")
            .join(format!("plan-{session_id}.md")),
    )
    .ok()
}

/// 计划落盘（ZCode plan-file-continuity 同款）：ExitPlanMode 批准时把计划
/// 全文原子写入 `<cwd>/.pigcode/plans/plan-<session_id>.md`（tmp+rename）。
/// 失败只打日志不阻断执行——计划已在对话历史与 rollout 里
fn write_plan_file(cwd: &Path, session_id: &str, plan: &str) {
    let dir = cwd.join(".pigcode").join("plans");
    let path = dir.join(format!("plan-{session_id}.md"));
    let tmp = dir.join(format!("plan-{session_id}.md.tmp"));
    let result = std::fs::create_dir_all(&dir)
        .and_then(|()| std::fs::write(&tmp, plan))
        .and_then(|()| std::fs::rename(&tmp, &path));
    if let Err(error) = result {
        eprintln!("[plan] 计划落盘失败 {}: {error}", path.display());
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

    fn pointer_test_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pig-pointer-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 指针行：只给路径+大小（图片标注类型），不带读取指引、不读文件内容
    #[test]
    fn pointer_references_never_inline_content() {
        let dir = pointer_test_dir("basic");
        std::fs::write(dir.join("a.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join("big.rs"), "x".repeat(2048)).unwrap();
        std::fs::write(dir.join("logo.png"), b"\x89PNG").unwrap();

        // 默认：路径 + 大小，无指引、无内容
        let out = pointer_file_references(&dir, "看下这个", &["a.rs".into()]);
        assert!(out.contains("[引用文件 a.rs（13 B）]"), "{out}");
        assert!(!out.contains("Read"), "不带读取指引: {out}");
        assert!(!out.contains("fn main"), "不得内联内容: {out}");

        // 大文件：同样只给指针（读取由模型用 Read 分页）
        let out = pointer_file_references(&dir, "看 10000 到 10050 行", &["big.rs".into()]);
        assert!(out.contains("[引用文件 big.rs（2.0 KB）]"), "{out}");
        assert!(!out.contains("xxx"), "不得内联内容: {out}");

        // 图片：只标注类型
        let out = pointer_file_references(&dir, "看图", &["logo.png".into()]);
        assert!(out.contains("（4 B，图片）]"), "{out}");

        // 工作区外/不存在：报错行
        let out = pointer_file_references(&dir, "x", &["../etc/passwd".into()]);
        assert!(out.contains("[无法引用文件 ../etc/passwd"), "{out}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
