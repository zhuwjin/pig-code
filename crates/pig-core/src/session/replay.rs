use super::*;

impl Session {
    /// resume 重放：按序发 durable 事件，UI 用同一 reduce 逻辑重建视图。
    pub fn replay(&mut self, records: &[RolloutRecord], tx: &async_channel::Sender<Event>) {
        let mut in_assistant = false;
        let mut replay_turns = 0usize;
        // 回放中遇到的后台子代理（去重）：回放结束后统一补 finished 落终态
        let mut replay_bg_agents: Vec<String> = vec![];
        for record in records {
            match record {
                RolloutRecord::Meta { .. } => {}
                RolloutRecord::User {
                    text,
                    files,
                    images,
                } => {
                    in_assistant = false;
                    let image_count = images.len();
                    // 事件文本带附件链接（与 live 同形态）；rollout 里的原文保持干净
                    let text = crate::rollout::user_display_text(text, images);
                    let files = files.clone();
                    self.emit(
                        |session_id, seq| Event::UserMessage {
                            session_id,
                            seq,
                            text,
                            files,
                            image_count,
                        },
                        tx,
                    );
                }
                RolloutRecord::Reasoning { text } => {
                    if !in_assistant {
                        replay_turns += 1;
                        let turn_id = format!("replay-{replay_turns}");
                        self.emit(
                            |session_id, seq| Event::TurnStarted {
                                session_id,
                                seq,
                                turn_id,
                            },
                            tx,
                        );
                        in_assistant = true;
                    }
                    let item = format!("replay-{replay_turns}-reason");
                    let delta = text.clone();
                    self.emit(
                        |session_id, seq| Event::ReasoningDelta {
                            session_id,
                            seq,
                            item_id: item,
                            delta,
                        },
                        tx,
                    );
                }
                RolloutRecord::Text { text } => {
                    if !in_assistant {
                        replay_turns += 1;
                        let turn_id = format!("replay-{replay_turns}");
                        self.emit(
                            |session_id, seq| Event::TurnStarted {
                                session_id,
                                seq,
                                turn_id,
                            },
                            tx,
                        );
                        in_assistant = true;
                    }
                    let item = format!(
                        "replay-{replay_turns}-text-{}",
                        self.seq.load(std::sync::atomic::Ordering::Relaxed)
                    );
                    let full = text.clone();
                    self.emit(
                        |session_id, seq| Event::TextDone {
                            session_id,
                            seq,
                            item_id: item,
                            full_text: full,
                        },
                        tx,
                    );
                }
                RolloutRecord::ToolCall {
                    tool,
                    summary: _,
                    arguments,
                    output,
                    is_error,
                    edit,
                    agent_card,
                } => {
                    if !in_assistant {
                        replay_turns += 1;
                        let turn_id = format!("replay-{replay_turns}");
                        self.emit(
                            |session_id, seq| Event::TurnStarted {
                                session_id,
                                seq,
                                turn_id,
                            },
                            tx,
                        );
                        in_assistant = true;
                    }
                    let item = format!(
                        "replay-{replay_turns}-tool-{}",
                        self.seq.load(std::sync::atomic::Ordering::Relaxed)
                    );
                    let detail = serde_json::from_str::<serde_json::Value>(arguments)
                        .map(|v| serde_json::to_string_pretty(&v).unwrap_or_default())
                        .unwrap_or_else(|_| arguments.clone());
                    let (tool, output) = (tool.clone(), output.clone());
                    let is_error = *is_error;
                    // 回放统一从完整参数重算 summary，summary 逻辑演进后回放也一致
                    let summary = tool::summarize(&crate::provider::ToolCall {
                        id: String::new(),
                        name: tool.clone(),
                        arguments: arguments.clone(),
                    });
                    let edit = edit.clone();
                    self.emit(
                        |session_id, seq| Event::ToolCallBegin {
                            session_id,
                            seq,
                            item_id: item.clone(),
                            tool,
                            input_summary: summary,
                            detail,
                        },
                        tx,
                    );
                    // 代理卡元信息随记录回放重建（紧挨 ToolCallBegin、同一个回放合成
                    // item_id）；后台代理的 agent_id 收集起来，回放结束后统一补
                    // finished——后台任务不随进程存活，重开后一律视为已终结，
                    // 否则回放的代理卡永转圈
                    if let Some(card) = agent_card {
                        if card.background
                            && !replay_bg_agents.iter().any(|id| id == &card.agent_id)
                        {
                            replay_bg_agents.push(card.agent_id.clone());
                        }
                        let card = card.clone();
                        self.emit(
                            |session_id, seq| Event::SubagentCard {
                                session_id,
                                seq,
                                item_id: item.clone(),
                                agent_id: card.agent_id.clone(),
                                profile: card.profile.clone(),
                                description: card.description.clone(),
                                model: card.model.clone(),
                                background: card.background,
                            },
                            tx,
                        );
                    }
                    self.emit(
                        |session_id, seq| Event::ToolCallEnd {
                            session_id,
                            seq,
                            item_id: item,
                            output,
                            is_error,
                            edit,
                        },
                        tx,
                    );
                }
                RolloutRecord::Compact { .. } => {}
                RolloutRecord::TurnStats {
                    input,
                    cache_read,
                    output,
                    duration_ms,
                    api_ms,
                    ttft_ms,
                    api_steps,
                } => {
                    // 回放恢复：会话累计 + 历史回合的 footer 统计（水位由 StepUsage 恢复）
                    self.input_total += input;
                    self.cache_read_total += cache_read;
                    let stats = pig_protocol::TurnUsageStats {
                        input: *input,
                        cache_read: *cache_read,
                        output: *output,
                        duration_ms: *duration_ms,
                        api_ms: *api_ms,
                        ttft_ms: *ttft_ms,
                        api_steps: *api_steps,
                    };
                    // duration_ms=0 保持「回放收尾事件」语义（不触发计划模式待执行等
                    // 新回合逻辑）；footer 的真实耗时从 stats 里取
                    self.emit(
                        |session_id, seq| Event::TurnComplete {
                            session_id,
                            seq,
                            duration_ms: 0,
                            stats: Some(stats),
                        },
                        tx,
                    );
                }
                RolloutRecord::TurnChanges { files } => {
                    let files = files.clone();
                    self.emit(
                        |session_id, seq| Event::TurnFileChanges {
                            session_id,
                            seq,
                            files,
                        },
                        tx,
                    );
                }
                RolloutRecord::StepUsage { used, .. } => {
                    // 水位 = 单次请求的总 token，逐条覆盖、最后一条生效
                    self.last_total_tokens = Some(*used);
                }
            }
        }
        // SQLite 里的面板当前态在 JSONL 事件流之后补发：
        // - todos 表 → 恢复待办并推快照
        // - file_changes 表 → 按路径逐条补 FileChanged（UI upsert 重建改动列表）
        let (todos_json, db_changes) = {
            let store = self.store.lock().expect("store lock");
            (store.get_todos(&self.id), store.file_changes(&self.id))
        };
        if let Some(json) = todos_json
            && let Ok(items) = serde_json::from_str::<Vec<pig_protocol::TodoItem>>(&json)
        {
            *self.state.todos.lock().expect("todos lock") = items.clone();
            self.emit(
                |session_id, seq| Event::TodoListChanged {
                    session_id,
                    seq,
                    items,
                },
                tx,
            );
        }
        for (path, unified_diff, additions, deletions) in db_changes {
            self.emit(
                |session_id, seq| Event::FileChanged {
                    session_id,
                    seq,
                    path,
                    unified_diff,
                    additions,
                    deletions,
                },
                tx,
            );
        }
        // 回放的历史回合以 TurnStarted 开头但记录里没有结尾事件；
        // 用 duration_ms=0 的 TurnComplete 收尾，让 UI 退出流式状态
        //（UI 据此跳过「回合结束·用时」脚注与计划模式待执行标记）
        if in_assistant {
            self.emit(
                |session_id, seq| Event::TurnComplete {
                    session_id,
                    seq,
                    duration_ms: 0,
                    stats: None,
                },
                tx,
            );
        }
        // 后台子代理的代理卡落终态：后台任务不随进程存活，重开后一律视为已终结
        //（放在收尾 TurnComplete 之后，UI 已在非流式态）
        for agent_id in replay_bg_agents {
            self.emit(
                |session_id, seq| Event::SubagentActivity {
                    session_id,
                    seq,
                    agent_id,
                    item: None,
                    finished: true,
                },
                tx,
            );
        }
    }


}
