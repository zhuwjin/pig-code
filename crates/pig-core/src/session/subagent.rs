use super::*;

impl Session {
    /// Agent 工具入口：参数门禁 → 档案/模型/工具解析 → 前台（借 Session 字段组 GateCtx）
    /// 或后台（全 owned + tokio::spawn，完成经 wake 通道唤醒父会话）驱动子代理。
    /// resume 复用原 agent_id：读入上下文 + 追加新 prompt 续跑（档案/模型按现状重解析）。
    pub(crate) async fn run_subagent(
        &mut self,
        call: &ToolCall,
        // 子工具的审批 request_id 以前台卡 item_id / 后台 task_id 为前缀（含 turn 信息），
        // 本参数自 GateCtx 下沉后不再单独使用
        _parent_turn_id: &str,
        parent_item_id: &str,
        parent_config: &ResolvedModel,
        tx: &async_channel::Sender<Event>,
        cancel: &CancellationToken,
    ) -> SubagentOutcome {
        // ---- 参数解析与互斥门禁 ----
        let args: serde_json::Value = serde_json::from_str(&call.arguments).unwrap_or_default();
        let fail = |note: String| SubagentOutcome::Finished {
            note,
            is_error: true,
            // 参数/档案/模型解析失败的早退没有 agent_id，不建代理卡
            card: None,
            cards: vec![],
        };
        let description = args["description"].as_str().unwrap_or("").trim();
        if description.is_empty() {
            return fail("Agent 缺少参数 description（3-5 词任务简述）".to_string());
        }
        let prompt_text = args["prompt"].as_str().unwrap_or("").trim();
        if prompt_text.is_empty() {
            return fail("Agent 缺少参数 prompt（完整自包含的任务简报）".to_string());
        }
        let background = args["run_in_background"].as_bool().unwrap_or(false);
        let resume = args["resume"].as_str().unwrap_or("").trim();
        let subagent_type = args["subagent_type"].as_str().unwrap_or("").trim();
        if !resume.is_empty() && !subagent_type.is_empty() {
            return fail("resume 与 subagent_type 互斥：续跑已有子代理时不能指定类型".to_string());
        }
        // 计划模式硬拒：子代理可能修改文件，Plan 只读语义不能被绕过
        if self.mode == ExecMode::Plan {
            return fail(
                "计划模式下不可委派子代理（子代理可能修改文件）。请先用只读工具自行调研并输出计划，或退出计划模式后再委派。"
                    .to_string(),
            );
        }

        // ---- 上下文准备：新起建 history；resume 读入历史 + 追加新 prompt ----
        let profiles = crate::agent::load_profiles(&self.cwd, &self.data_dir);
        let agents_dir = crate::agent::agents_dir(&self.data_dir.join("sessions"), &self.id);
        let (profile, agent_id, history) = if resume.is_empty() {
            let query = if subagent_type.is_empty() {
                "general-purpose"
            } else {
                subagent_type
            };
            let profile = match crate::agent::find_profile(&profiles, query) {
                Ok(profile) => profile.clone(),
                Err(error) => return fail(error),
            };
            let agent_id = format!("a{}-{}", crate::rollout::now_secs(), self.agent_seq + 1);
            self.agent_seq += 1;
            let history = vec![
                ChatMsg::system(crate::prompt::subagent_system_prompt(
                    &profile,
                    &self.cwd,
                    self.git_snapshot.as_deref(),
                    &self.agents_prompt,
                    &self.skills_prompt,
                )),
                ChatMsg::user(prompt_text.to_string()),
            ];
            (profile, agent_id, history)
        } else {
            let jsonl = agents_dir.join(format!("{resume}.jsonl"));
            if !jsonl.exists() {
                // 列可用 agent_id（*.jsonl 去后缀），帮助模型纠正拼写
                let mut ids: Vec<String> = std::fs::read_dir(&agents_dir)
                    .ok()
                    .into_iter()
                    .flatten()
                    .filter_map(|entry| entry.ok())
                    .filter_map(|entry| {
                        let path = entry.path();
                        if path.extension().is_some_and(|ext| ext == "jsonl") {
                            path.file_stem().map(|s| s.to_string_lossy().to_string())
                        } else {
                            None
                        }
                    })
                    .collect();
                ids.sort();
                let available = if ids.is_empty() {
                    "（无）".to_string()
                } else {
                    ids.join(", ")
                };
                return fail(format!("子代理 \"{resume}\" 不存在。可用: {available}"));
            }
            // 运行中冲突：注册表里同 agent_id 且 Running → 不允许并行续跑
            let running_task = self
                .state
                .tasks
                .lock()
                .expect("task registry lock")
                .iter()
                .find(|t| {
                    t.agent_id.as_deref() == Some(resume)
                        && matches!(t.status, pig_protocol::TaskStatus::Running)
                })
                .map(|t| t.id.clone());
            if let Some(task_id) = running_task {
                return fail(format!(
                    "该子代理仍在运行（task_id {task_id}），可用 TaskStop 停止后再续跑"
                ));
            }
            let (meta, mut history) = match crate::agent::read_agent(&jsonl) {
                Ok(loaded) => loaded,
                Err(error) => return fail(error),
            };
            // 档案按现状重找（被删 → 报错）；模型同样以档案现状重解析（忽略 meta.model）
            let profile = match crate::agent::find_profile(&profiles, &meta.profile) {
                Ok(profile) => profile.clone(),
                Err(error) => return fail(error),
            };
            history.push(ChatMsg::user(prompt_text.to_string()));
            (profile, resume.to_string(), history)
        };

        // ---- 模型解析（严格：失败即报错给模型）----
        let child_config = match self.app_config.as_ref() {
            Some(app_config) => {
                match crate::agent::resolve_subagent_model(app_config, parent_config, &profile) {
                    Ok(config) => config,
                    Err(error) => return fail(error),
                }
            }
            // 未加载应用配置：继承父模型不受影响，显式模型无法解析
            None if profile.model.is_some() => {
                return fail("未加载应用配置，无法解析子代理指定模型".to_string());
            }
            None => parent_config.clone(),
        };

        // ---- 工具收窄：子代理循环用 all()（天然无 Agent 防嵌套）+ MCP 继承 ----
        let all_tools = tool::all();
        let all_names: Vec<String> = all_tools.iter().map(|t| t.name().to_string()).collect();
        let keep = crate::agent::child_tool_set(&profile, &all_names, child_config.input_image);
        let mut child_tools: Vec<Box<dyn tool::Tool>> = all_tools
            .into_iter()
            .filter(|t| keep.iter().any(|name| name == t.name()))
            .collect();
        // MCP 继承：全工具档案（收窄后含 Write/Edit）继承全部已连接 MCP 工具；
        // 只读档案（如 explore）只继承 readOnlyHint 的。schemas 同步进子代理采样
        let mcp_inherits_all = crate::agent::child_inherits_all_mcp(&keep);
        if let Some(mcp) = &self.mcp {
            child_tools.extend(mcp.child_tools(mcp_inherits_all));
        }
        // Skill 补给所有子代理（只读，MCP 继承同款越档案默认）：技能正文按需加载，
        // 子代理系统提示同样注入技能清单
        child_tools.push(Box::new(tool::SkillTool::new(&self.cwd, &self.data_dir)));
        let child_schemas: Vec<serde_json::Value> =
            child_tools.iter().map(|t| t.schema()).collect();

        // ---- 上下文持久化：新起写 meta+初始消息；resume 只追加新 user 行 ----
        let jsonl = agents_dir.join(format!("{agent_id}.jsonl"));
        if resume.is_empty() {
            persist_agent_line(
                &jsonl,
                &serde_json::json!({
                    "type": "meta",
                    "agent_id": agent_id,
                    "profile": profile.name,
                    "description": description,
                    "model": child_config.model,
                    "provider": child_config.provider_name,
                    "created_at": crate::rollout::now_secs(),
                }),
            );
            for msg in &history {
                persist_agent_msg(&jsonl, msg);
            }
        } else {
            persist_agent_msg(&jsonl, history.last().expect("resume user pushed"));
        }

        let max_turns = profile.max_turns.unwrap_or(crate::agent::DEFAULT_MAX_TURNS);
        let mut drive = SubagentDrive {
            agent_id,
            cwd: self.cwd.clone(),
            data_dir: self.data_dir.clone(),
            profile,
            child_config,
            tools: child_tools,
            schemas: child_schemas,
            history,
            jsonl,
            max_turns,
            description: description.to_string(),
            mcp: self.mcp.clone(),
            mcp_inherits_all,
        };

        // 代理卡元信息：agent_id 分配 + child_config 解析完成后即组好——
        // live 经 SubagentCard 事件直发，并随 RolloutRecord::ToolCall 持久化
        //（回放经记录重建）；前台/后台/resume 同路（profile/model 按本次重解析结果）
        let card_record = crate::rollout::AgentCardRecord {
            agent_id: drive.agent_id.clone(),
            profile: drive.profile.name.clone(),
            description: drive.description.clone(),
            model: agent_card_model(&drive),
            background,
        };
        self.emit(
            |session_id, seq| Event::SubagentCard {
                session_id,
                seq,
                item_id: parent_item_id.to_string(),
                agent_id: card_record.agent_id.clone(),
                profile: card_record.profile.clone(),
                description: card_record.description.clone(),
                model: card_record.model.clone(),
                background: card_record.background,
            },
            tx,
        );

        // ---- 后台：注册任务 + spawn 驱动，立即返回 running ----
        if background {
            return self.spawn_subagent_background(drive, tx, card_record);
        }

        // ---- 前台：借 Session 字段组 GateCtx 同步驱动 ----
        let result = {
            // 子代理继承的 MCP 工具（门控执行段的按名查找兜底；规则按档案收窄）
            let mcp_extra = drive.extra_tools();
            let mut gate = GateCtx {
                cwd: &self.cwd,
                mode: self.mode,
                tracker: &mut self.tracker,
                state: &self.state,
                pending: &self.pending,
                permissions: &self.permissions,
                always_allowed: &mut self.always_allowed,
                session_id: &self.id,
                seq: &self.seq,
                store: &self.store,
                extra_tools: &mcp_extra,
            };
            drive_subagent(
                &mut gate,
                &mut drive,
                &ProgressSink::Foreground {
                    parent_item_id: parent_item_id.to_string(),
                },
                tx,
                cancel,
            )
            .await
        };
        // 实时面板收尾（含父取消）：右侧「子代理」tab 关「运行中」指示
        self.emit(
            |session_id, seq| Event::SubagentActivity {
                session_id,
                seq,
                agent_id: drive.agent_id.clone(),
                item: None,
                finished: true,
            },
            tx,
        );
        if result.cancelled {
            return SubagentOutcome::Cancelled {
                card: Some(card_record),
                cards: vec![],
            };
        }
        // 子代理成本计入父回合统计（不记 StepUsage/不更新水位）
        self.turn_input += result.usage.0;
        self.turn_cache_read += result.usage.1;
        self.turn_output += result.usage.2;
        // 收尾模板：正常收尾用模板；轮次用尽/请求失败直接用驱动带出的文本
        let note = if result.completed {
            format!(
                "agent_id: {}\nsubagent_type: {}\nstatus: completed\nturns: {}\n[summary]\n{}\nresume_hint: 用 Agent(resume=\"{}\", prompt=\"...\") 继续该子代理",
                drive.agent_id,
                drive.profile.name,
                result.turns,
                result.result_text,
                drive.agent_id
            )
        } else {
            result.result_text
        };
        SubagentOutcome::Finished {
            note,
            is_error: result.is_error,
            card: Some(card_record),
            cards: vec![],
        }
    }
    /// AgentSwarm 工具入口：parse_swarm_args 校验 → prepare_swarm_children 批量准备
    ///（与 run_subagent 同管线）→ 全部并发驱动（全局并发槽上限，超限排队）。
    /// 默认前台：阻塞至全部完成，聚合结果作为单个工具结果；turn 取消 → 取消全部子代理。
    /// run_in_background=true：逐个注册后台任务 + spawn 独立驱动（与后台 Agent 同
    /// 生命周期：TaskStop 可停、完成经 wake 通道逐个唤醒父会话），立即返回逐项回执。
    pub(crate) async fn run_swarm(
        &mut self,
        call: &ToolCall,
        parent_item_id: &str,
        parent_config: &ResolvedModel,
        tx: &async_channel::Sender<Event>,
        cancel: &CancellationToken,
    ) -> SubagentOutcome {
        let args: serde_json::Value = serde_json::from_str(&call.arguments).unwrap_or_default();
        let fail = |note: String| SubagentOutcome::Finished {
            note,
            is_error: true,
            // 参数/档案/模型解析失败的早退没有 agent_id，不建代理卡
            card: None,
            cards: vec![],
        };
        let plan = match tool::parse_swarm_args(&args) {
            Ok(plan) => plan,
            Err(error) => return fail(error),
        };
        // 计划模式硬拒（与 run_subagent 同口径）：子代理可能修改文件——前台/后台同拒
        if self.mode == ExecMode::Plan {
            return fail(
                "计划模式下不可委派子代理（子代理可能修改文件）。请先用只读工具自行调研并输出计划，或退出计划模式后再委派。"
                    .to_string(),
            );
        }
        let background = args["run_in_background"].as_bool().unwrap_or(false);
        let preps = match crate::agent::prepare_swarm_children(
            &crate::agent::SwarmPrepCtx {
                cwd: &self.cwd,
                data_dir: &self.data_dir,
                git_snapshot: self.git_snapshot.as_deref(),
                skills_prompt: &self.skills_prompt,
                agents_prompt: &self.agents_prompt,
                app_config: self.app_config.as_ref(),
                parent_config,
                session_id: &self.id,
                tasks: &self.state.tasks,
                mcp: self.mcp.as_ref(),
            },
            &plan,
            &mut self.agent_seq,
        ) {
            Ok(preps) => preps,
            Err(error) => return fail(error),
        };

        // ---- 后台：逐个派发独立后台驱动（不 join），立即返回逐项回执 ----
        if background {
            return self.dispatch_swarm_background(preps, parent_item_id, tx);
        }

        // ---- 前台：全部并发驱动，阻塞至全部完成 ----
        // 每个子代理的卡元信息逐张收集（live 经 SubagentCard 事件直发；随 rollout
        // ToolCall 记录的 agent_cards 持久化，回放重建全部子代理卡）
        let mut cards: Vec<crate::rollout::AgentCardRecord> = Vec::new();
        // slots 保持 plan 顺序（准备失败的条目原地落 Failed，spawn 结果按 ix 回填）
        let mut slots: Vec<Option<crate::agent::SwarmChildResult>> =
            (0..preps.len()).map(|_| None).collect();
        let mut handles = Vec::new();
        let mut tokens = Vec::new();
        for (ix, prep) in preps.into_iter().enumerate() {
            let prep = match prep {
                crate::agent::SwarmPrep::Ready(prep) => prep,
                // 准备阶段失败：不启动，聚合里记为失败项（部分失败不影响其他）
                crate::agent::SwarmPrep::Failed { description, error } => {
                    slots[ix] = Some(crate::agent::SwarmChildResult {
                        description,
                        agent_id: None,
                        status: crate::agent::SwarmChildStatus::Failed,
                        turns: 0,
                        result_path: None,
                        result_text: error,
                        queued: false,
                        usage: (0, 0, 0),
                    });
                    continue;
                }
            };
            let drive = SubagentDrive::from(*prep);
            // 代理卡（与 run_subagent 同口径；批量前台 background=false）
            let card = crate::rollout::AgentCardRecord {
                agent_id: drive.agent_id.clone(),
                profile: drive.profile.name.clone(),
                description: drive.description.clone(),
                model: agent_card_model(&drive),
                background: false,
            };
            self.emit(
                |session_id, seq| Event::SubagentCard {
                    session_id,
                    seq,
                    item_id: parent_item_id.to_string(),
                    agent_id: card.agent_id.clone(),
                    profile: card.profile.clone(),
                    description: card.description.clone(),
                    model: card.model.clone(),
                    background: card.background,
                },
                tx,
            );
            cards.push(card);
            // 排队中也注册（command 带「排队中 · 」前缀），TaskList/面板可见可停
            let child_cancel = CancellationToken::new();
            let command = format!("子代理 {}: {}", drive.profile.name, drive.description);
            let task_id = crate::task::register_agent_task_queued(
                &self.state,
                command,
                child_cancel.clone(),
                drive.agent_id.clone(),
            );
            // 全 owned 上下文（与 spawn_subagent_background 同清单）
            let cwd = self.cwd.clone();
            let mode = self.mode;
            let permissions = self.permissions.clone();
            let mut always_allowed = self.always_allowed.clone();
            let state = self.state.clone();
            let pending = self.pending.clone();
            let store = self.store.clone();
            let seq = self.seq.clone();
            let session_id = self.id.clone();
            let tx_bg = tx.clone();
            let result_path = agent_result_path(&drive.jsonl, &drive.agent_id);
            let description = drive.description.clone();
            let agent_id = drive.agent_id.clone();
            let task_id_bg = task_id.clone();
            tokens.push(child_cancel.clone());
            handles.push(tokio::spawn(async move {
                // 全局并发槽：超限排队（排队中也可被 TaskStop/父取消打断）
                let queued = crate::task::subagent_slots_available() == 0;
                if queued {
                    crate::task::note_output(
                        &state.tasks,
                        &task_id_bg,
                        "排队中：子代理全局并发槽已满，等待空槽…\n",
                    );
                }
                let Some(_permit) = crate::task::acquire_subagent_slot(&child_cancel).await else {
                    // 排队中被取消：stop_task 已置 Killed，注册表无需收尾
                    return (
                        ix,
                        crate::agent::SwarmChildResult {
                            description,
                            agent_id: Some(agent_id),
                            status: crate::agent::SwarmChildStatus::Cancelled,
                            turns: 0,
                            result_path: None,
                            result_text: String::new(),
                            queued,
                            usage: (0, 0, 0),
                        },
                    );
                };
                crate::task::mark_agent_task_started(&state, &task_id_bg);
                let mut tracker = ChangeTracker::default();
                let result = {
                    // 子代理继承的 MCP 工具（owned：闭包内现取，规则快照在 drive 上）
                    let mcp_extra = drive.extra_tools();
                    let mut gate = GateCtx {
                        cwd: &cwd,
                        mode,
                        tracker: &mut tracker,
                        state: &state,
                        pending: &pending,
                        permissions: &permissions,
                        always_allowed: &mut always_allowed,
                        session_id: &session_id,
                        seq: &seq,
                        store: &store,
                        extra_tools: &mcp_extra,
                    };
                    let mut drive = drive;
                    drive_subagent(
                        &mut gate,
                        &mut drive,
                        &ProgressSink::Background {
                            task_id: task_id_bg.clone(),
                        },
                        &tx_bg,
                        &child_cancel,
                    )
                    .await
                };
                // 实时面板收尾（含父取消/TaskStop）
                emit_bg(&session_id, &seq, &tx_bg, |sid, seq| {
                    Event::SubagentActivity {
                        session_id: sid,
                        seq,
                        agent_id: agent_id.clone(),
                        item: None,
                        finished: true,
                    }
                });
                // 注册表收尾：TaskStop 已置 Killed 的不覆写；父取消记 Killed
                {
                    let mut tasks = state.tasks.lock().expect("task registry lock");
                    if let Some(entry) = tasks.iter_mut().find(|t| t.id == task_id_bg)
                        && matches!(entry.status, pig_protocol::TaskStatus::Running)
                    {
                        entry.status = if result.cancelled {
                            pig_protocol::TaskStatus::Killed
                        } else if result.is_error {
                            pig_protocol::TaskStatus::Exited(-1)
                        } else {
                            pig_protocol::TaskStatus::Exited(0)
                        };
                        entry.ended_at = Some(crate::rollout::now_secs());
                    }
                }
                crate::task::note_output(
                    &state.tasks,
                    &task_id_bg,
                    &format!("[{}]\n{}\n", result.status_line, result.result_text),
                );
                let _ = state.task_notify.send(session_id.clone());
                (
                    ix,
                    crate::agent::SwarmChildResult {
                        description,
                        agent_id: Some(agent_id),
                        status: if result.cancelled {
                            crate::agent::SwarmChildStatus::Cancelled
                        } else if result.is_error {
                            crate::agent::SwarmChildStatus::Failed
                        } else {
                            crate::agent::SwarmChildStatus::Completed
                        },
                        turns: result.turns,
                        result_path: if result.cancelled {
                            None
                        } else {
                            std::fs::metadata(&result_path).ok().map(|_| result_path)
                        },
                        result_text: result.result_text,
                        queued,
                        usage: result.usage,
                    },
                )
            }));
        }

        // 全部并发；父取消 → 取消全部子代理并等收尾（Cancelled 语义与前台 Agent
        // 一致：部分结果不进聚合，工具卡落定「已停止」）
        let joined = tokio::select! {
            joined = futures_util::future::join_all(&mut handles) => Some(joined),
            _ = cancel.cancelled() => {
                for token in &tokens {
                    token.cancel();
                }
                let _ = futures_util::future::join_all(&mut handles).await;
                None
            }
        };
        let Some(joined) = joined else {
            return SubagentOutcome::Cancelled { card: None, cards };
        };
        for item in joined {
            match item {
                Ok((ix, child)) => slots[ix] = Some(child),
                Err(error) => eprintln!("[agent] swarm 子代理任务异常终止: {error}"),
            }
        }
        let children: Vec<crate::agent::SwarmChildResult> = slots.into_iter().flatten().collect();
        // 子代理成本计入父回合统计（前台口径；不记 StepUsage/不更新水位）
        for child in &children {
            self.turn_input += child.usage.0;
            self.turn_cache_read += child.usage.1;
            self.turn_output += child.usage.2;
        }
        let is_error = children
            .iter()
            .any(|c| c.status != crate::agent::SwarmChildStatus::Completed);
        SubagentOutcome::Finished {
            note: crate::agent::format_swarm_result(&children),
            is_error,
            // 批量代理卡：每个子代理一张（live 已逐张经 SubagentCard 事件发出），
            // 随 rollout ToolCall 记录的 agent_cards 持久化，回放重建全部子代理卡
            card: None,
            cards,
        }
    }

    /// 后台 swarm 分支：每个 Ready 子代理发 SubagentCard（background=true）+ 注册
    /// 「排队中」任务 + spawn 独立后台驱动（drive_subagent_detached，与后台 Agent
    /// 共用：并发槽排队/注册表收尾/结果落盘/完成经 wake 通道逐个唤醒父会话）；
    /// 不 join，立即返回逐项回执（准备阶段失败项同样列出）。usage 不进父回合
    /// 统计（与后台 Agent 同口径：驱动体内丢弃）；父 turn 继续运行/结束都不
    /// 波及子代理，父 turn 取消也不取消它们（只认各自注册表里的 TaskStop）。
    fn dispatch_swarm_background(
        &mut self,
        preps: Vec<crate::agent::SwarmPrep>,
        parent_item_id: &str,
        tx: &async_channel::Sender<Event>,
    ) -> SubagentOutcome {
        let mut cards: Vec<crate::rollout::AgentCardRecord> = Vec::new();
        let mut receipt: Vec<crate::agent::SwarmReceiptChild> = Vec::new();
        // 回执的 running/queued 按组装瞬间的空闲槽估算（瞬时值；真实排队态以
        // TaskList/面板的「排队中 · 」前缀为准，槽到手 mark_agent_task_started 摘前缀）
        let slots_available = crate::task::subagent_slots_available();
        for prep in preps {
            let prep = match prep {
                crate::agent::SwarmPrep::Ready(prep) => prep,
                // 准备阶段失败：不启动，回执里记为失败项（部分失败不影响其他）
                crate::agent::SwarmPrep::Failed { description, error } => {
                    receipt.push(crate::agent::SwarmReceiptChild {
                        description,
                        agent_id: None,
                        task_id: None,
                        queued: false,
                        error: Some(error),
                    });
                    continue;
                }
            };
            let drive = SubagentDrive::from(*prep);
            // 代理卡（与 run_subagent 同口径；后台批量 background=true）
            let card = crate::rollout::AgentCardRecord {
                agent_id: drive.agent_id.clone(),
                profile: drive.profile.name.clone(),
                description: drive.description.clone(),
                model: agent_card_model(&drive),
                background: true,
            };
            self.emit(
                |session_id, seq| Event::SubagentCard {
                    session_id,
                    seq,
                    item_id: parent_item_id.to_string(),
                    agent_id: card.agent_id.clone(),
                    profile: card.profile.clone(),
                    description: card.description.clone(),
                    model: card.model.clone(),
                    background: card.background,
                },
                tx,
            );
            let description = drive.description.clone();
            let agent_id = drive.agent_id.clone();
            // 排队中也注册（command 带「排队中 · 」前缀），TaskList/面板可见可停
            let child_cancel = CancellationToken::new();
            let command = format!("子代理 {}: {}", drive.profile.name, drive.description);
            let task_id = crate::task::register_agent_task_queued(
                &self.state,
                command,
                child_cancel.clone(),
                agent_id.clone(),
            );
            let queued = cards.len() >= slots_available;
            tokio::spawn(drive_subagent_detached(
                self.detached_gate_ctx(),
                drive,
                task_id.clone(),
                child_cancel,
                tx.clone(),
                std::time::Instant::now(),
            ));
            cards.push(card);
            receipt.push(crate::agent::SwarmReceiptChild {
                description,
                agent_id: Some(agent_id),
                task_id: Some(task_id),
                queued,
                error: None,
            });
        }
        SubagentOutcome::Finished {
            note: crate::agent::format_swarm_receipt(&receipt),
            // 无一派发成功（全部准备阶段失败）才算调用级失败；部分失败在回执里列出
            is_error: cards.is_empty(),
            card: None,
            cards,
        }
    }

    /// 后台子代理：注册任务条目后 tokio::spawn 驱动（全 owned 上下文），完成时更新
    /// 注册表 + notify + 经 wake 通道唤醒父会话（TaskStop 杀的不唤醒）。立即返回 running。
    fn spawn_subagent_background(
        &self,
        drive: SubagentDrive,
        tx: &async_channel::Sender<Event>,
        card: crate::rollout::AgentCardRecord,
    ) -> SubagentOutcome {
        let bg_cancel = CancellationToken::new();
        let command = format!("子代理 {}: {}", drive.profile.name, drive.description);
        // 排队中也注册（command 带「排队中 · 」前缀），TaskList/面板可见可停；
        // 并发槽到手后 mark_agent_task_started 摘前缀
        let task_id = crate::task::register_agent_task_queued(
            &self.state,
            command,
            bg_cancel.clone(),
            drive.agent_id.clone(),
        );
        let agent_id = drive.agent_id.clone();
        let started_at = std::time::Instant::now();
        // 给父模型的即时回执（不依赖任务结果，先组好）；并发槽已满时说明排队
        let queue_hint = if crate::task::subagent_slots_available() == 0 {
            "\n当前子代理全局并发槽已满（上限 8），本任务排队等待空槽，完成后照常通知。"
        } else {
            ""
        };
        let running_note = format!(
            "agent_id: {agent_id}\ntask_id: {task_id}\nstatus: running\n子代理已在后台运行，完成后结果会以 <task-notification> 通知送达——不要轮询。\n可用 TaskOutput 看进度、TaskStop 停止、Agent(resume=\"{agent_id}\", prompt=\"...\") 续跑。{queue_hint}"
        );
        tokio::spawn(drive_subagent_detached(
            self.detached_gate_ctx(),
            drive,
            task_id,
            bg_cancel,
            tx.clone(),
            started_at,
        ));
        SubagentOutcome::Finished {
            note: running_note,
            is_error: false,
            card: Some(card),
            cards: vec![],
        }
    }

    /// 后台子代理驱动的 owned 门控上下文（全 owned：共享 Arc 克隆 + 值快照），
    /// spawn_subagent_background 与后台 swarm 派发共用同一份清单
    fn detached_gate_ctx(&self) -> DetachedGateCtx {
        DetachedGateCtx {
            cwd: self.cwd.clone(),
            mode: self.mode,
            permissions: self.permissions.clone(),
            always_allowed: self.always_allowed.clone(),
            state: self.state.clone(),
            pending: self.pending.clone(),
            store: self.store.clone(),
            seq: self.seq.clone(),
            session_id: self.id.clone(),
        }
    }
}

/// 后台子代理驱动的 owned 门控上下文（Session::detached_gate_ctx 组装）：
/// 共享 Arc 克隆 + 值快照；独立 ChangeTracker 在驱动体内建——后台子代理的
/// 改动不进父「本轮改动」面板（git 口径的 review 面板仍可见）
struct DetachedGateCtx {
    cwd: PathBuf,
    mode: ExecMode,
    permissions: crate::permissions::PermissionRules,
    always_allowed: HashSet<(String, String)>,
    state: crate::task::SessionToolState,
    pending: PendingApprovals,
    store: Arc<Mutex<Store>>,
    seq: Arc<std::sync::atomic::AtomicU64>,
    session_id: String,
}

/// 后台子代理驱动体（spawn_subagent_background 与后台 swarm 共用）：全局并发槽
/// 排队（排队中可被 TaskStop 取消，注册表已是 Killed 直接退出）→ 驱动 → 实时面板/
/// 注册表收尾 → note_output → task_notify；未被取消的组 <task-notification> 经
/// wake 通道唤醒父会话（正文给 result.md 指针 + Read 引导，不内联全文——
/// sessions/ 子树在 extra_read_roots 白名单内，模型需要时自己 Read）。
/// usage 在此丢弃：后台子代理成本不计入父回合统计。
async fn drive_subagent_detached(
    mut ctx: DetachedGateCtx,
    drive: SubagentDrive,
    task_id: String,
    cancel: CancellationToken,
    tx: async_channel::Sender<Event>,
    started_at: std::time::Instant,
) {
    let agent_id = drive.agent_id.clone();
    let profile_name = drive.profile.name.clone();
    // 通知开标签的结构化属性（UI 紧凑卡用；正文保持逐字不变）
    let description_attr = sanitize_notification_attr(&drive.description, 60);
    let model_attr = format!(
        "{} · {}",
        drive.child_config.provider_name, drive.child_config.model
    );
    // 记录文件路径（子代理上下文 JSONL 绝对路径，过同样的属性消毒）
    let record_attr = sanitize_notification_attr(&drive.jsonl.display().to_string(), 512);
    // 结果全文路径（drive_subagent 收尾落盘；通知正文给指针 + Read 引导）
    let result_path = agent_result_path(&drive.jsonl, &drive.agent_id);
    let result_attr = sanitize_notification_attr(&result_path.display().to_string(), 512);
    // 全局并发槽：超限排队
    if crate::task::subagent_slots_available() == 0 {
        crate::task::note_output(
            &ctx.state.tasks,
            &task_id,
            "排队中：子代理全局并发槽已满，等待空槽…\n",
        );
    }
    let Some(_permit) = crate::task::acquire_subagent_slot(&cancel).await else {
        return;
    };
    crate::task::mark_agent_task_started(&ctx.state, &task_id);
    let mut tracker = ChangeTracker::default();
    let result = {
        // 子代理继承的 MCP 工具（owned：闭包内现取，规则快照在 drive 上）
        let mcp_extra = drive.extra_tools();
        let mut gate = GateCtx {
            cwd: &ctx.cwd,
            mode: ctx.mode,
            tracker: &mut tracker,
            state: &ctx.state,
            pending: &ctx.pending,
            permissions: &ctx.permissions,
            always_allowed: &mut ctx.always_allowed,
            session_id: &ctx.session_id,
            seq: &ctx.seq,
            store: &ctx.store,
            extra_tools: &mcp_extra,
        };
        let mut drive = drive;
        drive_subagent(
            &mut gate,
            &mut drive,
            &ProgressSink::Background {
                task_id: task_id.clone(),
            },
            &tx,
            &cancel,
        )
        .await
    };
    // 实时面板收尾（含 TaskStop 被杀）：右侧「子代理」tab 关「运行中」指示
    emit_bg(&ctx.session_id, &ctx.seq, &tx, |sid, seq| {
        Event::SubagentActivity {
            session_id: sid,
            seq,
            agent_id: agent_id.clone(),
            item: None,
            finished: true,
        }
    });
    // 注册表收尾：TaskStop 已置 Killed 的不覆写（cancelled 情形）
    {
        let mut tasks = ctx.state.tasks.lock().expect("task registry lock");
        if let Some(entry) = tasks.iter_mut().find(|t| t.id == task_id)
            && matches!(entry.status, pig_protocol::TaskStatus::Running)
        {
            entry.status = if result.is_error {
                pig_protocol::TaskStatus::Exited(-1)
            } else {
                pig_protocol::TaskStatus::Exited(0)
            };
            entry.ended_at = Some(crate::rollout::now_secs());
        }
    }
    crate::task::note_output(
        &ctx.state.tasks,
        &task_id,
        &format!("[{}]\n{}\n", result.status_line, result.result_text),
    );
    let _ = ctx.state.task_notify.send(ctx.session_id.clone());
    // 被 TaskStop 杀掉的不唤醒父会话
    if result.cancelled {
        return;
    }
    // 开标签的结构化属性不动（UI 紧凑卡数据源）；正文对齐 kimi-code：
    // 状态行 + 结果文件路径 + Read 引导
    let duration_ms = started_at.elapsed().as_millis() as u64;
    let duration = human_duration(duration_ms);
    let written_size = std::fs::metadata(&result_path).ok().map(|m| m.len());
    let body = if result.is_error {
        match written_size {
            Some(bytes) => format!(
                "后台子代理 {agent_id}（{profile_name}）失败：{}（耗时 {duration}）。\n详细输出已写入 {}（{}），可用 Read 查看。\n用 Agent(resume=\"{agent_id}\", prompt=\"...\") 可继续（子代理保留全部上文，续跑时让它重做没拿到结果的那步）。",
                result.result_text,
                result_path.display(),
                human_size(bytes)
            ),
            // 落盘失败降级：无文件可指，原因内联
            None => format!(
                "后台子代理 {agent_id}（{profile_name}）失败：{}（耗时 {duration}）。\n用 Agent(resume=\"{agent_id}\", prompt=\"...\") 可继续（子代理保留全部上文，续跑时让它重做没拿到结果的那步）。",
                result.result_text
            ),
        }
    } else {
        match written_size {
            Some(bytes) => format!(
                "后台子代理 {agent_id}（{profile_name}）已完成（{} 步，耗时 {duration}）。\n结果已写入 {}（{}），需要内容请用 Read 读取该文件。\n用 Agent(resume=\"{agent_id}\", prompt=\"...\") 可继续该子代理。",
                result.turns,
                result_path.display(),
                human_size(bytes)
            ),
            // 落盘失败降级：无文件可指，内联 ≤3000 字符预览（kimi 同款兜底）
            None => format!(
                "后台子代理 {agent_id}（{profile_name}）已完成（{} 步，耗时 {duration}）。\n\n{}\n\n用 Agent(resume=\"{agent_id}\", prompt=\"...\") 可继续该子代理。",
                result.turns,
                result.result_text.chars().take(3000).collect::<String>()
            ),
        }
    };
    let notification = if result.is_error {
        format!(
            "<task-notification agent_id=\"{agent_id}\" profile=\"{profile_name}\" status=\"failed\" turns=\"{}\" model=\"{model_attr}\" description=\"{description_attr}\" duration_ms=\"{duration_ms}\" record=\"{record_attr}\" result=\"{result_attr}\">\n{body}\n</task-notification>",
            result.turns
        )
    } else {
        format!(
            "<task-notification agent_id=\"{agent_id}\" profile=\"{profile_name}\" status=\"completed\" turns=\"{}\" model=\"{model_attr}\" description=\"{description_attr}\" duration_ms=\"{duration_ms}\" record=\"{record_attr}\" result=\"{result_attr}\">\n{body}\n</task-notification>",
            result.turns
        )
    };
    let _ = ctx.state.wake_notify.send((ctx.session_id, notification));
}

/// 子代理驱动（前台/后台共用）：步循环 + 收窄工具门控 + 上下文持久化。
/// 全 owned：前台借 Session 字段组 GateCtx，后台连 GateCtx 也全 owned。
struct SubagentDrive {
    agent_id: String,
    cwd: PathBuf,
    data_dir: PathBuf,
    profile: crate::agent::AgentProfile,
    child_config: ResolvedModel,
    tools: Vec<Box<dyn tool::Tool>>,
    schemas: Vec<serde_json::Value>,
    history: Vec<ChatMsg>,
    jsonl: PathBuf,
    max_turns: usize,
    description: String,
    /// 会话 MCP 句柄（None = 未连接）：后台/闭包在 GateCtx 组装点现取继承工具
    mcp: Option<std::sync::Arc<crate::mcp::McpManager>>,
    /// 继承规则快照（准备时按档案收窄结果判定）：true = 继承全部已连接 MCP 工具；
    /// false = 只读档案，只继承 readOnlyHint 的
    mcp_inherits_all: bool,
}

impl SubagentDrive {
    /// 门控执行段的 extra_tools：按继承规则从 MCP 句柄现取（McpTool clone 很便宜）
    /// + Skill（不在 all() 静态表里，经 extra 按名兜底）
    fn extra_tools(&self) -> Vec<Box<dyn tool::Tool>> {
        let mut extra = self
            .mcp
            .as_ref()
            .map(|mcp| mcp.child_tools(self.mcp_inherits_all))
            .unwrap_or_default();
        extra.push(Box::new(tool::SkillTool::new(&self.cwd, &self.data_dir)));
        extra
    }
}

/// 代理卡副标题的模型段："{provider_name} · {model}"（可带思考档后缀）
fn agent_card_model(drive: &SubagentDrive) -> String {
    let mut model = format!(
        "{} · {}",
        drive.child_config.provider_name, drive.child_config.model
    );
    if let Some(level) = &drive.profile.thought_level {
        model = format!("{model} · {level}");
    }
    model
}

/// SwarmChildPrep → SubagentDrive：字段一一对应，准备侧与驱动侧的结构映射收口在这里
///（run_swarm 前台/后台两分支共用）
impl From<crate::agent::SwarmChildPrep> for SubagentDrive {
    fn from(prep: crate::agent::SwarmChildPrep) -> Self {
        Self {
            agent_id: prep.agent_id,
            cwd: prep.cwd,
            data_dir: prep.data_dir,
            profile: prep.profile,
            child_config: prep.child_config,
            tools: prep.tools,
            schemas: prep.schemas,
            history: prep.history,
            jsonl: prep.jsonl,
            max_turns: prep.max_turns,
            description: prep.description,
            mcp: prep.mcp,
            mcp_inherits_all: prep.mcp_inherits_all,
        }
    }
}
/// 子代理进度上报出口
enum ProgressSink {
    /// 前台：SubagentProgress 实时发到父会话 Agent 工具卡
    Foreground { parent_item_id: String },
    /// 后台：写任务注册表 output（父卡已结束，不发 SubagentProgress）
    Background { task_id: String },
}
impl ProgressSink {
    /// 子工具 item_id / 审批 request_id 的归属前缀
    fn item_prefix(&self) -> &str {
        match self {
            ProgressSink::Foreground { parent_item_id } => parent_item_id,
            ProgressSink::Background { task_id } => task_id,
        }
    }
}
/// 子代理进度上报（按 sink 分流，见 ProgressSink）
fn report_progress(
    ctx: &GateCtx<'_>,
    sink: &ProgressSink,
    tx: &async_channel::Sender<Event>,
    note: String,
) {
    match sink {
        ProgressSink::Foreground { parent_item_id } => {
            let item_id = parent_item_id.clone();
            emit_bg(ctx.session_id, ctx.seq, tx, |session_id, seq| {
                Event::SubagentProgress {
                    session_id,
                    seq,
                    item_id,
                    note,
                }
            });
        }
        ProgressSink::Background { task_id } => {
            crate::task::note_output(&ctx.state.tasks, task_id, &format!("{note}\n"));
        }
    }
}
/// 子代理上下文 JSONL 追加一行（失败非致命：打日志继续，与 rollout.append 同口径）
fn persist_agent_line(jsonl: &Path, line: &serde_json::Value) {
    if let Err(error) = crate::agent::append_agent_record(jsonl, line) {
        eprintln!("[agent] 子代理上下文落盘失败: {error}");
    }
}
/// 子代理消息落盘：base64 不落盘（与主 rollout 同口径）；resume 后模型看不到图，可接受
fn persist_agent_msg(jsonl: &Path, msg: &ChatMsg) {
    let mut msg = msg.clone();
    msg.images.clear();
    persist_agent_line(jsonl, &serde_json::json!({ "type": "msg", "msg": msg }));
}
/// 通知开标签属性值消毒：去 `"` 与换行（防标签被截断/注入），截 max_chars 字符
///（description 用 60；record 路径用 512——绝对路径远超 60）
fn sanitize_notification_attr(text: &str, max_chars: usize) -> String {
    text.chars()
        .filter(|c| !matches!(c, '"' | '\n' | '\r'))
        .take(max_chars)
        .collect()
}
/// 耗时人类可读格式：≥60s → "Xm Ys"，否则 "X.X 秒"
fn human_duration(ms: u64) -> String {
    if ms >= 60_000 {
        format!("{}m {}s", ms / 60_000, (ms % 60_000) / 1000)
    } else {
        format!("{:.1} 秒", ms as f64 / 1000.0)
    }
}
/// 文件大小人类可读格式：KB/MB 一位小数
fn human_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    }
}
/// drive_subagent 的结果
struct SubagentDriveResult {
    /// 单行状态（注册表 output 收尾用）
    status_line: String,
    /// 结果文本（completed 已过 32K 预算截断+落盘；失败为原因；cancelled 为空）
    result_text: String,
    /// 实际步数
    turns: usize,
    /// 子代理正常收尾（轮次用尽/请求失败为 false）——前台模板组装用
    completed: bool,
    is_error: bool,
    /// 采样 token 累计 (input, cache_read, output)：前台累加进父回合统计，后台丢弃
    usage: (u64, u64, u64),
    /// 被驱动的 cancel token 中止（后台 TaskStop / 前台父取消）
    cancelled: bool,
}
/// 子代理结果文件路径：{agents_dir}/{agent_id}.result.md（与上下文 jsonl 同目录）
fn agent_result_path(jsonl: &Path, agent_id: &str) -> PathBuf {
    jsonl.with_file_name(format!("{agent_id}.result.md"))
}
/// 子代理驱动入口：跑驱动循环，结果全文先落 {agents_dir}/{agent_id}.result.md
///（成功/失败都写，失败写的是错误说明；取消/被杀无产物跳过；写失败仅降级、
/// 截断提示无文件可指，不致命），随后按 32K 预算截断 result_text。
async fn drive_subagent(
    ctx: &mut GateCtx<'_>,
    run: &mut SubagentDrive,
    progress: &ProgressSink,
    tx: &async_channel::Sender<Event>,
    cancel: &CancellationToken,
) -> SubagentDriveResult {
    let result_path = agent_result_path(&run.jsonl, &run.agent_id);
    let mut result = drive_subagent_loop(ctx, run, progress, tx, cancel).await;
    if !result.cancelled {
        let written = std::fs::write(&result_path, &result.result_text).is_ok();
        result.result_text = truncate_agent_result(
            written.then_some(result_path.as_path()),
            std::mem::take(&mut result.result_text),
        );
    }
    result
}
/// 子代理实时展示项上报（live-only，前台/后台都发）：把 history[emitted..] 的
/// 新增消息投影成展示项逐条发出（批内 tool 结果按 tool_call_id 回填 output），
/// 并前移水位。面板经 agent_id 认领。
fn emit_activity_since(
    ctx: &GateCtx<'_>,
    tx: &async_channel::Sender<Event>,
    agent_id: &str,
    history: &[ChatMsg],
    emitted: &mut usize,
) {
    for item in crate::agent::project_display_items(&history[*emitted..]) {
        let agent_id = agent_id.to_string();
        emit_bg(ctx.session_id, ctx.seq, tx, move |session_id, seq| {
            Event::SubagentActivity {
                session_id,
                seq,
                agent_id,
                item: Some(item),
                finished: false,
            }
        });
    }
    *emitted = history.len();
}
/// 子代理驱动循环（前台/后台共用）：独立上下文采样 + 收窄工具集门控执行，
/// 父时间线只有 Agent 一张工具卡（前台进度走 SubagentProgress，子工具不发顶层事件）；
/// 每个 step 追加的消息经 SubagentActivity 实时上报（右侧「子代理」tab 增量展示）。
async fn drive_subagent_loop(
    ctx: &mut GateCtx<'_>,
    run: &mut SubagentDrive,
    progress: &ProgressSink,
    tx: &async_channel::Sender<Event>,
    cancel: &CancellationToken,
) -> SubagentDriveResult {
    let mut usage = (0u64, 0u64, 0u64);
    let mut last_text = String::new();
    let mut steps_run = 0usize;
    let mut completed = false;
    // 实时上报水位：history 里已投影为 SubagentActivity 的前缀长度
    //（初始 system/user/resume 历史不算——它们由 LoadSubagent 全量加载覆盖）
    let mut emitted_up_to = run.history.len();
    for step in 1..=run.max_turns {
        steps_run = step;
        report_progress(ctx, progress, tx, format!("第 {step} 步 · 思考中…"));
        let (child_tx, mut child_rx) = tokio::sync::mpsc::unbounded_channel();
        let provider_task = tokio::spawn(provider::stream_chat(
            run.child_config.clone(),
            run.history.clone(),
            run.schemas.clone(),
            child_tx,
            cancel.clone(),
        ));
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut failed: Option<String> = None;
        loop {
            let event = tokio::select! {
                event = child_rx.recv() => event,
                _ = cancel.cancelled() => None,
            };
            match event {
                // 子代理的思考/文本增量不转发顶层事件：父时间线只有进度状态行
                Some(ProviderEvent::Reasoning(delta)) => reasoning.push_str(&delta),
                Some(ProviderEvent::Text(delta)) => text.push_str(&delta),
                Some(ProviderEvent::ToolCalls(calls)) => tool_calls = calls,
                Some(ProviderEvent::Usage {
                    input,
                    cache_read,
                    output,
                    ..
                }) => {
                    // token 用量经返回值带出：前台累加父回合统计，后台丢弃；
                    // 不记 StepUsage/不更新水位（水位是父会话自己的上下文）
                    usage.0 += input;
                    usage.1 += cache_read;
                    usage.2 += output;
                }
                Some(ProviderEvent::Finished) | None => break,
                Some(ProviderEvent::Failed(error)) => {
                    failed = Some(error);
                    break;
                }
            }
        }
        if let Some(error) = failed {
            provider_task.abort();
            // 收尾前把本步已落历史的消息补报给实时面板（下同）
            emit_activity_since(ctx, tx, &run.agent_id, &run.history, &mut emitted_up_to);
            let note = format!("子代理模型请求失败: {error}");
            return SubagentDriveResult {
                status_line: format!("failed: {note}"),
                result_text: note,
                turns: steps_run,
                completed: false,
                is_error: true,
                usage,
                cancelled: false,
            };
        }
        if cancel.is_cancelled() {
            provider_task.abort();
            emit_activity_since(ctx, tx, &run.agent_id, &run.history, &mut emitted_up_to);
            return SubagentDriveResult {
                status_line: "cancelled".to_string(),
                result_text: String::new(),
                turns: steps_run,
                completed: false,
                is_error: false,
                usage,
                cancelled: true,
            };
        }
        let _ = provider_task.await;
        let assistant = ChatMsg::assistant(
            text,
            tool_calls.clone(),
            Some(reasoning).filter(|r| !r.is_empty()),
        );
        last_text = assistant.content.clone().unwrap_or_default();
        run.history.push(assistant);
        persist_agent_msg(&run.jsonl, run.history.last().expect("assistant pushed"));
        if tool_calls.is_empty() {
            // 本步只有 assistant 一条：收尾前上报
            emit_activity_since(ctx, tx, &run.agent_id, &run.history, &mut emitted_up_to);
            completed = true;
            break;
        }
        for child_call in &tool_calls {
            report_progress(
                ctx,
                progress,
                tx,
                format!("第 {step} 步 · {}", tool::summarize(child_call)),
            );
            let child_item_id = format!("{}-c{step}-{}", progress.item_prefix(), child_call.id);
            let Some(child_tool) = run
                .tools
                .iter()
                .find(|t| t.name() == child_call.name)
                .map(|t| t.as_ref())
            else {
                // 收窄后的工具集没有这个名字：记为错误结果继续（不中断子代理）
                let note = format!("未知工具 {}（子代理可用工具已收窄）", child_call.name);
                run.history.push(ChatMsg::tool_result(&child_call.id, note));
                persist_agent_msg(&run.jsonl, run.history.last().expect("tool result pushed"));
                continue;
            };
            match exec_tool_gated_ctx(
                ctx,
                child_call,
                Some(child_tool),
                &child_item_id,
                progress.item_prefix(),
                tx,
                cancel,
            )
            .await
            {
                GatedToolOutcome::Cancelled => {
                    emit_activity_since(ctx, tx, &run.agent_id, &run.history, &mut emitted_up_to);
                    return SubagentDriveResult {
                        status_line: "cancelled".to_string(),
                        result_text: String::new(),
                        turns: steps_run,
                        completed: false,
                        is_error: false,
                        usage,
                        cancelled: true,
                    };
                }
                GatedToolOutcome::Rejected { note } => {
                    run.history.push(ChatMsg::tool_result(&child_call.id, note));
                    persist_agent_msg(&run.jsonl, run.history.last().expect("tool result pushed"));
                }
                GatedToolOutcome::Executed { output, images, .. } => {
                    run.history.push(ChatMsg::tool_result_with_images(
                        &child_call.id,
                        output,
                        images,
                    ));
                    persist_agent_msg(&run.jsonl, run.history.last().expect("tool result pushed"));
                }
            }
        }
        // 步末上报：本步的 assistant（含 tool_calls）+ 全部 tool 结果同批投影，
        // 批内按 tool_call_id 回填 output
        emit_activity_since(ctx, tx, &run.agent_id, &run.history, &mut emitted_up_to);
    }
    // ---- 收尾：未截断全文返回（result.md 落盘与 32K 预算截断在外层 drive_subagent 收口）----
    if completed {
        if last_text.is_empty() {
            SubagentDriveResult {
                status_line: "failed: 子代理未产出最终文本".to_string(),
                result_text: "子代理未产出最终文本".to_string(),
                turns: steps_run,
                completed: true,
                is_error: true,
                usage,
                cancelled: false,
            }
        } else {
            SubagentDriveResult {
                status_line: format!("completed（{steps_run} 步）"),
                result_text: last_text,
                turns: steps_run,
                completed: true,
                is_error: false,
                usage,
                cancelled: false,
            }
        }
    } else if last_text.is_empty() {
        SubagentDriveResult {
            status_line: format!("failed: 已达最大轮次 {}，子代理未产出结论", run.max_turns),
            result_text: format!("已达最大轮次 {}。子代理未产出结论", run.max_turns),
            turns: steps_run,
            completed: false,
            is_error: true,
            usage,
            cancelled: false,
        }
    } else {
        SubagentDriveResult {
            status_line: format!("completed（已达最大轮次 {}）", run.max_turns),
            result_text: format!("已达最大轮次 {}。{last_text}", run.max_turns),
            turns: steps_run,
            completed: false,
            is_error: false,
            usage,
            cancelled: false,
        }
    }
}
/// 子代理结果预算：32K 字符内原样返回；超出返回前 32K + 截断指引。
/// 全文由调用方先行落盘 {agents_dir}/{agent_id}.result.md（kimi output.log 同款），
/// result_path 指它；None = 落盘失败。
fn truncate_agent_result(result_path: Option<&Path>, result: String) -> String {
    const MAX_RESULT_CHARS: usize = 32_000;
    if result.chars().count() <= MAX_RESULT_CHARS {
        return result;
    }
    let hint = match result_path {
        Some(path) => format!("\n\n[结果过长已截断，全文: {}]", path.display()),
        None => "\n\n[结果过长已截断，全文落盘失败]".to_string(),
    };
    let head: String = result.chars().take(MAX_RESULT_CHARS).collect();
    format!("{head}{hint}")
}
