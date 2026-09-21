use std::collections::HashMap;

use gpui_kit::assets::IconName as AssetIconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::text::{TextView, TextViewState};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, h_flex, v_flex};
use gpui_kit::component::Sizable as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use pig_protocol::{ApprovalDecision, Event};

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
    },
    Approval {
        request_id: String,
        decision: Option<ApprovalDecision>,
    },
}

pub struct ChatMessage {
    pub role: Role,
    pub text: String,
    pub files: Vec<String>,
    pub segments: Vec<Segment>,
    pub footer: Option<String>,
}

impl ChatMessage {
    fn user(text: String, files: Vec<String>) -> Self {
        Self {
            role: Role::User,
            text,
            files,
            segments: vec![],
            footer: None,
        }
    }

    fn system(text: String) -> Self {
        Self {
            role: Role::System,
            text,
            files: vec![],
            segments: vec![],
            footer: None,
        }
    }

    fn assistant() -> Self {
        Self {
            role: Role::Assistant,
            text: String::new(),
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
    streaming: bool,
    context_usage: Option<(u64, u64)>,
    /// 计划模式回合完成，等待用户确认执行
    plan_pending: bool,
    turn_started: Option<std::time::Instant>,
    /// 当前回合由回放重建（turn_id 以 replay- 开头）：思考段不打真实用时
    replay_turn: bool,
    /// 本会话变更统计（来自 AppView 汇总）
    changes: (u32, u32),
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
            streaming: false,
            context_usage: None,
            plan_pending: false,
            turn_started: None,
            replay_turn: false,
            changes: (0, 0),
            queued: Vec::new(),
            _ticker: ticker,
        }
    }

    fn set_streaming(&mut self, streaming: bool, _cx: &mut Context<Self>) {
        self.streaming = streaming;
        self.turn_started = streaming.then(std::time::Instant::now);
    }

    pub fn set_changes(&mut self, added: u32, removed: u32, cx: &mut Context<Self>) {
        self.changes = (added, removed);
        cx.notify();
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
        self.scroll_handle.scroll_to_bottom();
        cx.notify();
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.messages.clear();
        self.item_index.clear();
        cx.notify();
    }

    pub fn add_system_note(&mut self, text: &str, cx: &mut Context<Self>) {
        self.messages.push(ChatMessage::system(text.to_string()));
        self.scroll_handle.scroll_to_bottom();
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
                Segment::Approval { .. } => {}
            }
        }
        (tool_done, text, thinking, tool_output)
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
                });
                if let Some(Segment::Thinking { text, .. }) = self.current_segment(six) {
                    text.push_str(&delta);
                }
                self.scroll_handle.scroll_to_bottom();
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
                self.scroll_handle.scroll_to_bottom();
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
                });
                if let Some(Segment::ToolCall { tool, summary, .. }) = self.current_segment(six) {
                    *tool = tool.clone();
                    *summary = input_summary;
                }
                self.scroll_handle.scroll_to_bottom();
            }
            Event::ToolCallEnd {
                item_id,
                output,
                is_error,
                ..
            } => {
                let six = self.find_or_create(&item_id, || Segment::ToolCall {
                    tool: String::new(),
                    summary: String::new(),
                    output: String::new(),
                    is_error: false,
                    done: false,
                    expanded: false,
                });
                if let Some(Segment::ToolCall {
                    output: out,
                    is_error: err,
                    done,
                    expanded,
                    ..
                }) = self.current_segment(six)
                {
                    *out = output;
                    *err = is_error;
                    *done = true;
                    // 失败的调用直接展开输出，省去用户多点一下
                    if is_error {
                        *expanded = true;
                    }
                }
                self.scroll_handle.scroll_to_bottom();
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
                self.scroll_handle.scroll_to_bottom();
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
            | Event::GitInfo { .. }
            | Event::BranchChanged { .. }
            | Event::ConfigSnapshot { .. }
            | Event::TestResult { .. }
            | Event::ProjectList { .. } => {}
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

    fn render_user_message(&self, message: &ChatMessage, cx: &mut Context<Self>) -> AnyElement {
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
                    .with_animation(
                        "user-msg-enter",
                        Animation::new(std::time::Duration::from_millis(150))
                            .with_easing(ease_out_quint()),
                        |el, delta| el.top(px(4.0 * (1.0 - delta))).opacity(delta),
                    )
                    .child(message.text.clone()),
            )
            .into_any_element()
    }

    /// 思考折叠块（ZCode 同款）：无边框的一行 header（大脑图标 + 文案 + 箭头），
    /// 展开后正文以左侧竖线缩进展示，超高内部滚动。
    #[allow(clippy::too_many_arguments)]
    fn render_thinking(
        &self,
        message_ix: usize,
        segment_ix: usize,
        text: &str,
        open: bool,
        started: std::time::Instant,
        duration: Option<std::time::Duration>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let secs = |d: std::time::Duration| (d.as_secs_f64().ceil() as u64).max(1);
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
        v_flex()
            .w_full()
            .child(
                h_flex()
                    .id(("thinking", message_ix * 1024 + segment_ix))
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
                    .child(Icon::new(AssetIconName::Brain).size_4().text_color(muted))
                    .child(div().text_sm().text_color(muted).child(label))
                    .child(
                        Icon::new(if open {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .size_4()
                        .text_color(muted),
                    ),
            )
            .when(open, |this| {
                this.child(
                    div()
                        .id(("thinking-body", message_ix * 1024 + segment_ix))
                        .mt_1()
                        .ml(px(8.))
                        .border_l_1()
                        .border_color(cx.theme().border)
                        .pl(px(14.))
                        .max_h(px(240.))
                        .overflow_y_scroll()
                        .text_sm()
                        .text_color(muted)
                        .child(text.to_string()),
                )
            })
            .into_any_element()
    }

    /// 工具调用折叠行（与思考块同族）：无边框 header（工具图标 + 名称 · 摘要 +
    /// 状态 + 箭头），展开后输出以左侧竖线缩进展示，超高内部滚动。
    /// `approval_pending`：该工具正在等待批准（状态位显示黄色 ✋ 等待批准）。
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
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let tool_icon = match tool {
            "bash" => AssetIconName::Terminal,
            "read_file" => AssetIconName::Eye,
            "write_file" => AssetIconName::FilePlus,
            "edit" => AssetIconName::FilePen,
            "glob" => AssetIconName::FolderSearch,
            "grep" => AssetIconName::TextSearch,
            _ => AssetIconName::Wrench,
        };
        let (status_icon, status_color) = if approval_pending {
            (AssetIconName::Hand, cx.theme().warning)
        } else if !done {
            (AssetIconName::LoaderCircle, muted)
        } else if is_error {
            (AssetIconName::TriangleAlert, cx.theme().danger)
        } else {
            (AssetIconName::CircleCheck, cx.theme().success)
        };

        v_flex()
            .w_full()
            .child(
                h_flex()
                    .id(("tool", message_ix * 1024 + segment_ix))
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
                    .child(Icon::new(tool_icon).size_4().text_color(muted))
                    .child(div().text_sm().text_color(muted).child(tool.to_string()))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_sm()
                            .font_family("monospace")
                            .text_color(muted)
                            .child(summary.to_string()),
                    )
                    .when(approval_pending, |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().warning)
                                .child("等待批准"),
                        )
                    })
                    .child(Icon::new(status_icon).size_4().text_color(status_color))
                    .child(
                        Icon::new(if expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .size_4()
                        .text_color(muted),
                    ),
            )
            .when(expanded, |this| {
                this.child(
                    div()
                        .id(("tool-body", message_ix * 1024 + segment_ix))
                        .mt_1()
                        .ml(px(8.))
                        .border_l_1()
                        .border_color(cx.theme().border)
                        .pl(px(14.))
                        .max_h(px(240.))
                        .overflow_y_scroll()
                        .text_xs()
                        .font_family("monospace")
                        .text_color(if is_error { cx.theme().danger } else { muted })
                        .child(if output.is_empty() {
                            "（暂无输出）".to_string()
                        } else {
                            output.to_string()
                        }),
                )
            })
            .into_any_element()
    }

    fn render_message(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let message = &self.messages[ix];
        match message.role {
            Role::User => self.render_user_message(message, cx),
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
                            ..
                        } => self.render_thinking(ix, six, text, *open, *started, *duration, cx),
                        Segment::Markdown { state, .. } => TextView::new(state)
                            .selectable(true)
                            .stream_fade(self.streaming)
                            .into_any_element(),
                        Segment::ToolCall {
                            tool,
                            summary,
                            output,
                            is_error,
                            done,
                            expanded,
                            ..
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
                                cx,
                            )
                        }
                        Segment::Approval { .. } => unreachable!(),
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
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut items = Vec::with_capacity(self.messages.len());
        for ix in 0..self.messages.len() {
            items.push(self.render_message(ix, cx));
        }

        let working_secs = self
            .turn_started
            .map(|t| t.elapsed().as_secs())
            .unwrap_or(0);
        let (added, removed) = self.changes;

        v_flex()
            .size_full()
            .child(
                h_flex()
                    .w_full()
                    .px_4()
                    .py_2()
                    .min_h(px(28.))
                    .child(h_flex().gap_2().when(self.streaming, |this| {
                        this.child(
                            Icon::new(IconName::LoaderCircle)
                                .size_4()
                                .text_color(cx.theme().muted_foreground),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(format!("工作中 {working_secs} 秒")),
                        )
                    }))
                    .child(div().flex_1())
                    .when(added + removed > 0, |this| {
                        this.child(
                            h_flex()
                                .gap_1()
                                .px_2()
                                .py_0p5()
                                .rounded_full()
                                .bg(cx.theme().accent)
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child("更改"),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().success)
                                        .child(format!("+{added}")),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().danger)
                                        .child(format!("-{removed}")),
                                ),
                        )
                    }),
            )
            .child(
                div()
                    .id("message-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll_handle)
                    .child(
                        v_flex()
                            .w_full()
                            .max_w(px(860.))
                            .mx_auto()
                            .p_4()
                            .gap_4()
                            .children(items)
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
