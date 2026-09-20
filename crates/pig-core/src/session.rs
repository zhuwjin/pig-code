use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use futures_util::stream::FuturesUnordered;
use futures_util::StreamExt as _;
use pig_protocol::{ApprovalDecision, Event, ExecMode, Op, SessionMeta};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::config;
use crate::provider::ResolvedModel;
use pig_protocol::AppConfig;
use crate::project::ProjectStore;
use crate::provider::{ChatMsg, ProviderEvent, ToolCall};
use crate::rollout::{Rollout, RolloutRecord, SessionIndex, now_secs, rebuild_history};
use crate::tool::{ChangeTracker, ToolContext};
use crate::{prompt, provider, tool};

/// 等待中的审批：request_id → 回执通道。manager 与各 session 共享；
/// request_id 带 session_id 前缀，全局唯一。
pub type PendingApprovals = Arc<Mutex<HashMap<String, oneshot::Sender<ApprovalDecision>>>>;

/// 会话级模型覆盖（SetModel）
#[derive(Clone, Debug, Default)]
pub struct ModelSelection {
    pub provider_id: String,
    pub model_id: String,
    pub reasoning_level: Option<String>,
}

/// provider+model(+推理等级) → 端点解析
pub fn resolve_model(
    config: &AppConfig,
    selection: Option<&ModelSelection>,
) -> Option<ResolvedModel> {
    let (provider_id, model_id, level) = match selection {
        Some(sel) => (
            sel.provider_id.clone(),
            sel.model_id.clone(),
            sel.reasoning_level.clone(),
        ),
        None => (
            config.default_provider.clone(),
            config.default_model.clone(),
            None,
        ),
    };
    let provider = config
        .providers
        .iter()
        .find(|p| p.enabled && p.id == provider_id)
        .or_else(|| config.providers.iter().find(|p| p.enabled))?;
    let model = provider
        .models
        .iter()
        .find(|m| m.id == model_id)
        .or_else(|| provider.models.first())?;
    let reasoning_params = level
        .as_ref()
        .and_then(|level| model.reasoning_params.get(level).cloned());
    Some(ResolvedModel {
        base_url: provider.base_url.clone(),
        api_key: config::expand_env(&provider.api_key),
        model: model.id.clone(),
        context_window: model.context_window,
        max_output_tokens: model.max_output_tokens,
        api_format: provider.api_format,
        reasoning_params,
        provider_name: provider.name.clone(),
    })
}

pub struct Session {
    pub id: String,
    pub cwd: PathBuf,
    history: Vec<ChatMsg>,
    seq: u64,
    turn_counter: u64,
    tracker: ChangeTracker,
    always_allowed: HashSet<String>,
    pending: PendingApprovals,
    mode: ExecMode,
    model_override: Option<ModelSelection>,
    rollout: Option<Rollout>,
    index: Arc<Mutex<SessionIndex>>,
    data_dir: PathBuf,
    last_total_tokens: Option<u64>,
}

enum StepOutcome {
    TextOnly,
    ToolsExecuted,
    Ended,
}

impl Session {
    pub fn create(
        meta: SessionMeta,
        pending: PendingApprovals,
        index: Arc<Mutex<SessionIndex>>,
        sessions_dir: &Path,
        data_dir: PathBuf,
    ) -> Result<Self, String> {
        let rollout = Rollout::create(sessions_dir, &meta)?;
        Ok(Self {
            id: meta.id,
            cwd: meta.cwd,
            history: Vec::new(),
            seq: 0,
            turn_counter: 0,
            tracker: ChangeTracker::default(),
            always_allowed: HashSet::new(),
            pending,
            mode: ExecMode::ConfirmBeforeEdit,
            model_override: None,
            rollout: Some(rollout),
            index,
            data_dir,
            last_total_tokens: None,
        })
    }

    /// 从 rollout 重建。注意：ChangeTracker 快照为空——resume 后 revert 只能回退
    /// 本次进程内的变更，历史变更的基线是当前磁盘内容（已知限制，M5+ 再议）。
    pub fn load(
        id: &str,
        sessions_dir: &Path,
        pending: PendingApprovals,
        index: Arc<Mutex<SessionIndex>>,
        data_dir: PathBuf,
    ) -> Result<(Self, Vec<RolloutRecord>), String> {
        let records = Rollout::load(&sessions_dir.join(format!("{id}.jsonl")))?;
        let Some(RolloutRecord::Meta { cwd, .. }) = records.first() else {
            return Err(format!("rollout 缺少 meta 行: {id}"));
        };
        let history = rebuild_history(
            &records,
            prompt::system_prompt(cwd, true, ExecMode::ConfirmBeforeEdit, &data_dir),
        );
        let session = Self {
            id: id.to_string(),
            cwd: cwd.clone(),
            history,
            seq: 0,
            turn_counter: 0,
            tracker: ChangeTracker::default(),
            always_allowed: HashSet::new(),
            pending,
            mode: ExecMode::ConfirmBeforeEdit,
            model_override: None,
            rollout: None,
            index,
            data_dir,
            last_total_tokens: None,
        };
        Ok((session, records))
    }

    fn emit(&mut self, build: impl FnOnce(String, u64) -> Event, tx: &async_channel::Sender<Event>) {
        self.seq += 1;
        let _ = tx.send_blocking(build(self.id.clone(), self.seq));
    }

    fn record(&mut self, record: &RolloutRecord) {
        if let Some(rollout) = &mut self.rollout {
            rollout.append(record);
        }
    }

    fn touch_index(&mut self) {
        let id = self.id.clone();
        self.index
            .lock()
            .expect("index lock")
            .update(&id, |meta| meta.updated_at = now_secs());
    }

    /// resume 重放：按序发 durable 事件，UI 用同一 reduce 逻辑重建视图。
    pub fn replay(&mut self, records: &[RolloutRecord], tx: &async_channel::Sender<Event>) {
        let mut in_assistant = false;
        let mut replay_turns = 0usize;
        for record in records {
            match record {
                RolloutRecord::Meta { .. } => {}
                RolloutRecord::User { text, files } => {
                    in_assistant = false;
                    let text = text.clone();
                    let files = files.clone();
                    self.emit(
                        |session_id, seq| Event::UserMessage {
                            session_id,
                            seq,
                            text,
                            files,
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
                    let item = format!("replay-{replay_turns}-text-{}", self.seq);
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
                    summary,
                    arguments,
                    output,
                    is_error,
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
                    let item = format!("replay-{replay_turns}-tool-{}", self.seq);
                    let detail = serde_json::from_str::<serde_json::Value>(arguments)
                        .map(|v| serde_json::to_string_pretty(&v).unwrap_or_default())
                        .unwrap_or_else(|_| arguments.clone());
                    let (tool, summary, output) = (tool.clone(), summary.clone(), output.clone());
                    let is_error = *is_error;
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
                    self.emit(
                        |session_id, seq| Event::ToolCallEnd {
                            session_id,
                            seq,
                            item_id: item,
                            output,
                            is_error,
                        },
                        tx,
                    );
                }
                RolloutRecord::FileChange {
                    path,
                    unified_diff,
                    additions,
                    deletions,
                } => {
                    let (path, diff) = (path.clone(), unified_diff.clone());
                    let (additions, deletions) = (*additions, *deletions);
                    self.emit(
                        |session_id, seq| Event::FileChanged {
                            session_id,
                            seq,
                            path,
                            unified_diff: diff,
                            additions,
                            deletions,
                        },
                        tx,
                    );
                }
                RolloutRecord::Compact { .. } => {}
            }
        }
    }

    pub fn set_mode(&mut self, mode: ExecMode) {
        self.mode = mode;
    }

    pub fn set_model(&mut self, selection: ModelSelection) {
        self.model_override = Some(selection);
    }

    /// compact：优先模型摘要；失败回退朴素截断。返回 false = 被取消。
    /// 历史重建为 system + 摘要消息 + 最近 4 条原样消息。
    pub async fn run_compact(
        &mut self,
        config: Option<&ResolvedModel>,
        automatic: bool,
        tx: &async_channel::Sender<Event>,
        cancel: &CancellationToken,
    ) -> bool {
        const KEEP: usize = 4;
        if self.history.len() <= KEEP + 1 {
            self.emit(
                |session_id, seq| Event::ContextCompacted {
                    session_id,
                    seq,
                    omitted: 0,
                    note: "历史很短，无需压缩".to_string(),
                    automatic,
                },
                tx,
            );
            return true;
        }
        let omitted = self.history.len() - 1 - KEEP;
        let changed: Vec<String> = self.tracker.tracked_paths();

        let summary = match config {
            Some(config) => {
                let prompt_text = compaction_prompt(&self.history);
                match provider::complete_text(config, prompt_text, cancel).await {
                    Ok(summary) => Some(summary),
                    Err(_) if cancel.is_cancelled() => {
                        self.emit(|session_id, seq| Event::TurnAborted { session_id, seq }, tx);
                        return false;
                    }
                    Err(error) => {
                        eprintln!("[pig-core] 摘要请求失败，回退截断: {error}");
                        None
                    }
                }
            }
            None => None,
        };

        let note = match &summary {
            Some(summary) => format!(
                "[前文已压缩{}·模型摘要] 省略 {omitted} 条消息。

{summary}

{}",
                if automatic { "（自动）" } else { "" },
                if changed.is_empty() {
                    "期间无文件变更。".to_string()
                } else {
                    format!("期间修改的文件: {}", changed.join(", "))
                }
            ),
            None => format!(
                "[前文已压缩{}] 共 {omitted} 条消息被省略。{}",
                if automatic { "（自动）" } else { "" },
                if changed.is_empty() {
                    "期间无文件变更。".to_string()
                } else {
                    format!("期间修改的文件: {}", changed.join(", "))
                }
            ),
        };

        let mut tail: Vec<ChatMsg> = self.history[self.history.len() - KEEP..].to_vec();
        // 真实 API 不接受没有 assistant 前置的孤儿 tool 消息，裁到安全边界
        while tail.first().is_some_and(|m| m.role == "tool") {
            tail.remove(0);
        }
        self.history = vec![self.history[0].clone(), ChatMsg::system(note.clone())];
        self.history.extend(tail);
        self.record(&RolloutRecord::Compact {
            note: note.clone(),
            omitted,
        });
        self.emit(
            |session_id, seq| Event::ContextCompacted {
                session_id,
                seq,
                omitted,
                note,
                automatic,
            },
            tx,
        );
        self.touch_index();
        true
    }

    pub fn revert_file(&mut self, path: &str, tx: &async_channel::Sender<Event>) {
        let result =
            tool::resolve_checked(&self.cwd, path, false).and_then(|full| self.tracker.revert(&full));
        match result {
            Ok(()) => {
                self.emit(
                    |session_id, seq| Event::FileReverted {
                        session_id,
                        seq,
                        path: path.to_string(),
                    },
                    tx,
                );
            }
            Err(error) => self.emit(
                |session_id, seq| Event::Error {
                    session_id: Some(session_id),
                    seq,
                    message: error,
                },
                tx,
            ),
        }
    }

    pub async fn run_turn(
        &mut self,
        content: String,
        files: Vec<String>,
        config: &ResolvedModel,
        tx: &async_channel::Sender<Event>,
        cancel: CancellationToken,
    ) {
        self.turn_counter += 1;
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
        ));
        if self.history.is_empty() {
            self.history.push(system);
        } else if self.history[0].role == "system" {
            self.history[0] = system;
        }
        let mut user_text = expand_file_references(&self.cwd, &content, &files);
        if self.history.len() == 1 {
            let title: String = content.chars().take(30).collect();
            let id = self.id.clone();
            self.index.lock().expect("index lock").update(&id, |meta| {
                meta.title = title;
                meta.updated_at = now_secs();
            });
        }
        // user_text 含展开后的文件内容；rollout 只记原文
        let rollout_text = if files.is_empty() {
            content.clone()
        } else {
            format!("{content}\n\n引用文件: {}", files.join(", "))
        };
        let record_files = files.clone();
        self.history.push(ChatMsg::user(std::mem::take(&mut user_text)));
        self.record(&RolloutRecord::User {
            text: rollout_text.clone(),
            files: record_files.clone(),
        });
        self.emit(
            |session_id, seq| Event::UserMessage {
                session_id,
                seq,
                text: rollout_text.clone(),
                files: record_files.clone(),
            },
            tx,
        );

        let mut step = 0usize;
        loop {
            step += 1;
            match self.run_step(turn_id.clone(), step, config, tx, &cancel).await {
                StepOutcome::TextOnly => {
                    self.emit(
                        |session_id, seq| Event::TurnComplete {
                            session_id,
                            seq,
                            duration_ms: started.elapsed().as_millis() as u64,
                        },
                        tx,
                    );
                    self.touch_index();
                    return;
                }
                StepOutcome::ToolsExecuted => continue,
                StepOutcome::Ended => return,
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
        let provider_task = tokio::spawn(provider::stream_chat(
            config.clone(),
            self.history.clone(),
            tool::schemas(),
            event_tx,
            cancel.clone(),
        ));

        let mut text = String::new();
        let mut reasoning = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut provider_failed = false;

        loop {
            let event = tokio::select! {
                event = event_rx.recv() => event,
                _ = cancel.cancelled() => None,
            };
            match event {
                Some(ProviderEvent::Reasoning(delta)) => {
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
                Some(ProviderEvent::Usage { used, total }) => {
                    self.last_total_tokens = Some(used);
                    self.emit(
                        |session_id, seq| Event::ContextUsage {
                            session_id,
                            seq,
                            used,
                            total,
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
            self.record(&RolloutRecord::Reasoning { text: reasoning });
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
        self.history.push(ChatMsg::assistant(text, tool_calls.clone()));
        if tool_calls.is_empty() {
            return StepOutcome::TextOnly;
        }

        let tools = tool::all();
        for call in &tool_calls {
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

            if self.mode == ExecMode::Plan && !read_only {
                let note = "计划模式：修改类工具已被禁止执行。请只输出计划文本，等用户切换到其他模式后再执行。"
                    .to_string();
                self.history.push(ChatMsg::tool_result(&call.id, note.clone()));
                self.record(&RolloutRecord::ToolCall {
                    tool: call.name.clone(),
                    summary,
                    arguments: call.arguments.clone(),
                    output: note.clone(),
                    is_error: true,
                });
                self.emit(
                    |session_id, seq| Event::ToolCallEnd {
                        session_id,
                        seq,
                        item_id,
                        output: note,
                        is_error: true,
                    },
                    tx,
                );
                continue;
            }

            if tool_ref.is_some_and(|t| tool::requires_approval(t.as_ref(), self.mode))
                && !self.always_allowed.contains(call.name.as_str())
            {
                let request_id = format!("{}-{turn_id}-approval-{item_id}", self.id);
                let approval_detail = approval_detail(call, &self.cwd);
                let (reply_tx, reply_rx) = oneshot::channel();
                self.pending
                    .lock()
                    .expect("pending lock")
                    .insert(request_id.clone(), reply_tx);
                self.emit(
                    |session_id, seq| Event::ApprovalRequested {
                        session_id,
                        seq,
                        request_id: request_id.clone(),
                        tool: call.name.clone(),
                        detail: approval_detail,
                    },
                    tx,
                );
                let decision = tokio::select! {
                    reply = reply_rx => reply.unwrap_or(ApprovalDecision::Reject),
                    _ = cancel.cancelled() => {
                        self.pending.lock().expect("pending lock").remove(&request_id);
                        self.emit(|session_id, seq| Event::TurnAborted { session_id, seq }, tx);
                        return StepOutcome::Ended;
                    }
                };
                match decision {
                    ApprovalDecision::Allow => {}
                    ApprovalDecision::AlwaysAllow => {
                        self.always_allowed.insert(call.name.clone());
                    }
                    ApprovalDecision::Reject => {
                        let note = "用户拒绝了该操作。请尊重用户意愿，改用其他方式或说明理由后继续。"
                            .to_string();
                        self.history.push(ChatMsg::tool_result(&call.id, note.clone()));
                        self.record(&RolloutRecord::ToolCall {
                            tool: call.name.clone(),
                            summary,
                            arguments: call.arguments.clone(),
                            output: note.clone(),
                            is_error: true,
                        });
                        self.emit(
                            |session_id, seq| Event::ToolCallEnd {
                                session_id,
                                seq,
                                item_id,
                                output: note,
                                is_error: true,
                            },
                            tx,
                        );
                        continue;
                    }
                }
            }

            let cwd = self.cwd.clone();
            let result = {
                let ctx = ToolContext {
                    cwd: &cwd,
                    tracker: &mut self.tracker,
                };
                tokio::select! {
                    result = tool::execute(call, ctx) => Some(result),
                    _ = cancel.cancelled() => None,
                }
            };
            let Some((output, is_error, file_change)) = result else {
                self.emit(|session_id, seq| Event::TurnAborted { session_id, seq }, tx);
                return StepOutcome::Ended;
            };
            self.history.push(ChatMsg::tool_result(&call.id, output.clone()));
            self.record(&RolloutRecord::ToolCall {
                tool: call.name.clone(),
                summary,
                arguments: call.arguments.clone(),
                output: output.clone(),
                is_error,
            });
            self.emit(
                |session_id, seq| Event::ToolCallEnd {
                    session_id,
                    seq,
                    item_id,
                    output,
                    is_error,
                },
                tx,
            );
            if let Some(change) = file_change {
                self.record(&RolloutRecord::FileChange {
                    path: change.path.clone(),
                    unified_diff: change.unified_diff.clone(),
                    additions: change.additions,
                    deletions: change.deletions,
                });
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

pub const COMPACTION_MARKER: &str = "[COMPACTION]";

fn compaction_prompt(history: &[ChatMsg]) -> String {
    let mut out = format!(
        "{COMPACTION_MARKER} 请将以下编程助手对话历史压缩为摘要，必须保留：用户目标、已完成的工作、         文件变更（路径+简述）、关键决策、待办事项。用中文，分点列出，控制在 500 字以内。

"
    );
    for msg in history {
        let role = &msg.role;
        if let Some(content) = &msg.content {
            let content: String = content.chars().take(2000).collect();
            out.push_str(&format!("--- {role} ---
{content}
"));
        }
        if let Some(calls) = &msg.tool_calls {
            for call in calls {
                let args: String = call.function.arguments.chars().take(200).collect();
                out.push_str(&format!("--- {role} [tool_call {}] ---
{args}
", call.function.name));
            }
        }
    }
    out
}

fn approval_detail(call: &ToolCall, cwd: &std::path::Path) -> String {
    let args: serde_json::Value = serde_json::from_str(&call.arguments).unwrap_or_default();
    match call.name.as_str() {
        "bash" => args["command"].as_str().unwrap_or("?").to_string(),
        "write_file" => {
            let path = args["path"].as_str().unwrap_or("?");
            let content = args["content"].as_str().unwrap_or("");
            let old = std::fs::read_to_string(cwd.join(path)).unwrap_or_default();
            diff_preview(path, &old, content)
        }
        "edit" => {
            let path = args["path"].as_str().unwrap_or("?");
            let old_string = args["old_string"].as_str().unwrap_or("");
            let new_string = args["new_string"].as_str().unwrap_or("");
            let current = std::fs::read_to_string(cwd.join(path)).unwrap_or_default();
            let new = current.replacen(old_string, new_string, 1);
            diff_preview(path, &current, &new)
        }
        _ => serde_json::to_string_pretty(&args).unwrap_or_default(),
    }
}

fn diff_preview(path: &str, old: &str, new: &str) -> String {
    let diff = similar::TextDiff::from_lines(old, new);
    let unified = diff
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string();
    format!("{path}\n\n{unified}")
}

struct SessionEntry {
    session: Option<Session>,
    cancel: Option<CancellationToken>,
    model_override: Option<ModelSelection>,
    /// 回合进行中到达的消息在此排队（FIFO），回合结束自动接续
    queue: std::collections::VecDeque<(String, Vec<String>, ExecMode)>,
}

type TurnFuture = std::pin::Pin<Box<dyn Future<Output = (String, Session)>>>;

fn start_turn(
    entry: &mut SessionEntry,
    session_id: String,
    content: String,
    files: Vec<String>,
    mode: ExecMode,
    config: &ResolvedModel,
    event_tx: &async_channel::Sender<Event>,
    turns: &FuturesUnordered<TurnFuture>,
) {
    let mut session = entry.session.take().expect("session present");
    session.set_mode(mode);
    if let Some(selection) = &entry.model_override {
        session.set_model(selection.clone());
    }
    let cancel = CancellationToken::new();
    entry.cancel = Some(cancel.clone());
    let tx = event_tx.clone();
    let config = config.clone();
    turns.push(Box::pin(async move {
        session.run_turn(content, files, &config, &tx, cancel).await;
        (session_id, session)
    }));
}

/// agent manager：多会话并存，各会话独立 turn/cancel/审批表。
pub async fn agent_loop(
    op_rx: async_channel::Receiver<Op>,
    event_tx: async_channel::Sender<Event>,
    config_path: Option<PathBuf>,
    default_cwd: PathBuf,
    data_dir: PathBuf,
) {
    let config_path = config_path.unwrap_or_else(config::default_path);
    let mut load_error: Option<String> = None;
    let mut config: Option<AppConfig> = match config::load(&config_path) {
        Ok(config) => Some(config),
        Err(error) => {
            load_error = Some(error);
            None
        }
    };
    let sessions_dir = data_dir.join("sessions");
    let index = Arc::new(Mutex::new(SessionIndex::load(sessions_dir.clone())));
    let mut projects = ProjectStore::load(&data_dir);
    let pending: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));

    let mut seq = 0u64;
    macro_rules! emit_global {
        ($event:expr) => {{
            seq += 1;
            let _ = event_tx.send_blocking($event);
        }};
    }
    if let Some(error) = &load_error {
        emit_global!(Event::Error {
            session_id: None,
            seq,
            message: format!("配置解析失败: {error}。请检查或修复 ~/.pigcode/config.toml"),
        });
    }

    let mut sessions: HashMap<String, SessionEntry> = HashMap::new();
    let mut turns: FuturesUnordered<TurnFuture> = FuturesUnordered::new();
    let mut id_counter = 0u64;

    macro_rules! resolve {
        ($override:expr) => {
            config.as_ref().and_then(|c| resolve_model(c, $override))
        };
    }
    macro_rules! model_label {
        ($override:expr) => {
            resolve!($override)
                .map(|r| (r.model.clone(), r.provider_name.clone()))
                .unwrap_or_else(|| ("未配置模型".to_string(), String::new()))
        };
    }

    loop {
        tokio::select! {
            op = op_rx.recv() => {
                let Ok(op) = op else { break };
                match op {
                    Op::NewSession { cwd } => {
                        id_counter += 1;
                        let id = format!("s{}-{}", now_secs(), id_counter);
                        let meta = SessionMeta {
                            id: id.clone(),
                            title: "新任务".to_string(),
                            cwd,
                            created_at: now_secs(),
                            updated_at: now_secs(),
                            pinned: false,
                            archived: false,
                        };
                        match Session::create(meta.clone(), pending.clone(), index.clone(), &sessions_dir, data_dir.clone()) {
                            Ok(session) => {
                                sessions.insert(id.clone(), SessionEntry { session: Some(session), cancel: None, model_override: None, queue: Default::default() });
                                index.lock().expect("index lock").upsert(meta.clone());
                                let (model, provider_name) = model_label!(None);
                                emit_global!(Event::SessionConfigured {
                                    session_id: id.clone(),
                                    cwd: meta.cwd.clone(),
                                    model,
                                    provider_name,
                                });
                                emit_global!(Event::SessionList {
                                    sessions: index.lock().expect("index lock").sorted(),
                                });
                            }
                            Err(error) => emit_global!(Event::Error {
                                session_id: None,
                                seq,
                                message: error,
                            }),
                        }
                    }
                    Op::OpenSession { session_id } => {
                        if sessions.contains_key(&session_id) {
                            let meta = index.lock().expect("index lock")
                                .entries.iter().find(|e| e.id == session_id).cloned();
                            if let Some(meta) = meta {
                                let (model, provider_name) = model_label!(None);
                                emit_global!(Event::SessionConfigured {
                                    session_id,
                                    cwd: meta.cwd,
                                    model,
                                    provider_name,
                                });
                            }
                            continue;
                        }
                        match Session::load(&session_id, &sessions_dir, pending.clone(), index.clone(), data_dir.clone()) {
                            Ok((session, records)) => {
                                let cwd = session.cwd.clone();
                                sessions.insert(session_id.clone(), SessionEntry {
                                    session: Some(session),
                                    cancel: None,
                                    model_override: None,
                                    queue: Default::default(),
                                });
                                let (model, provider_name) = model_label!(None);
                                emit_global!(Event::SessionConfigured {
                                    session_id: session_id.clone(),
                                    cwd,
                                    model,
                                    provider_name,
                                });
                                if let Some(entry) = sessions.get_mut(&session_id) {
                                    if let Some(session) = entry.session.as_mut() {
                                        session.replay(&records, &event_tx);
                                    }
                                }
                            }
                            Err(error) => emit_global!(Event::Error {
                                session_id: None,
                                seq,
                                message: error,
                            }),
                        }
                    }
                    Op::ListProjects => {
                        emit_global!(Event::ProjectList {
                            projects: projects.projects.clone(),
                        });
                    }
                    Op::AddProject { path } => {
                        projects.add(path);
                        emit_global!(Event::ProjectList {
                            projects: projects.projects.clone(),
                        });
                    }
                    Op::RemoveProject { path } => {
                        projects.remove(&path);
                        emit_global!(Event::ProjectList {
                            projects: projects.projects.clone(),
                        });
                    }
                    Op::ListSessions => {
                        emit_global!(Event::SessionList {
                            sessions: index.lock().expect("index lock").sorted(),
                        });
                    }
                    Op::UpdateSessionMeta { session_id, pinned, archived, title } => {
                        index.lock().expect("index lock").update(&session_id, |meta| {
                            if let Some(pinned) = pinned { meta.pinned = pinned; }
                            if let Some(archived) = archived { meta.archived = archived; }
                            if let Some(title) = &title { meta.title = title.clone(); }
                            meta.updated_at = now_secs();
                        });
                        emit_global!(Event::SessionList {
                            sessions: index.lock().expect("index lock").sorted(),
                        });
                    }
                    Op::SendMessage { session_id, content, files, mode } => {
                        let Some(entry) = sessions.get_mut(&session_id) else {
                            emit_global!(Event::Error {
                                session_id: Some(session_id),
                                seq,
                                message: "会话不存在，请先新建或打开".to_string(),
                            });
                            continue;
                        };
                        // 回合进行中 → 排队，回合结束自动接续（Interrupt 不清队列）
                        if entry.session.is_none() {
                            entry.queue.push_back((content.clone(), files, mode));
                            emit_global!(Event::MessageQueued {
                                session_id: session_id.clone(),
                                seq,
                                text: content,
                            });
                            continue;
                        }
                        let Some(resolved) = resolve!(entry.model_override.as_ref()) else {
                            emit_global!(Event::Error {
                                session_id: Some(session_id),
                                seq,
                                message: "未配置模型，请在 设置 → 模型设置 中添加供应商和模型".to_string(),
                            });
                            continue;
                        };
                        start_turn(entry, session_id, content, files, mode, &resolved, &event_tx, &turns);
                    }
                    Op::CancelQueued { session_id, text } => {
                        if let Some(entry) = sessions.get_mut(&session_id) {
                            if let Some(pos) = entry.queue.iter().position(|(t, _, _)| t == &text) {
                                entry.queue.remove(pos);
                            }
                        }
                    }
                    Op::GitInfo { cwd } => {
                        let tx = event_tx.clone();
                        tokio::spawn(async move {
                            let cwd2 = cwd.clone();
                            let result = tokio::task::spawn_blocking(move || crate::git::git_info(&cwd2)).await;
                            if let Ok((current_branch, branches)) = result {
                                let _ = tx.send(Event::GitInfo {
                                    cwd,
                                    current_branch,
                                    branches,
                                }).await;
                            }
                        });
                    }
                    Op::CheckoutBranch { cwd, branch } => {
                        let tx = event_tx.clone();
                        tokio::spawn(async move {
                            let cwd2 = cwd.clone();
                            let branch2 = branch.clone();
                            let result = tokio::task::spawn_blocking(move || {
                                crate::git::checkout(&cwd2, &branch2)
                            })
                            .await;
                            match result {
                                Ok(Ok(())) => {
                                    let _ = tx.send(Event::BranchChanged { cwd, branch }).await;
                                }
                                Ok(Err(error)) => {
                                    let _ = tx.send(Event::Error {
                                        session_id: None,
                                        seq: 0,
                                        message: error,
                                    }).await;
                                }
                                Err(e) => {
                                    let _ = tx.send(Event::Error {
                                        session_id: None,
                                        seq: 0,
                                        message: format!("git 任务失败: {e}"),
                                    }).await;
                                }
                            }
                        });
                    }
                    Op::Interrupt { session_id } => {
                        if let Some(entry) = sessions.get(&session_id) {
                            if let Some(cancel) = &entry.cancel {
                                cancel.cancel();
                            }
                        }
                    }
                    Op::ApprovalReply { request_id, decision } => {
                        if let Some(reply) = pending.lock().expect("pending lock").remove(&request_id) {
                            let _ = reply.send(decision);
                        }
                    }
                    Op::SetExecMode { session_id, mode } => {
                        if let Some(entry) = sessions.get_mut(&session_id) {
                            if let Some(session) = entry.session.as_mut() {
                                session.set_mode(mode);
                            }
                        }
                    }
                    Op::RevertFile { session_id, path } => {
                        match sessions.get_mut(&session_id) {
                            Some(entry) if entry.session.is_some() => {
                                entry.session.as_mut().expect("session").revert_file(&path, &event_tx);
                            }
                            Some(_) => emit_global!(Event::Error {
                                session_id: Some(session_id),
                                seq,
                                message: "回合进行中，无法撤销文件".to_string(),
                            }),
                            None => emit_global!(Event::Error {
                                session_id: Some(session_id),
                                seq,
                                message: "会话不存在".to_string(),
                            }),
                        }
                    }
                    Op::SearchFiles { session_id, query } => {
                        let cwd = sessions.get(&session_id)
                            .and_then(|e| e.session.as_ref().map(|s| s.cwd.clone()))
                            .unwrap_or_else(|| default_cwd.clone());
                        let tx = event_tx.clone();
                        let query_for_search = query.clone();
                        tokio::spawn(async move {
                            let results = tokio::task::spawn_blocking(move || {
                                tool::search_files(&cwd, &query_for_search, 20)
                            })
                            .await
                            .unwrap_or_default();
                            let _ = tx.send(Event::FileSearchResults {
                                session_id,
                                query,
                                results,
                            }).await;
                        });
                    }
                    Op::Compact { session_id } => {
                        match sessions.get_mut(&session_id) {
                            Some(entry) if entry.session.is_some() => {
                                let mut session = entry.session.take().expect("session present");
                                let tx = event_tx.clone();
                                let sid = session_id.clone();
                                let resolved = resolve!(entry.model_override.as_ref());
                                entry.cancel = Some(CancellationToken::new());
                                let cancel = entry.cancel.clone().expect("cancel");
                                turns.push(Box::pin(async move {
                                    session.run_compact(resolved.as_ref(), false, &tx, &cancel).await;
                                    (sid, session)
                                }));
                            }
                            _ => emit_global!(Event::Error {
                                session_id: Some(session_id),
                                seq,
                                message: "回合进行中或会话不存在，无法压缩".to_string(),
                            }),
                        }
                    }
                    Op::SetModel { session_id, provider_id, model_id, reasoning_level } => {
                        if let Some(entry) = sessions.get_mut(&session_id) {
                            entry.model_override = Some(ModelSelection {
                                provider_id,
                                model_id,
                                reasoning_level,
                            });
                        }
                    }
                    Op::GetConfig => {
                        emit_global!(Event::ConfigSnapshot {
                            config: config.clone().unwrap_or_default(),
                        });
                    }
                    Op::SaveConfig { config: new_config } => {
                        if let Err(error) = config::save(&config_path, &new_config) {
                            emit_global!(Event::Error {
                                session_id: None,
                                seq,
                                message: error,
                            });
                        } else {
                            config = Some(new_config);
                        }
                        emit_global!(Event::ConfigSnapshot {
                            config: config.clone().unwrap_or_default(),
                        });
                    }
                    Op::TestProvider { provider_id } => {
                        let found = config.as_ref().and_then(|c| {
                            c.providers.iter().find(|p| p.id == provider_id).cloned()
                        });
                        let tx = event_tx.clone();
                        tokio::spawn(async move {
                            let result = match found {
                                Some(provider) => {
                                    let model = provider
                                        .models
                                        .first()
                                        .map(|m| m.id.clone())
                                        .unwrap_or_else(|| "ping".to_string());
                                    provider::test_provider(
                                        &provider.base_url,
                                        &config::expand_env(&provider.api_key),
                                        provider.api_format,
                                        &model,
                                    )
                                    .await
                                }
                                None => Err("供应商不存在".to_string()),
                            };
                            let (ok, message) = match result {
                                Ok(message) => (true, message),
                                Err(message) => (false, message),
                            };
                            let _ = tx.send(Event::TestResult {
                                provider_id,
                                ok,
                                message,
                            }).await;
                        });
                    }
                    Op::Shutdown => break,
                }
            }
            Some((session_id, session)) = turns.next(), if !turns.is_empty() => {
                if let Some(entry) = sessions.get_mut(&session_id) {
                    entry.session = Some(session);
                    entry.cancel = None;
                    // 回合结束（含中止/出错）后自动取出队首继续
                    if let Some((content, files, mode)) = entry.queue.pop_front() {
                        if let Some(resolved) = resolve!(entry.model_override.as_ref()) {
                            start_turn(entry, session_id.clone(), content, files, mode, &resolved, &event_tx, &turns);
                        }
                    }
                }
            }
        }
    }
}
