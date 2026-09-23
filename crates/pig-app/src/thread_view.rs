use std::collections::HashMap;

use gpui_kit::assets::IconName as AssetIconName;
use gpui_kit::base::{Scrollbar, SelectableText, TextSelectionHandle};
use gpui_kit::component::Sizable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::shimmer::ShimmerText;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::text::{TextView, TextViewState};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use pig_protocol::{ApprovalDecision, EditDiff, Event};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    System,
}

pub enum Segment {
    Thinking {
        text: String,
        open: bool,
        /// 用户手动展开/收过后为 true，自动折叠不再覆盖
        pinned: bool,
        /// 秒表起点（首个 delta 到达时刻）
        started: std::time::Instant,
        /// 思考结束定格的用时；回放重建的历史段没有真实时钟，保持 None（显示「持续了几秒」）
        duration: Option<std::time::Duration>,
        /// 展开正文的滚动句柄（track_scroll 持久滚动位置）
        body_scroll: ScrollHandle,
    },
    Markdown {
        state: Entity<TextViewState>,
        text: String,
    },
    ToolCall {
        tool: String,
        summary: String,
        output: String,
        is_error: bool,
        done: bool,
        expanded: bool,
        /// 写/改类工具的本次编辑 diff（内联 diff 卡片）
        edit: Option<EditDiff>,
        /// 展开正文的滚动句柄（track_scroll 持久滚动位置）
        body_scroll: ScrollHandle,
    },
    /// 一轮结束时的本轮文件改动面板（ZCode turn 头部文件更改同款）
    TurnChanges { rows: Vec<TurnFileRow>, open: bool },
    Approval {
        request_id: String,
        decision: Option<ApprovalDecision>,
    },
}

/// 每轮改动面板里的单文件行
pub struct TurnFileRow {
    edit: EditDiff,
    expanded: bool,
    /// 内联 diff 卡的滚动句柄
    scroll: ScrollHandle,
}

pub struct ChatMessage {
    pub role: Role,
    pub text: String,
    /// 用户消息的选择 handle + 刷新订阅（拖动选择时驱动实时高亮），仅 User 角色有
    pub selection: Option<(TextSelectionHandle, Subscription)>,
    pub files: Vec<String>,
    pub segments: Vec<Segment>,
    pub footer: Option<String>,
}

impl ChatMessage {
    fn user(text: String, files: Vec<String>) -> Self {
        Self {
            role: Role::User,
            text,
            selection: None,
            files,
            segments: vec![],
            footer: None,
        }
    }

    fn system(text: String) -> Self {
        Self {
            role: Role::System,
            text,
            selection: None,
            files: vec![],
            segments: vec![],
            footer: None,
        }
    }

    fn assistant() -> Self {
        Self {
            role: Role::Assistant,
            text: String::new(),
            selection: None,
            files: vec![],
            segments: vec![],
            footer: None,
        }
    }
}

#[derive(Clone)]
pub enum ThreadEvent {
    /// 计划模式：用户点了「执行计划」
    ExecutePlan,
    /// 取消排队消息（文本匹配）
    CancelQueued(String),
    ApprovalReply {
        request_id: String,
        decision: ApprovalDecision,
    },
}

pub struct ThreadView {
    messages: Vec<ChatMessage>,
    item_index: HashMap<String, usize>,
    scroll_handle: ScrollHandle,
    /// 跟随模式：输出时自动贴底。用户上翻暂停跟随（浮出「最新消息」按钮），
    /// 回到底部（任意方式）或点击浮钮后恢复
    follow_bottom: bool,
    streaming: bool,
    context_usage: Option<(u64, u64)>,
    /// 计划模式回合完成，等待用户确认执行
    plan_pending: bool,
    turn_started: Option<std::time::Instant>,
    /// 当前回合由回放重建（turn_id 以 replay- 开头）：思考段不打真实用时
    replay_turn: bool,
    /// 排队中的消息（FIFO）
    queued: Vec<String>,
    _ticker: Task<()>,
}

impl EventEmitter<ThreadEvent> for ThreadView {}

impl ThreadView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        // 每秒 tick：驱动"工作中 N 秒"计时刷新
        let ticker = cx.spawn(async move |this: WeakEntity<ThreadView>, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(1))
                    .await;
                let alive = this
                    .update(cx, |this, cx| {
                        if this.streaming {
                            cx.notify();
                        }
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        });
        Self {
            messages: vec![],
            item_index: HashMap::new(),
            scroll_handle: ScrollHandle::new(),
            follow_bottom: true,
            streaming: false,
            context_usage: None,
            plan_pending: false,
            turn_started: None,
            replay_turn: false,
            queued: Vec::new(),
            _ticker: ticker,
        }
    }

    fn set_streaming(&mut self, streaming: bool, _cx: &mut Context<Self>) {
        self.streaming = streaming;
        self.turn_started = streaming.then(std::time::Instant::now);
    }

    /// 当前是否已在底部（offset.y ∈ [-max.y, 0]，距底 = offset.y + max.y）
    fn at_bottom(&self) -> bool {
        self.scroll_handle.offset().y + self.scroll_handle.max_offset().y <= px(2.)
    }

    /// 输出期自动滚动：仅跟随模式贴底；用户上翻后不打扰
    fn auto_scroll(&mut self) {
        if self.follow_bottom {
            self.scroll_handle.scroll_to_bottom();
        }
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// 自测用。
    pub fn debug_queued(&self) -> &[String] {
        &self.queued
    }

    pub fn is_streaming(&self) -> bool {
        self.streaming
    }

    pub fn set_plan_pending(&mut self, pending: bool, cx: &mut Context<Self>) {
        self.plan_pending = pending;
        cx.notify();
    }

    pub fn is_plan_pending(&self) -> bool {
        self.plan_pending
    }

    /// 与点击「执行计划」按钮相同的路径（自测用）。
    pub fn trigger_execute_plan(&mut self, cx: &mut Context<Self>) {
        self.plan_pending = false;
        cx.emit(ThreadEvent::ExecutePlan);
        cx.notify();
    }

    #[allow(dead_code)] // 调试用
    pub fn message_count(&self) -> usize {
        self.messages.len()
    }

    pub fn append_user_message(
        &mut self,
        text: String,
        files: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        self.messages.push(ChatMessage::user(text, files));
        // 用户自己发消息：强制回到底部并恢复跟随
        self.follow_bottom = true;
        self.auto_scroll();
        cx.notify();
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.messages.clear();
        self.item_index.clear();
        self.follow_bottom = true;
        cx.notify();
    }

    pub fn add_system_note(&mut self, text: &str, cx: &mut Context<Self>) {
        self.messages.push(ChatMessage::system(text.to_string()));
        self.auto_scroll();
        cx.notify();
    }

    /// 供自测断言用：所有系统提示条文本。
    pub fn debug_system_notes(&self) -> Vec<String> {
        self.messages
            .iter()
            .filter(|m| m.role == Role::System)
            .map(|m| m.text.clone())
            .collect()
    }

    /// 供自测断言用。
    pub fn debug_last_assistant(&self) -> (bool, String, String, String) {
        let Some(message) = self
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant)
        else {
            return (false, String::new(), String::new(), String::new());
        };
        let mut tool_done = false;
        let mut text = String::new();
        let mut thinking = String::new();
        let mut tool_output = String::new();
        for segment in &message.segments {
            match segment {
                Segment::Thinking { text: t, .. } => thinking.push_str(t),
                Segment::Markdown { text: t, .. } => text.push_str(t),
                Segment::ToolCall {
                    done,
                    output,
                    is_error,
                    ..
                } => {
                    tool_done |= *done && !*is_error;
                    tool_output.push_str(output);
                }
                Segment::TurnChanges { .. } | Segment::Approval { .. } => {}
            }
        }
        (tool_done, text, thinking, tool_output)
    }

    /// 任意消息中是否出现过某工具的工具卡（自测用）。
    pub fn debug_has_tool_call(&self, tool: &str) -> bool {
        self.messages.iter().any(|m| {
            m.segments.iter().any(|s| match s {
                Segment::ToolCall { tool: name, .. } => name == tool,
                _ => false,
            })
        })
    }

    /// 当前待审批的 request_id（自测用）。
    pub fn pending_approval(&self) -> Option<String> {
        self.messages.iter().rev().find_map(|m| {
            m.segments.iter().find_map(|s| match s {
                Segment::Approval {
                    request_id,
                    decision: None,
                    ..
                } => Some(request_id.clone()),
                _ => None,
            })
        })
    }

    /// 走与点击按钮相同的路径对待决议的审批卡做出决定（自测用）。
    pub fn decide_pending(&mut self, decision: ApprovalDecision, cx: &mut Context<Self>) -> bool {
        let found = self.messages.iter().enumerate().rev().find_map(|(mix, m)| {
            m.segments.iter().enumerate().find_map(|(six, s)| match s {
                Segment::Approval { decision: None, .. } => Some((mix, six)),
                _ => None,
            })
        });
        let Some((mix, six)) = found else {
            return false;
        };
        self.decide_approval(mix, six, decision, cx);
        true
    }

    fn decide_approval(
        &mut self,
        message_ix: usize,
        segment_ix: usize,
        decision: ApprovalDecision,
        cx: &mut Context<Self>,
    ) {
        if let Some(Segment::Approval {
            request_id,
            decision: slot,
            ..
        }) = self
            .messages
            .get_mut(message_ix)
            .and_then(|m| m.segments.get_mut(segment_ix))
        {
            *slot = Some(decision);
            cx.emit(ThreadEvent::ApprovalReply {
                request_id: request_id.clone(),
                decision,
            });
        }
        cx.notify();
    }

    pub fn reduce_event(&mut self, event: Event, cx: &mut Context<Self>) {
        match event {
            Event::SessionConfigured { .. } => {}
            Event::TurnStarted { turn_id, .. } => {
                self.plan_pending = false;
                self.replay_turn = turn_id.starts_with("replay-");
                self.messages.push(ChatMessage::assistant());
                self.item_index.clear();
                self.set_streaming(true, cx);
            }
            Event::ReasoningDelta { item_id, delta, .. } => {
                if !self.item_index.contains_key(&item_id) {
                    // 新一段思考开始 = 上一段思考结束
                    self.finish_thinking();
                }
                let six = self.find_or_create(&item_id, || Segment::Thinking {
                    text: String::new(),
                    open: false,
                    pinned: false,
                    started: std::time::Instant::now(),
                    duration: None,
                    body_scroll: ScrollHandle::new(),
                });
                if let Some(Segment::Thinking { text, .. }) = self.current_segment(six) {
                    text.push_str(&delta);
                }
                self.auto_scroll();
            }
            Event::TextDelta { item_id, delta, .. } => {
                self.finish_thinking();
                let state_holder = cx.new(|cx| TextViewState::markdown("", cx));
                let six = self.find_or_create(&item_id, || Segment::Markdown {
                    state: state_holder,
                    text: String::new(),
                });
                if let Some(Segment::Markdown { state, text }) = self.current_segment(six) {
                    text.push_str(&delta);
                    let delta = delta.clone();
                    state.update(cx, |state, cx| state.push_str(&delta, cx));
                }
                self.auto_scroll();
            }
            Event::TextDone {
                item_id, full_text, ..
            } => {
                self.finish_thinking();
                let state_holder = cx.new(|cx| TextViewState::markdown("", cx));
                let six = self.find_or_create(&item_id, || Segment::Markdown {
                    state: state_holder,
                    text: String::new(),
                });
                if let Some(Segment::Markdown { state, text }) = self.current_segment(six) {
                    *text = full_text.clone();
                    state.update(cx, |state, cx| state.set_text(&full_text, cx));
                }
            }
            Event::ToolCallBegin {
                item_id,
                tool,
                input_summary,
                ..
            } => {
                self.finish_thinking();
                let six = self.find_or_create(&item_id, || Segment::ToolCall {
                    tool: tool.clone(),
                    summary: input_summary.clone(),
                    output: String::new(),
                    is_error: false,
                    done: false,
                    expanded: false,
                    edit: None,
                    body_scroll: ScrollHandle::new(),
                });
                if let Some(Segment::ToolCall { tool, summary, .. }) = self.current_segment(six) {
                    *tool = tool.clone();
                    *summary = input_summary;
                }
                self.auto_scroll();
            }
            Event::ToolCallEnd {
                item_id,
                output,
                is_error,
                edit,
                ..
            } => {
                let six = self.find_or_create(&item_id, || Segment::ToolCall {
                    tool: String::new(),
                    summary: String::new(),
                    output: String::new(),
                    is_error: false,
                    done: false,
                    expanded: false,
                    edit: None,
                    body_scroll: ScrollHandle::new(),
                });
                if let Some(Segment::ToolCall {
                    output: out,
                    is_error: err,
                    done,
                    expanded,
                    edit: slot,
                    ..
                }) = self.current_segment(six)
                {
                    *out = output;
                    *err = is_error;
                    *done = true;
                    *slot = edit;
                    // 失败的调用直接展开输出，省去用户多点一下
                    if is_error {
                        *expanded = true;
                    }
                }
                self.auto_scroll();
            }
            Event::ApprovalRequested { request_id, .. } => {
                self.finish_thinking();
                if self
                    .messages
                    .last()
                    .is_none_or(|m| m.role != Role::Assistant)
                {
                    self.messages.push(ChatMessage::assistant());
                }
                self.messages
                    .last_mut()
                    .expect("assistant message")
                    .segments
                    .push(Segment::Approval {
                        request_id,
                        decision: None,
                    });
                self.auto_scroll();
            }
            Event::ContextUsage { used, total, .. } => {
                self.context_usage = Some((used, total));
            }
            Event::TurnComplete { duration_ms, .. } => {
                self.finish_thinking();
                self.replay_turn = false;
                if let Some(message) = self.messages.last_mut() {
                    for segment in &mut message.segments {
                        if let Segment::Thinking { open, pinned, .. } = segment {
                            if !*pinned {
                                *open = false;
                            }
                        }
                    }
                }
                // duration_ms=0 是会话回放的收尾事件：只退出流式状态，不写用时脚注
                if duration_ms > 0 {
                    let usage = self
                        .context_usage
                        .map(|(used, total)| {
                            format!(" · 上下文 {:.1}k / {}k", used as f64 / 1000.0, total / 1000)
                        })
                        .unwrap_or_default();
                    if let Some(message) = self.messages.last_mut() {
                        message.footer = Some(format!(
                            "回合结束 · 用时 {:.1}s{usage}",
                            duration_ms as f64 / 1000.0
                        ));
                    }
                }
                self.set_streaming(false, cx);
            }
            Event::TurnAborted { .. } => {
                self.finish_thinking();
                self.replay_turn = false;
                if let Some(message) = self.messages.last_mut() {
                    message.footer = Some("已停止".to_string());
                }
                self.set_streaming(false, cx);
            }
            Event::TurnFileChanges { files, .. } => {
                self.finish_thinking();
                if !files.is_empty() {
                    if self
                        .messages
                        .last()
                        .is_none_or(|m| m.role != Role::Assistant)
                    {
                        self.messages.push(ChatMessage::assistant());
                    }
                    self.messages
                        .last_mut()
                        .expect("assistant message")
                        .segments
                        .push(Segment::TurnChanges {
                            rows: files
                                .into_iter()
                                .map(|edit| TurnFileRow {
                                    edit,
                                    expanded: false,
                                    scroll: ScrollHandle::new(),
                                })
                                .collect(),
                            open: false,
                        });
                }
                self.auto_scroll();
            }
            Event::FileChanged { .. } | Event::FileReverted { .. } => {}
            Event::UserMessage { text, files, .. } => {
                let trimmed = text.trim().to_string();
                self.append_user_message(text, files, cx);
                if let Some(pos) = self
                    .queued
                    .iter()
                    .position(|t| trimmed == *t || trimmed.starts_with(t.as_str()))
                {
                    self.queued.remove(pos);
                }
            }
            Event::MessageQueued { text, .. } => {
                self.queued.push(text);
            }
            // 这些事件由 AppView::route_event 拦截处理，不到这里
            Event::SessionList { .. }
            | Event::FileSearchResults { .. }
            | Event::ContextCompacted { .. }
            | Event::TodoListChanged { .. }
            | Event::TaskListChanged { .. }
            | Event::QuestionRequested { .. }
            | Event::GitInfo { .. }
            | Event::BranchChanged { .. }
            | Event::GitStatus { .. }
            | Event::GitDiff { .. }
            | Event::ConfigSnapshot { .. }
            | Event::TestResult { .. }
            | Event::WorkspaceList { .. } => {}
            Event::Error { message, .. } => {
                self.finish_thinking();
                self.replay_turn = false;
                self.set_streaming(false, cx);
                self.messages
                    .push(ChatMessage::system(format!("⚠ {message}")));
            }
        }
        cx.notify();
    }

    /// 收尾当前消息里还在计时的思考段，定格用时。
    /// 回放重建的回合没有真实时钟（事件在一瞬间到达），保持 None 显示「持续了几秒」。
    fn finish_thinking(&mut self) {
        if self.replay_turn {
            return;
        }
        let Some(message) = self.messages.last_mut() else {
            return;
        };
        for segment in &mut message.segments {
            if let Segment::Thinking {
                started, duration, ..
            } = segment
            {
                duration.get_or_insert_with(|| started.elapsed());
            }
        }
    }

    /// 在当前助手消息里按 item_id 找 segment，找不到则用 `create` 追加。
    fn find_or_create(&mut self, item_id: &str, create: impl FnOnce() -> Segment) -> usize {
        if let Some(&six) = self.item_index.get(item_id) {
            return six;
        }
        if self
            .messages
            .last()
            .is_none_or(|m| m.role != Role::Assistant)
        {
            self.messages.push(ChatMessage::assistant());
        }
        let message = self.messages.last_mut().expect("assistant message");
        message.segments.push(create());
        let six = message.segments.len() - 1;
        self.item_index.insert(item_id.to_string(), six);
        six
    }

    fn current_segment(&mut self, six: usize) -> Option<&mut Segment> {
        self.messages.last_mut()?.segments.get_mut(six)
    }

    fn render_user_message(
        &self,
        ix: usize,
        message: &ChatMessage,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        v_flex()
            .w_full()
            .items_end()
            .gap_1()
            .when(!message.files.is_empty(), |this| {
                this.child(h_flex().gap_1().children(message.files.iter().map(|file| {
                    h_flex()
                        .gap_1()
                        .px_2()
                        .py_0p5()
                        .rounded(cx.theme().radius)
                        .bg(cx.theme().accent)
                        .child(
                            Icon::new(IconName::FileText)
                                .size_3()
                                .text_color(cx.theme().muted_foreground),
                        )
                        .child(div().text_xs().child(file.clone()))
                })))
            })
            .child(
                div()
                    .max_w(relative(0.8))
                    .px_3()
                    .py_2()
                    .rounded_2xl()
                    .bg(cx.theme().accent)
                    .text_sm()
                    .with_animation(
                        "user-msg-enter",
                        Animation::new(std::time::Duration::from_millis(150))
                            .with_easing(ease_out_quint()),
                        |el, delta| el.top(px(4.0 * (1.0 - delta))).opacity(delta),
                    )
                    // 纯文本原文渲染 + 窗口级选择（拖拽/双击选词/Ctrl+C 复制）；
                    // 显式 handle + refresh_window_on_change 让拖动过程实时高亮
                    .child(
                        SelectableText::with_handle(
                            ("user-msg-text", ix),
                            message
                                .selection
                                .as_ref()
                                .expect("render 时已惰性创建选择 handle")
                                .0
                                .clone(),
                            message.text.clone(),
                        )
                        .document_order(ix as u64),
                    ),
            )
            .into_any_element()
    }

    /// 思考折叠块（ZCode 同款）：无边框的一行 header（大脑图标 + 文案），箭头悬停/
    /// 展开时才显示；展开后正文以左侧竖线缩进展示，超高内部滚动。
    #[allow(clippy::too_many_arguments)]
    fn render_thinking(
        &self,
        message_ix: usize,
        segment_ix: usize,
        text: &str,
        open: bool,
        started: std::time::Instant,
        duration: Option<std::time::Duration>,
        body_scroll: &ScrollHandle,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let secs = |d: std::time::Duration| (d.as_secs_f64().ceil() as u64).max(1);
        let in_progress = duration.is_none() && self.streaming && !self.replay_turn;
        let label = match duration {
            Some(d) => format!("思考 · 持续了 {} 秒", secs(d)),
            // 思考仍在进行：流式中且不是回放（回放的 TurnComplete 前 streaming 也为 true）
            None if self.streaming && !self.replay_turn => {
                format!("正在思考 · {} 秒", secs(started.elapsed()))
            }
            // 回放重建的历史段没有真实时钟
            None => "思考 · 持续了几秒".to_string(),
        };
        let muted = cx.theme().muted_foreground;
        let subtlest = muted.opacity(0.6);
        let group_id = format!("thinking-row-{message_ix}-{segment_ix}");
        v_flex()
            .w_full()
            .child(
                h_flex()
                    .id(("thinking", message_ix * 1024 + segment_ix))
                    .group(group_id.clone())
                    .w_full()
                    .gap_2()
                    .py_1()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(Segment::Thinking { open, pinned, .. }) = this
                            .messages
                            .get_mut(message_ix)
                            .and_then(|m| m.segments.get_mut(segment_ix))
                        {
                            *open = !*open;
                            *pinned = true;
                        }
                        cx.notify();
                    }))
                    .child(
                        Icon::new(AssetIconName::Brain)
                            .size_4()
                            .text_color(subtlest),
                    )
                    // 思考进行中：shimmer 扫过高亮；id 必须稳定（文案每秒变，默认动画
                    // id 取文案会导致扫光每秒重启）
                    .child(if in_progress {
                        ShimmerText::new(label)
                            .id(("thinking-shimmer", message_ix * 1024 + segment_ix))
                            .text_sm()
                            .text_color(subtlest)
                            .into_any_element()
                    } else {
                        div()
                            .text_sm()
                            .text_color(subtlest)
                            .child(label)
                            .into_any_element()
                    })
                    // 箭头默认隐藏，行悬停或展开时显示
                    .child(
                        div()
                            .invisible()
                            .group_hover(group_id, |this| this.visible())
                            .when(open, |this| this.visible())
                            .child(
                                Icon::new(if open {
                                    IconName::ChevronDown
                                } else {
                                    IconName::ChevronRight
                                })
                                .size_4()
                                .text_color(subtlest),
                            ),
                    ),
            )
            .when(open, |this| {
                this.child(
                    // 包装层携带滚动链处理：正文能滚时吞掉滚轮，避免外层消息列表联动
                    div()
                        .relative()
                        .on_scroll_wheel(consume_scroll(body_scroll))
                        .child(
                            div()
                                .id(("thinking-body", message_ix * 1024 + segment_ix))
                                .mt_1()
                                .ml(px(8.))
                                .border_l_1()
                                .border_color(cx.theme().border)
                                .pl(px(14.))
                                .max_h(px(240.))
                                .overflow_y_scroll()
                                .track_scroll(body_scroll)
                                .text_sm()
                                .text_color(subtlest)
                                .child(text.to_string()),
                        ),
                )
            })
            .into_any_element()
    }

    /// 工具调用（ZCode 同款）：无边框摘要行（图标 + 中文工具名 + 单行摘要 + 状态词），
    /// 箭头仅悬停/展开时显示；展开后是圆角描边卡片：完整输入（终端类带 `$` 前缀）+
    /// 等宽输出，输出限高内部滚动。运行中不用 spinner，工具名扫光（ZCode 的取舍：
    /// 流式期间工具多，持续动画耗渲染资源）。
    /// `approval_pending`：该工具正在等待批准（行尾显示黄色「等待批准」）。
    #[allow(clippy::too_many_arguments)]
    fn render_tool_card(
        &self,
        message_ix: usize,
        segment_ix: usize,
        tool: &str,
        summary: &str,
        output: &str,
        is_error: bool,
        done: bool,
        expanded: bool,
        approval_pending: bool,
        edit: Option<&EditDiff>,
        body_scroll: &ScrollHandle,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // ZCode 三级文字层级：正文 > subtle(60%) > subtlest(30~40%)，靠层级而非边框/色彩造信息密度
        let subtle = cx.theme().muted_foreground;
        let subtlest = subtle.opacity(0.6);
        let group_id = format!("tool-row-{message_ix}-{segment_ix}");
        let running = !done && !approval_pending;
        let tool_icon = match tool {
            "Bash" => AssetIconName::Terminal,
            "Read" => AssetIconName::Eye,
            "Write" => AssetIconName::FilePlus,
            "Edit" => AssetIconName::FilePen,
            "Glob" => AssetIconName::FolderSearch,
            "Grep" => AssetIconName::TextSearch,
            "TodoList" => AssetIconName::ListTodo,
            "FetchURL" => AssetIconName::Globe,
            "AskUserQuestion" => AssetIconName::MessageCircleQuestionMark,
            _ => AssetIconName::Wrench,
        };
        let kind_label = match tool {
            "Bash" => "终端",
            "Read" => "读取",
            "Write" => "写入",
            "Edit" => "编辑",
            "Glob" => "查找文件",
            "Grep" => "搜索",
            "TodoList" => "待办",
            "FetchURL" => "抓取网页",
            "TaskList" | "TaskOutput" | "TaskStop" => "后台任务",
            "AskUserQuestion" => "提问",
            _ => tool,
        };
        // 摘要压成单行：多行命令的换行折叠为空格（否则折叠行会被撑成多行）
        let summary_line = summary.split_whitespace().collect::<Vec<_>>().join(" ");

        v_flex()
            .w_full()
            .child(
                h_flex()
                    .id(("tool", message_ix * 1024 + segment_ix))
                    .group(group_id.clone())
                    .w_full()
                    .gap_2()
                    .py_1()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(Segment::ToolCall { expanded, .. }) = this
                            .messages
                            .get_mut(message_ix)
                            .and_then(|m| m.segments.get_mut(segment_ix))
                        {
                            *expanded = !*expanded;
                        }
                        cx.notify();
                    }))
                    .child(Icon::new(tool_icon).size_4().text_color(subtlest))
                    // 工具运行中：工具名 shimmer 扫光（等批准/已结束回静态文本）
                    .child(if running {
                        ShimmerText::new(kind_label)
                            .id(("tool-label-shimmer", message_ix * 1024 + segment_ix))
                            .text_sm()
                            .text_color(subtlest)
                            .into_any_element()
                    } else {
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(subtlest)
                            .child(kind_label.to_string())
                            .into_any_element()
                    })
                    // 成功只在工具名后给一枚小勾，失败在右侧给状态词（ZCode 同款）
                    .when(done && !is_error, |this| {
                        this.child(
                            Icon::new(IconName::Check)
                                .size_3()
                                .text_color(cx.theme().success),
                        )
                    })
                    // 编辑类：文件名（亮一档）+ 目录路径（最暗，优先截断）；其余工具单行摘要
                    // 摘要只占内容宽（过长时收缩截断），让统计/箭头跟在文字后面而非靠右
                    .child(if edit.is_some() {
                        let (dir, name) = split_path(summary);
                        h_flex()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .gap_1()
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_sm()
                                    .text_color(subtle)
                                    .child(name),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .text_sm()
                                    .text_color(subtlest)
                                    .child(dir),
                            )
                            .into_any_element()
                    } else {
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_sm()
                            .text_color(subtle)
                            .child(summary_line)
                            .into_any_element()
                    })
                    // 增删计数（等宽，为零的一侧不显示）
                    .when_some(edit, |this, edit| {
                        this.child(
                            h_flex()
                                .gap_1()
                                .flex_shrink_0()
                                .text_sm()
                                .font_family(cx.theme().mono_font_family.clone())
                                .when(edit.additions > 0, |this| {
                                    this.child(
                                        div()
                                            .text_color(cx.theme().success)
                                            .child(format!("+{}", edit.additions)),
                                    )
                                })
                                .when(edit.deletions > 0, |this| {
                                    this.child(
                                        div()
                                            .text_color(cx.theme().danger)
                                            .child(format!("-{}", edit.deletions)),
                                    )
                                }),
                        )
                    })
                    .when(approval_pending, |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().warning)
                                .child("等待批准"),
                        )
                    })
                    .when(done && is_error, |this| {
                        this.child(div().text_xs().text_color(cx.theme().danger).child("失败"))
                    })
                    // 箭头默认隐藏，行悬停或展开时显示（保持行内干净）
                    .child(
                        div()
                            .invisible()
                            .group_hover(group_id, |this| this.visible())
                            .when(expanded, |this| this.visible())
                            .child(
                                Icon::new(if expanded {
                                    IconName::ChevronDown
                                } else {
                                    IconName::ChevronRight
                                })
                                .size_4()
                                .text_color(subtlest),
                            ),
                    ),
            )
            .when(expanded, |this| {
                // 展开正文统一放进带滚动条的视口（track_scroll 持久滚动位置 + 可见滚动条）
                this.child(
                    div()
                        .relative()
                        .mt_2()
                        .w_full()
                        // 滚动链：正文能滚时吞掉滚轮，避免外层消息列表联动
                        .on_scroll_wheel(consume_scroll(body_scroll))
                        // 编辑类工具展开为内联 diff 代码卡；其余工具是通用输入+输出卡
                        .child(if let Some(edit) = edit {
                            Self::render_edit_diff(
                                ("tool-body", message_ix * 1024 + segment_ix),
                                edit,
                                body_scroll,
                                cx,
                            )
                        } else {
                            v_flex()
                                .w_full()
                                .gap_3()
                                .rounded_xl()
                                .border_1()
                                .border_color(cx.theme().border)
                                .bg(cx.theme().group_box)
                                .px_4()
                                .py_3()
                                // 完整输入：终端类带 `$` 前缀，其余工具直接全文（折叠行里被截断的部分）
                                .when(!summary.is_empty(), |this| {
                                    this.child(
                                        h_flex()
                                            .w_full()
                                            .gap_2()
                                            .items_start()
                                            .when(tool == "Bash", |this| {
                                                this.child(
                                                    div().text_sm().text_color(subtle).child("$"),
                                                )
                                            })
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .text_sm()
                                                    .text_color(cx.theme().foreground)
                                                    .child(summary.to_string()),
                                            ),
                                    )
                                })
                                .child(
                                    div()
                                        .id(("tool-body", message_ix * 1024 + segment_ix))
                                        .max_h(px(120.))
                                        .overflow_y_scroll()
                                        .track_scroll(body_scroll)
                                        .text_sm()
                                        .font_family(cx.theme().mono_font_family.clone())
                                        .text_color(if is_error {
                                            cx.theme().danger
                                        } else {
                                            subtle
                                        })
                                        .child(if output.is_empty() && done {
                                            "没有输出。".to_string()
                                        } else {
                                            output.to_string()
                                        }),
                                )
                                .into_any_element()
                        })
                        // diff 卡的滚动条已内置（随圆角补丁收角）；通用卡的补在这里
                        .when(edit.is_none(), |this| {
                            this.child(Scrollbar::vertical(body_scroll))
                        }),
                )
            })
            .into_any_element()
    }

    /// 给圆角卡片补四角：填充每个角落的"R×R 方形 − 半径 R 的四分之一圆"区域
    /// （圆角缺口）。gpui 的 ContentMask 只有矩形裁剪，行底色/色条/滚动条
    /// 都会越过圆角描边；用卡片**背后**的颜色补上缺口后，内容在视觉上
    /// 即被圆角收住，不出框。曲线用二次贝塞尔逼近四分之一圆（控制点取外角，
    /// 偏差 <0.5px）。调用方需在此之后再描一次圆角边框（补丁盖住了角上的描边）。
    fn paint_rounded_corner_patches(
        bounds: Bounds<Pixels>,
        radius: Pixels,
        color: Hsla,
        window: &mut Window,
    ) {
        let r = radius;
        let w = bounds.size.width;
        let h = bounds.size.height;
        let mut path = PathBuilder::fill();
        // 左上
        path.move_to(point(px(0.), px(0.)));
        path.line_to(point(r, px(0.)));
        path.curve_to(point(px(0.), r), point(px(0.), px(0.)));
        path.close();
        // 右上
        path.move_to(point(w, px(0.)));
        path.line_to(point(w, r));
        path.curve_to(point(w - r, px(0.)), point(w, px(0.)));
        path.close();
        // 左下
        path.move_to(point(px(0.), h));
        path.line_to(point(px(0.), h - r));
        path.curve_to(point(r, h), point(px(0.), h));
        path.close();
        // 右下
        path.move_to(point(w, h));
        path.line_to(point(w, h - r));
        path.curve_to(point(w - r, h), point(w, h));
        path.close();
        path.translate(bounds.origin);
        if let Ok(path) = path.build() {
            window.paint_path(path, color);
        }
    }

    /// 编辑工具的展开卡片（ZCode LightweightDiffPreview 同款）：圆角描边代码卡，
    /// 无 padding；行号 gutter（新增绿/删除红/其余最暗）+ 增删行淡底色与左缘色条，
    /// 行号是预览行连续序号（非文件行号），限高内部滚动，超 400 行截断；
    /// 四角用卡片底色补丁收圆（gpui 内容裁剪仅矩形），滚动条内置随补丁收角。
    fn render_edit_diff(
        id: impl Into<ElementId>,
        edit: &EditDiff,
        body_scroll: &ScrollHandle,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        const MAX_ROWS: usize = 400;
        enum Kind {
            Hunk,
            Add,
            Del,
            Context,
        }
        let border = cx.theme().border;
        let code_color = cx.theme().foreground;
        let gutter_muted = cx.theme().muted_foreground.opacity(0.6);
        let added = cx.theme().success;
        let removed = cx.theme().danger;
        let transparent = cx.theme().transparent;

        let mut rows: Vec<AnyElement> = Vec::new();
        let mut line_no = 0u32;
        let mut omitted = 0usize;
        for line in edit.unified_diff.lines() {
            if rows.len() >= MAX_ROWS {
                omitted += 1;
                continue;
            }
            let (kind, text) = if line.starts_with("+++") || line.starts_with("---") {
                continue;
            } else if line.starts_with("@@") {
                (Kind::Hunk, line)
            } else if let Some(rest) = line.strip_prefix('+') {
                (Kind::Add, rest)
            } else if let Some(rest) = line.strip_prefix('-') {
                (Kind::Del, rest)
            } else if line.starts_with('\\') {
                // "\ No newline at end of file"
                continue;
            } else {
                (Kind::Context, line.strip_prefix(' ').unwrap_or(line))
            };
            if matches!(kind, Kind::Hunk) {
                rows.push(
                    h_flex()
                        .w_full()
                        .items_stretch()
                        .child(div().w(px(3.)))
                        .child(
                            div()
                                .w(px(45.))
                                .flex_shrink_0()
                                .border_r_1()
                                .border_color(border),
                        )
                        .child(
                            div()
                                .px_3()
                                .whitespace_nowrap()
                                .text_color(gutter_muted)
                                .child(text.to_string()),
                        )
                        .into_any_element(),
                );
                continue;
            }
            line_no += 1;
            let (bar, number_color, bg) = match kind {
                Kind::Add => (added, added, Some(added.opacity(0.14))),
                Kind::Del => (removed, removed, Some(removed.opacity(0.14))),
                Kind::Context => (transparent, gutter_muted, None),
                Kind::Hunk => unreachable!(),
            };
            rows.push(
                h_flex()
                    .w_full()
                    .items_stretch()
                    .when_some(bg, |this, bg| this.bg(bg))
                    // 左缘色条（对应 ZCode 的 inset 3px box-shadow）
                    .child(div().w(px(3.)).flex_shrink_0().bg(bar))
                    .child(
                        div()
                            .w(px(45.))
                            .pr_2()
                            .text_right()
                            .flex_shrink_0()
                            .border_r_1()
                            .border_color(border)
                            .text_color(number_color)
                            .child(line_no.to_string()),
                    )
                    .child(
                        div()
                            .px_3()
                            .whitespace_nowrap()
                            .text_color(code_color)
                            .child(text.to_string()),
                    )
                    .into_any_element(),
            );
        }
        if omitted > 0 {
            rows.push(
                div()
                    .w_full()
                    .py_1()
                    .text_center()
                    .text_color(gutter_muted)
                    .child(format!("… 省略 {omitted} 行 …"))
                    .into_any_element(),
            );
        }

        let card_bg = cx.theme().secondary;
        // 卡片背后 = 页面底色（消息区自身透明，与 Root 的 tokens.background 同值）
        let behind = cx.theme().background;
        div()
            .relative()
            .w_full()
            .child(
                div()
                    .id(id)
                    .w_full()
                    .rounded_xl()
                    .border_1()
                    .border_color(border)
                    .bg(card_bg)
                    .max_h(px(240.))
                    .overflow_y_scroll()
                    .track_scroll(body_scroll)
                    .text_xs()
                    .line_height(px(19.))
                    .font_family(cx.theme().mono_font_family.clone())
                    .children(rows),
            )
            // 滚动条收进卡片内部，角上同样被补丁收住
            .child(Scrollbar::vertical(body_scroll))
            .child(
                canvas(
                    |bounds, window, _| (bounds, rems(0.75).to_pixels(window.rem_size())),
                    move |bounds, (_, radius), window, _| {
                        Self::paint_rounded_corner_patches(bounds, radius, behind, window);
                    },
                )
                .absolute()
                .inset_0(),
            )
            // 补丁盖住了角上的描边，重描一遍圆角边框
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .rounded_xl()
                    .border_1()
                    .border_color(border),
            )
            .into_any_element()
    }

    /// 每轮改动面板（ZCode turn 头部「文件更改」同款）：一行汇总
    /// 「N 个文件已更改 +A -D」（箭头悬停显示），展开后逐文件行（路径 + +N/-N），
    /// 文件行再展开为内联 diff 卡（复用编辑卡的渲染）。
    fn render_turn_changes(
        &self,
        message_ix: usize,
        segment_ix: usize,
        rows: &[TurnFileRow],
        open: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let subtle = cx.theme().muted_foreground;
        let subtlest = subtle.opacity(0.6);
        let group_id = format!("turn-changes-{message_ix}-{segment_ix}");
        let (adds, dels) = rows.iter().fold((0u32, 0u32), |(a, d), r| {
            (a + r.edit.additions, d + r.edit.deletions)
        });

        let mut file_rows: Vec<AnyElement> = Vec::new();
        for (rix, row) in rows.iter().enumerate() {
            let (dir, name) = split_path(&row.edit.path);
            let row_group = format!("{group_id}-file-{rix}");
            file_rows.push(
                v_flex()
                    .w_full()
                    .child(
                        h_flex()
                            .id(("turn-file", (message_ix * 1024 + segment_ix) * 512 + rix))
                            .group(row_group.clone())
                            .w_full()
                            .gap_2()
                            .py_1()
                            .pl(px(26.))
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(Segment::TurnChanges { rows, .. }) = this
                                    .messages
                                    .get_mut(message_ix)
                                    .and_then(|m| m.segments.get_mut(segment_ix))
                                    && let Some(row) = rows.get_mut(rix)
                                {
                                    row.expanded = !row.expanded;
                                }
                                cx.notify();
                            }))
                            .child(Icon::new(IconName::FileText).size_4().text_color(subtlest))
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_sm()
                                    .text_color(subtle)
                                    .child(name),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .text_sm()
                                    .text_color(subtlest)
                                    .child(dir),
                            )
                            .when(row.edit.additions > 0, |this| {
                                this.child(
                                    div()
                                        .text_sm()
                                        .font_family(cx.theme().mono_font_family.clone())
                                        .text_color(cx.theme().success)
                                        .child(format!("+{}", row.edit.additions)),
                                )
                            })
                            .when(row.edit.deletions > 0, |this| {
                                this.child(
                                    div()
                                        .text_sm()
                                        .font_family(cx.theme().mono_font_family.clone())
                                        .text_color(cx.theme().danger)
                                        .child(format!("-{}", row.edit.deletions)),
                                )
                            })
                            .child(
                                div()
                                    .invisible()
                                    .group_hover(row_group, |this| this.visible())
                                    .when(row.expanded, |this| this.visible())
                                    .child(
                                        Icon::new(if row.expanded {
                                            IconName::ChevronDown
                                        } else {
                                            IconName::ChevronRight
                                        })
                                        .size_4()
                                        .text_color(subtlest),
                                    ),
                            ),
                    )
                    .when(row.expanded, |this| {
                        this.child(
                            div()
                                .relative()
                                .on_scroll_wheel(consume_scroll(&row.scroll))
                                .child(Self::render_edit_diff(
                                    ("turn-diff", (message_ix * 1024 + segment_ix) * 512 + rix),
                                    &row.edit,
                                    &row.scroll,
                                    cx,
                                )),
                        )
                    })
                    .into_any_element(),
            );
        }

        v_flex()
            .w_full()
            .child(
                h_flex()
                    .id(("turn-changes", message_ix * 1024 + segment_ix))
                    .group(group_id.clone())
                    .w_full()
                    .gap_2()
                    .py_1()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(Segment::TurnChanges { open, .. }) = this
                            .messages
                            .get_mut(message_ix)
                            .and_then(|m| m.segments.get_mut(segment_ix))
                        {
                            *open = !*open;
                        }
                        cx.notify();
                    }))
                    .child(
                        Icon::new(AssetIconName::ListTodo)
                            .size_4()
                            .text_color(subtlest),
                    )
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(subtlest)
                            .child(format!("{} 个文件已更改", rows.len())),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .text_sm()
                            .font_family(cx.theme().mono_font_family.clone())
                            .when(adds > 0, |this| {
                                this.child(
                                    div()
                                        .text_color(cx.theme().success)
                                        .child(format!("+{adds}")),
                                )
                            })
                            .when(dels > 0, |this| {
                                this.child(
                                    div()
                                        .text_color(cx.theme().danger)
                                        .child(format!("-{dels}")),
                                )
                            }),
                    )
                    .child(
                        div()
                            .invisible()
                            .group_hover(group_id, |this| this.visible())
                            .when(open, |this| this.visible())
                            .child(
                                Icon::new(if open {
                                    IconName::ChevronDown
                                } else {
                                    IconName::ChevronRight
                                })
                                .size_4()
                                .text_color(subtlest),
                            ),
                    ),
            )
            .when(open, |this| this.children(file_rows))
            .into_any_element()
    }

    fn render_message(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let message = &self.messages[ix];
        match message.role {
            Role::User => self.render_user_message(ix, message, cx),
            Role::System => div()
                .w_full()
                .text_center()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(message.text.clone())
                .into_any_element(),
            Role::Assistant => {
                let mut segments = Vec::with_capacity(message.segments.len() + 1);
                for (six, segment) in message.segments.iter().enumerate() {
                    // 审批不占独立行：待批准状态显示在对应的工具调用行上
                    //（ApprovalRequested 紧跟在该工具的 ToolCallBegin 之后发出）
                    if matches!(segment, Segment::Approval { .. }) {
                        continue;
                    }
                    segments.push(match segment {
                        Segment::Thinking {
                            text,
                            open,
                            started,
                            duration,
                            body_scroll,
                            ..
                        } => self.render_thinking(
                            ix,
                            six,
                            text,
                            *open,
                            *started,
                            *duration,
                            body_scroll,
                            cx,
                        ),
                        Segment::Markdown { state, .. } => TextView::new(state)
                            .selectable(true)
                            .stream_fade(self.streaming)
                            .text_sm()
                            .into_any_element(),
                        Segment::ToolCall {
                            tool,
                            summary,
                            output,
                            is_error,
                            done,
                            expanded,
                            edit,
                            body_scroll,
                        } => {
                            let approval_pending = matches!(
                                message.segments.get(six + 1),
                                Some(Segment::Approval { decision: None, .. })
                            );
                            self.render_tool_card(
                                ix,
                                six,
                                tool,
                                summary,
                                output,
                                *is_error,
                                *done,
                                *expanded,
                                approval_pending,
                                edit.as_ref(),
                                body_scroll,
                                cx,
                            )
                        }
                        Segment::Approval { .. } => unreachable!(),
                        Segment::TurnChanges { rows, open } => {
                            self.render_turn_changes(ix, six, rows, *open, cx)
                        }
                    });
                }
                if let Some(footer) = &message.footer {
                    segments.push(
                        h_flex()
                            .gap_2()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(footer.clone())
                            .into_any_element(),
                    );
                }
                if self.plan_pending && ix == self.messages.len() - 1 {
                    segments.push(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .px_3()
                            .py_2()
                            .rounded(cx.theme().radius)
                            .border_1()
                            .border_color(cx.theme().success)
                            .bg(cx.theme().success.opacity(0.08))
                            .child(
                                Icon::new(IconName::CircleCheck)
                                    .size_4()
                                    .text_color(cx.theme().success),
                            )
                            .child(div().text_sm().flex_1().child("计划已就绪"))
                            .child(
                                Button::new("execute-plan")
                                    .primary()
                                    .small()
                                    .label("执行计划 ▶")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.trigger_execute_plan(cx);
                                    })),
                            )
                            .into_any_element(),
                    );
                }
                v_flex()
                    .w_full()
                    .gap_3()
                    .children(segments.into_iter().enumerate().map(|(six, segment)| {
                        div()
                            .with_animation(
                                format!("seg-enter-{ix}-{six}"),
                                Animation::new(std::time::Duration::from_millis(150))
                                    .with_easing(ease_out_quint()),
                                |el, delta| el.opacity(delta),
                            )
                            .child(segment)
                    }))
                    .into_any_element()
            }
        }
    }
}

impl Render for ThreadView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 用户消息的选择 handle 惰性创建：订阅选择变化驱动拖动过程中的实时高亮，
        // 订阅随 ChatMessage 存放，clear() 时一并释放
        for message in &mut self.messages {
            if message.role == Role::User && message.selection.is_none() {
                let handle = TextSelectionHandle::new(message.text.clone(), cx);
                let subscription = handle.refresh_window_on_change(window, cx);
                message.selection = Some((handle, subscription));
            }
        }
        let mut items = Vec::with_capacity(self.messages.len());
        for ix in 0..self.messages.len() {
            items.push(self.render_message(ix, cx));
        }

        let working_secs = self
            .turn_started
            .map(|t| t.elapsed().as_secs())
            .unwrap_or(0);
        let working_label = if working_secs >= 60 {
            format!("工作中 {} 分 {} 秒", working_secs / 60, working_secs % 60)
        } else {
            format!("工作中 {working_secs} 秒")
        };

        // 回到底部（滚轮/拖滚动条/键盘任意方式）自动恢复跟随
        if !self.follow_bottom && self.at_bottom() {
            self.follow_bottom = true;
        }

        v_flex()
            .size_full()
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .id("message-list")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll_handle)
                            // 用户上翻：暂停跟随并浮出「最新消息」按钮（不吞事件，列表照常滚动）
                            .on_scroll_wheel(cx.listener(
                                |this, event: &ScrollWheelEvent, window, cx| {
                                    let delta = event.delta.pixel_delta(window.line_height());
                                    if delta.y > px(0.) && this.follow_bottom {
                                        this.follow_bottom = false;
                                        cx.notify();
                                    }
                                },
                            ))
                            .child(
                                v_flex()
                                    .w_full()
                                    .max_w(px(860.))
                                    .mx_auto()
                                    .p_4()
                                    .gap_4()
                                    .children(items)
                                    // 工作中指示：跟在最后一条消息之后，随对话一起滚动
                                    .when(self.streaming, |this| {
                                        this.child(
                                            h_flex()
                                                .gap_2()
                                                .child(
                                                    Spinner::new()
                                                        .icon(AssetIconName::LoaderCircle)
                                                        .color(cx.theme().muted_foreground),
                                                )
                                                .child(
                                                    ShimmerText::new(working_label)
                                                        .id("working-shimmer")
                                                        .text_xs()
                                                        .text_color(cx.theme().muted_foreground),
                                                ),
                                        )
                                    })
                                    .when(self.messages.is_empty(), |this| {
                                        this.child(
                                    div()
                                        .w_full()
                                        .py_8()
                                        .text_center()
                                        .text_sm()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(
                                            "空会话。输入消息开始对话，/ 查看命令，@ 引用文件。",
                                        ),
                                )
                                    }),
                            ),
                    )
                    // 未跟随时浮出「最新消息」按钮：点击回到底部并恢复跟随
                    .when(!self.follow_bottom, |this| {
                        this.child(
                            h_flex()
                                .absolute()
                                .bottom_4()
                                .left_0()
                                .right_0()
                                .justify_center()
                                .child(
                                    h_flex()
                                        .id("latest-fab")
                                        .items_center()
                                        .gap_2()
                                        .px_3()
                                        .py_2()
                                        .rounded_full()
                                        .bg(cx.theme().popover)
                                        .border_1()
                                        .border_color(cx.theme().border)
                                        .shadow_md()
                                        .child(
                                            Icon::new(AssetIconName::ArrowDown)
                                                .size_4()
                                                .text_color(cx.theme().foreground),
                                        )
                                        .child(div().text_sm().child("最新消息"))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.follow_bottom = true;
                                            this.scroll_handle.scroll_to_bottom();
                                            cx.notify();
                                        })),
                                ),
                        )
                    }),
            )
            .when(!self.queued.is_empty(), |this| {
                this.child(
                    h_flex()
                        .w_full()
                        .max_w(px(860.))
                        .mx_auto()
                        .px_4()
                        .pb_2()
                        .gap_2()
                        .children(self.queued.iter().enumerate().map(|(ix, text)| {
                            let text = text.clone();
                            h_flex()
                                .id(("queued", ix))
                                .gap_1()
                                .pl_2()
                                .pr_1()
                                .py_0p5()
                                .rounded_full()
                                .border_1()
                                .border_color(cx.theme().border)
                                .bg(cx.theme().accent.opacity(0.5))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(format!(
                                            "排队中: {}",
                                            text.chars().take(20).collect::<String>()
                                        )),
                                )
                                .child(
                                    div()
                                        .id(("queued-cancel", ix))
                                        .cursor_pointer()
                                        .rounded_sm()
                                        .hover(|this| this.bg(cx.theme().danger.opacity(0.3)))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.queued.remove(ix);
                                            cx.emit(ThreadEvent::CancelQueued(text.clone()));
                                            cx.notify();
                                        }))
                                        .child(Icon::new(IconName::Close).size_3()),
                                )
                        })),
                )
            })
    }
}

/// 拆成 (目录部分含结尾分隔符, 文件名)；无分隔符时目录为空
fn split_path(path: &str) -> (String, String) {
    match path.rfind(['/', '\\']) {
        Some(ix) => (path[..=ix].to_string(), path[ix + 1..].to_string()),
        None => (String::new(), path.to_string()),
    }
}

/// 不滚动穿透：滚轮落在展开正文上一律吞掉（这版 gpui 的内置滚动监听不阻断冒泡，
/// 不吞的话外层消息列表会联动）；到顶/到底也不放行给外层。
fn consume_scroll(
    _handle: &ScrollHandle,
) -> impl Fn(&ScrollWheelEvent, &mut Window, &mut App) + 'static {
    move |_, _, cx| {
        cx.stop_propagation();
    }
}
