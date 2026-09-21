use std::collections::HashMap;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::text::{TextView, TextViewState};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, h_flex, v_flex};
use gpui_kit::component::{Sizable as _, StyledExt as _};
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
        tool: String,
        detail: String,
        decision: Option<ApprovalDecision>,
        detail_open: bool,
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
            Event::TurnStarted { .. } => {
                self.plan_pending = false;
                self.messages.push(ChatMessage::assistant());
                self.item_index.clear();
                self.set_streaming(true, cx);
            }
            Event::ReasoningDelta { item_id, delta, .. } => {
                let six = self.find_or_create(&item_id, || Segment::Thinking {
                    text: String::new(),
                    open: true,
                    pinned: false,
                });
                if let Some(Segment::Thinking { text, .. }) = self.current_segment(six) {
                    text.push_str(&delta);
                }
                self.scroll_handle.scroll_to_bottom();
            }
            Event::TextDelta { item_id, delta, .. } => {
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
                    ..
                }) = self.current_segment(six)
                {
                    *out = output;
                    *err = is_error;
                    *done = true;
                }
                self.scroll_handle.scroll_to_bottom();
            }
            Event::ApprovalRequested {
                request_id,
                tool,
                detail,
                ..
            } => {
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
                        tool,
                        detail,
                        decision: None,
                        detail_open: false,
                    });
                self.scroll_handle.scroll_to_bottom();
            }
            Event::ContextUsage { used, total, .. } => {
                self.context_usage = Some((used, total));
            }
            Event::TurnComplete { duration_ms, .. } => {
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
                self.set_streaming(false, cx);
                self.messages
                    .push(ChatMessage::system(format!("⚠ {message}")));
            }
        }
        cx.notify();
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

    fn render_thinking(
        &self,
        message_ix: usize,
        segment_ix: usize,
        text: &str,
        open: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        v_flex()
            .w_full()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .id(("thinking", message_ix * 1024 + segment_ix))
                    .w_full()
                    .gap_2()
                    .px_3()
                    .py_1()
                    .rounded(cx.theme().radius)
                    .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
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
                        Icon::new(if open {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .size_4()
                        .text_color(cx.theme().muted_foreground),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("思考过程（{} 字）", text.chars().count())),
                    ),
            )
            .when(open, |this| {
                this.child(
                    div()
                        .relative()
                        .px_3()
                        .py_2()
                        .border_t_1()
                        .border_color(cx.theme().border)
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .with_animation(
                            ("thinking-open", message_ix * 1024 + segment_ix),
                            Animation::new(std::time::Duration::from_millis(120))
                                .with_easing(ease_out_quint()),
                            |el, delta| el.top(px(4.0 * (1.0 - delta))).opacity(delta),
                        )
                        .child(text.to_string()),
                )
            })
            .into_any_element()
    }

    fn render_approval(
        &self,
        message_ix: usize,
        segment_ix: usize,
        tool: &str,
        detail: &str,
        decision: Option<ApprovalDecision>,
        detail_open: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let detail_lines: Vec<&str> = detail.lines().collect();
        let overflow = detail_lines.len() > 10 && !detail_open;
        let visible: &[&str] = if overflow {
            &detail_lines[..10]
        } else {
            &detail_lines
        };

        v_flex()
            .w_full()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().warning)
            .bg(cx.theme().popover)
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .child(
                        Icon::new(IconName::TriangleAlert)
                            .size_4()
                            .text_color(cx.theme().warning),
                    )
                    .child(
                        div()
                            .text_sm()
                            .font_semibold()
                            .child(format!("请求执行 {tool}")),
                    )
                    .child(div().flex_1())
                    .when_some(decision, |this, decision| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(match decision {
                                    ApprovalDecision::Allow => "已允许",
                                    ApprovalDecision::AlwaysAllow => "已始终允许",
                                    ApprovalDecision::Reject => "已拒绝",
                                }),
                        )
                    }),
            )
            .child(
                v_flex()
                    .px_3()
                    .py_2()
                    .text_xs()
                    .font_family("monospace")
                    .text_color(cx.theme().muted_foreground)
                    .children(
                        visible
                            .iter()
                            .map(|line| div().whitespace_nowrap().child(line.to_string())),
                    ),
            )
            .when(overflow, |this| {
                this.child(
                    div()
                        .id(("approval-expand", message_ix * 1024 + segment_ix))
                        .px_3()
                        .pb_2()
                        .text_xs()
                        .text_color(cx.theme().link)
                        .cursor_pointer()
                        .hover(|this| this.text_color(cx.theme().link_hover))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(Segment::Approval { detail_open, .. }) = this
                                .messages
                                .get_mut(message_ix)
                                .and_then(|m| m.segments.get_mut(segment_ix))
                            {
                                *detail_open = true;
                            }
                            cx.notify();
                        }))
                        .child(format!("展开全部（共 {} 行）", detail_lines.len())),
                )
            })
            .when(decision.is_none(), |this| {
                this.child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .px_3()
                        .pb_2()
                        .child(
                            Button::new(("approval-allow", message_ix * 1024 + segment_ix))
                                .primary()
                                .small()
                                .label("允许")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.decide_approval(
                                        message_ix,
                                        segment_ix,
                                        ApprovalDecision::Allow,
                                        cx,
                                    );
                                })),
                        )
                        .child(
                            Button::new(("approval-always", message_ix * 1024 + segment_ix))
                                .outline()
                                .small()
                                .label("始终允许")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.decide_approval(
                                        message_ix,
                                        segment_ix,
                                        ApprovalDecision::AlwaysAllow,
                                        cx,
                                    );
                                })),
                        )
                        .child(
                            Button::new(("approval-reject", message_ix * 1024 + segment_ix))
                                .danger()
                                .small()
                                .label("拒绝")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.decide_approval(
                                        message_ix,
                                        segment_ix,
                                        ApprovalDecision::Reject,
                                        cx,
                                    );
                                })),
                        ),
                )
            })
            .into_any_element()
    }

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
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let lines: Vec<&str> = output.lines().collect();
        let overflow = lines.len() > 6 && !expanded;
        let visible: &[&str] = if overflow { &lines[..6] } else { &lines };

        let (status_icon, status_color) = if !done {
            (IconName::LoaderCircle, cx.theme().muted_foreground)
        } else if is_error {
            (IconName::TriangleAlert, cx.theme().danger)
        } else {
            (IconName::CircleCheck, cx.theme().success)
        };

        v_flex()
            .w_full()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().popover)
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .px_3()
                    .py_1()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        Icon::new(if tool == "bash" {
                            IconName::SquareTerminal
                        } else {
                            IconName::FileText
                        })
                        .size_4()
                        .text_color(cx.theme().muted_foreground),
                    )
                    .child(
                        div()
                            .text_sm()
                            .font_family("monospace")
                            .child(format!("{tool} {summary}")),
                    )
                    .child(div().flex_1())
                    .child(Icon::new(status_icon).size_4().text_color(status_color)),
            )
            .when(!output.is_empty(), |this| {
                this.child(
                    v_flex()
                        .px_3()
                        .py_2()
                        .text_xs()
                        .text_color(if is_error {
                            cx.theme().danger
                        } else {
                            cx.theme().muted_foreground
                        })
                        .font_family("monospace")
                        .children(
                            visible
                                .iter()
                                .map(|line| div().whitespace_nowrap().child(line.to_string())),
                        ),
                )
            })
            .when(overflow, |this| {
                this.child(
                    div()
                        .id(("tool-expand", message_ix * 1024 + segment_ix))
                        .px_3()
                        .pb_2()
                        .text_xs()
                        .text_color(cx.theme().link)
                        .cursor_pointer()
                        .hover(|this| this.text_color(cx.theme().link_hover))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(Segment::ToolCall { expanded, .. }) = this
                                .messages
                                .get_mut(message_ix)
                                .and_then(|m| m.segments.get_mut(segment_ix))
                            {
                                *expanded = true;
                            }
                            cx.notify();
                        }))
                        .child(format!("展开全部（共 {} 行）", lines.len())),
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
                    segments.push(match segment {
                        Segment::Thinking { text, open, .. } => {
                            self.render_thinking(ix, six, text, *open, cx)
                        }
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
                        } => self.render_tool_card(
                            ix, six, tool, summary, output, *is_error, *done, *expanded, cx,
                        ),
                        Segment::Approval {
                            tool,
                            detail,
                            decision,
                            detail_open,
                            ..
                        } => {
                            self.render_approval(ix, six, tool, detail, *decision, *detail_open, cx)
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
