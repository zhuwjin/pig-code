use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use gpui_kit::assets::IconName as AssetIconName;
use gpui_kit::base::{
    Align, ElementExt as _, Placement, Positioner, Scrollbar, SelectableText, TextSelectionHandle,
};
use gpui_kit::component::Sizable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::shimmer::ShimmerText;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::text::{TextView, TextViewState, TextViewStyle};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use gpui_kit::{Overflow, StyleRefinement};
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
    /// 计划模式回合完成，等待用户确认执行
    plan_pending: bool,
    turn_started: Option<std::time::Instant>,
    /// 当前回合由回放重建（turn_id 以 replay- 开头）：思考段不打真实用时
    replay_turn: bool,
    /// 排队中的消息（FIFO）
    queued: Vec<String>,
    /// turn 导航条：悬停的用户消息下标（驱动横条的山峰式加宽与高亮）
    nav_hover: Option<usize>,
    /// 预览卡当前为哪条消息打开（悬停稳定 120ms 才打开，离开 80ms 才关闭）
    nav_card: Option<usize>,
    /// 各导航横条的屏幕 bounds（on_prepaint 记录），预览卡按它做侧边锚定；
    /// render 只持 &self，故用 RefCell
    nav_bar_bounds: RefCell<HashMap<usize, Rc<Cell<Bounds<Pixels>>>>>,
    /// 导航条自身的滚动句柄（turn 数超出可见高度时 rail 内部滚动）
    nav_rail_scroll: ScrollHandle,
    /// 上一帧的活动导航项；活动项变化时让 rail 滚动到可见
    nav_last_active: Option<usize>,
    /// 导航点击后抑制一次「回到底部自动恢复跟随」：跳转滚动在 prepaint 才生效，
    /// 生效前 offset 仍是旧值，贴着底会被误判成用户滚回了底部
    nav_jump: bool,
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
            plan_pending: false,
            turn_started: None,
            replay_turn: false,
            queued: Vec::new(),
            nav_hover: None,
            nav_card: None,
            nav_bar_bounds: RefCell::new(HashMap::new()),
            nav_rail_scroll: ScrollHandle::new(),
            nav_last_active: None,
            nav_jump: false,
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

    /// 自测用：turn 导航条可见条件——（用户消息数, 消息面板宽度 px）
    pub fn debug_nav_state(&self) -> (usize, f32) {
        let turns = self
            .messages
            .iter()
            .filter(|m| m.role == Role::User)
            .count();
        (turns, f32::from(self.scroll_handle.bounds().size.width))
    }

    /// 自测用：导航条活动项排查——（nav_last_active, offset_y, max_offset_y,
    /// 容器高, 各用户消息行的 [top, bottom) 内容坐标）
    pub fn debug_nav_active_detail(
        &self,
    ) -> (Option<usize>, f32, f32, f32, Vec<(usize, f32, f32)>) {
        let user_rows = self
            .messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.role == Role::User)
            .filter_map(|(ix, _)| {
                self.scroll_handle
                    .bounds_for_item(ix)
                    .map(|b| (ix, f32::from(b.top()), f32::from(b.bottom())))
            })
            .collect();
        (
            self.nav_last_active,
            f32::from(self.scroll_handle.offset().y),
            f32::from(self.scroll_handle.max_offset().y),
            f32::from(self.scroll_handle.bounds().size.height),
            user_rows,
        )
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.messages.clear();
        self.item_index.clear();
        self.follow_bottom = true;
        self.nav_hover = None;
        self.nav_card = None;
        self.nav_bar_bounds.borrow_mut().clear();
        self.nav_last_active = None;
        self.nav_jump = false;
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
            // 模式 chip 在 composer（main.rs 处理）；消息流无需响应
            Event::ExecModeChanged { .. } => {}
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
            Event::TurnComplete {
                duration_ms, stats, ..
            } => {
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
                // duration_ms=0 是会话回放的收尾事件：只退出流式状态，不写用时脚注；
                // stats 有值的历史回合（TurnStats 回放）仍写完整统计脚注
                if duration_ms > 0 || stats.is_some() {
                    let stats_part = stats.as_ref().map(format_turn_stats).unwrap_or_default();
                    let duration = stats
                        .as_ref()
                        .map(|s| s.duration_ms)
                        .filter(|ms| *ms > 0)
                        .unwrap_or(duration_ms);
                    if let Some(message) = self.messages.last_mut() {
                        message.footer = Some(format!(
                            "回合结束 · 用时 {:.1}s{stats_part}",
                            duration as f64 / 1000.0
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
            Event::FileChanged { .. } | Event::FileReverted { .. } | Event::ContextUsage { .. } => {
            }
            Event::UserMessage {
                text,
                files,
                image_count,
                ..
            } => {
                let trimmed = text.trim().to_string();
                // 气泡末尾追加图片张数占位（不渲染缩略图）；队列匹配用原文
                let display = if image_count > 0 {
                    format!("{text}\n\n[图片 ×{image_count}]")
                } else {
                    text
                };
                self.append_user_message(display, files, cx);
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
            | Event::SessionTitleChanged { .. }
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
            | Event::ModelInfo { .. }
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
                    // 成功不给标记；失败在行尾放叉号（悬停显示原因）
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
                    )
                    // 失败：行尾叉号常显，悬停展示失败原因（输出压单行并截断）
                    .when(done && is_error, |this| {
                        let collapsed = output.split_whitespace().collect::<Vec<_>>().join(" ");
                        const MAX_REASON_CHARS: usize = 200;
                        let reason = if collapsed.chars().count() > MAX_REASON_CHARS {
                            let head: String =
                                collapsed.chars().take(MAX_REASON_CHARS - 3).collect();
                            format!("{}...", head.trim_end())
                        } else {
                            collapsed
                        };
                        this.child(
                            div()
                                .id(("tool-err", message_ix * 1024 + segment_ix))
                                .flex_shrink_0()
                                .tooltip(move |window, cx| {
                                    Tooltip::new(reason.clone()).build(window, cx)
                                })
                                .child(
                                    Icon::new(IconName::Close)
                                        .size_3()
                                        .text_color(cx.theme().danger),
                                ),
                        )
                    }),
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
                        Segment::Markdown { state, .. } => {
                            // 表格列宽对齐 ZCode（markdown-table.tsx 的
                            // `w-max min-w-full` + auto table layout）：列贴合内容宽度，
                            // 帧宽不足时先收缩并让单元格文本换行，收缩到列地板后
                            // 整体横向滚动，而不是按字符数比例把列无限压瘪。
                            // 组件层 TextViewStyle 会叠在主题派生样式之上，圆角/
                            // 表头底色都保留；这里只覆盖表格容器一项。
                            let mut table = StyleRefinement::default();
                            table.overflow.x = Some(Overflow::Scroll);
                            TextView::new(state)
                                .selectable(true)
                                .stream_fade(self.streaming)
                                .text_sm()
                                .style(TextViewStyle::default().table(table))
                                .into_any_element()
                        }
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

    /// turn 导航条（ZCode ConversationTurnNavigator 同款）：消息流左缘的竖排
    /// 小横条，一条用户消息一根。悬停时目标与相邻横条山峰式加宽；悬停稳定
    /// 120ms 后在横条右侧弹出该轮预览卡（用户消息前 2 行 + 助手回复前 3 行，
    /// 离开 80ms 关闭）；点击跳转对应消息。
    /// 无悬停时高亮视口顶部所属的 turn；流式中的最后一根保持最低亮度。
    fn render_turn_nav(
        &self,
        user_ixs: &[usize],
        active: Option<usize>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let foreground = cx.theme().foreground;
        let subtlest = cx.theme().muted_foreground.opacity(0.6);
        // 只有最后一轮可能处于流式（对齐 ZCode：同一 running turn 只有最后
        // 一条 query 呈现 running 强调）
        let running_ix = if self.streaming {
            user_ixs.last().copied()
        } else {
            None
        };
        let focus_pos = self
            .nav_hover
            .and_then(|hover| user_ixs.iter().position(|&ix| ix == hover));
        // rail 高度上限：对齐 ZCode 的 max-h calc(100% - 6rem)
        let rail_max_h = self.scroll_handle.bounds().size.height - px(96.);
        let rail_max_h = if rail_max_h < px(0.) {
            px(0.)
        } else {
            rail_max_h
        };
        // 清掉已不存在消息的横条 bounds
        self.nav_bar_bounds
            .borrow_mut()
            .retain(|&ix, _| ix < self.messages.len());
        // 预览卡内容只给当前打开的那根横条算（不必每帧为全部横条生成预览文本）
        let card = self.nav_card.and_then(|ix| {
            let bounds = self
                .nav_bar_bounds
                .borrow()
                .get(&ix)
                .map(|cell| cell.get())?;
            if bounds.size.width <= px(0.) {
                return None; // 首帧 prepaint 前还没有 bounds
            }
            let user_preview = nav_preview_text(&[self.messages[ix].text.as_str()], "（无文本）");
            let (assistant_preview, assistant_is_text) = self.nav_assistant_preview(ix, user_ixs);
            Some((bounds, user_preview, assistant_preview, assistant_is_text))
        });

        div()
            .absolute()
            .top_0()
            .bottom_0()
            .left(px(8.))
            .flex()
            .flex_col()
            .justify_center()
            .child(
                v_flex()
                    .id("turn-nav-rail")
                    .w(px(32.))
                    .max_h(rail_max_h)
                    .overflow_y_scroll()
                    .track_scroll(&self.nav_rail_scroll)
                    // 滚轮落在导航条上只滚 rail，不联动消息列表
                    .on_scroll_wheel(consume_scroll(&self.nav_rail_scroll))
                    .children(user_ixs.iter().enumerate().map(|(pos, &ix)| {
                        // 山峰式加宽：悬停项 2.6x，相邻 1.7x / 1.25x（对齐 ZCode 档位）；
                        // 最大 31.2px，不超出 32px 的 rail 宽度
                        let (scale, mut opacity, focus_color): (f32, f32, bool) =
                            match focus_pos.map(|focus| pos.abs_diff(focus)) {
                                Some(0) => (2.6_f32, 1.0, true),
                                Some(1) => (1.7, 0.86, false),
                                Some(2) => (1.25, 0.72, false),
                                _ => (1.0, 0.58, false),
                            };
                        // 无悬停时由滚动位置驱动的活动项强调
                        let show_active = focus_pos.is_none() && active == Some(ix);
                        if show_active {
                            opacity = 0.9;
                        }
                        if running_ix == Some(ix) {
                            opacity = opacity.max(0.72);
                        }
                        let color = if focus_color || show_active {
                            foreground
                        } else {
                            subtlest
                        };
                        let bounds_cell = self
                            .nav_bar_bounds
                            .borrow_mut()
                            .entry(ix)
                            .or_insert_with(|| Rc::new(Cell::new(Bounds::default())))
                            .clone();
                        div()
                            .id(("turn-nav-bar", ix))
                            .w_full()
                            .h(px(10.))
                            .flex()
                            .items_center()
                            .cursor_pointer()
                            .on_prepaint(move |bounds, _, _| bounds_cell.set(bounds))
                            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                                if *hovered {
                                    this.nav_hover = Some(ix);
                                    // 悬停稳定 120ms 才开卡（对齐 ZCode openDelay），
                                    // 快速滑过不闪卡
                                    cx.spawn(async move |this, cx| {
                                        cx.background_executor()
                                            .timer(std::time::Duration::from_millis(120))
                                            .await;
                                        this.update(cx, |this, cx| {
                                            if this.nav_hover == Some(ix) {
                                                this.nav_card = Some(ix);
                                                cx.notify();
                                            }
                                        })
                                        .ok();
                                    })
                                    .detach();
                                } else {
                                    // 只清自己这根的悬停：gpui 按绘制顺序逐元素
                                    // 判定 hover，向上滑（bar2→bar1）时新横条的
                                    // enter 先触发、旧横条的 leave 后到，无条件
                                    // 清空会把刚设置的 enter 抹掉
                                    if this.nav_hover == Some(ix) {
                                        this.nav_hover = None;
                                    }
                                    // 离开 80ms 才关闭（对齐 ZCode closeDelay）；
                                    // 这期间移到相邻横条会取消关闭
                                    cx.spawn(async move |this, cx| {
                                        cx.background_executor()
                                            .timer(std::time::Duration::from_millis(80))
                                            .await;
                                        this.update(cx, |this, cx| {
                                            if this.nav_hover.is_none() {
                                                this.nav_card = None;
                                                cx.notify();
                                            }
                                        })
                                        .ok();
                                    })
                                    .detach();
                                }
                                cx.notify();
                            }))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                // 跳走后暂停跟随；若目标就在底部附近，render
                                // 里的 at_bottom 检查会恢复跟随
                                this.follow_bottom = false;
                                this.nav_jump = true;
                                this.scroll_handle.scroll_to_top_of_item(ix);
                                cx.notify();
                            }))
                            .child(
                                div()
                                    .h(px(2.))
                                    .w(px(12. * scale))
                                    .rounded_full()
                                    .bg(color)
                                    .opacity(opacity),
                            )
                            .into_any_element()
                    })),
            )
            // 预览卡：deferred 到窗口层绘制（逃出 rail 的滚动裁剪），锚定横条右侧
            //（ZCode 是 side=right align=start sideOffset=8 的 HoverCard；gpui-kit
            // 的 HoverCard 只有 corner 锚定、弹不到触发器右侧，故按 Positioner 自绘）
            .when_some(
                card,
                |this, (bounds, user_preview, assistant_preview, is_text)| {
                    this.child(
                        deferred(
                            Positioner::side(bounds)
                                .placement(Placement::Right)
                                .align(Align::Start)
                                .offset(px(8.))
                                .margin(px(8.))
                                .occlude()
                                .child(
                                    v_flex()
                                        .w(px(320.))
                                        .p_3()
                                        .gap_2()
                                        .bg(cx.theme().popover)
                                        .border_1()
                                        .border_color(cx.theme().border)
                                        .rounded_lg()
                                        .shadow_lg()
                                        .child(
                                            div()
                                                .text_sm()
                                                .font_medium()
                                                .line_clamp(2)
                                                .child(user_preview),
                                        )
                                        .child(
                                            div()
                                                .text_sm()
                                                // 文本回复 80% 亮度，占位文案最暗档
                                                //（对齐 ZCode 的 popover-foreground/80
                                                // 与 foreground-subtle 分档）
                                                .text_color(if is_text {
                                                    cx.theme().foreground.opacity(0.8)
                                                } else {
                                                    cx.theme().muted_foreground
                                                })
                                                .line_clamp(3)
                                                .child(assistant_preview),
                                        ),
                                ),
                        )
                        .with_priority(1),
                    )
                },
            )
            .into_any_element()
    }

    /// 导航预览卡的助手摘要：该用户消息之后第一条助手消息的 Markdown 文本拼接
    ///（对齐 ZCode：assistantTextRows 合并、最多 2 段 220 字符）。
    /// 无文本时按流式状态给占位文案；返回的 bool 表示是否为真实回复文本。
    fn nav_assistant_preview(&self, ix: usize, user_ixs: &[usize]) -> (String, bool) {
        let running = self.streaming && user_ixs.last() == Some(&ix);
        let texts: Vec<&str> = self.messages[ix + 1..]
            .iter()
            .find(|message| message.role == Role::Assistant)
            .map(|message| {
                message
                    .segments
                    .iter()
                    .filter_map(|segment| match segment {
                        Segment::Markdown { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        if texts.is_empty() {
            return (
                if running {
                    "正在生成…"
                } else {
                    "（暂无文本回复）"
                }
                .to_string(),
                false,
            );
        }
        (nav_preview_text(&texts, "（暂无文本回复）"), true)
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
            // 导航跳转的 offset 在 prepaint 才更新，这一帧读到的还是旧位置；
            // 跳过本次恢复，下一帧按真实位置再判断
            if self.nav_jump {
                self.nav_jump = false;
            } else {
                self.follow_bottom = true;
            }
        }

        // turn 导航条：一条用户消息 = 一个 turn 入口。消息列表的每条消息都是
        // 滚动容器的直接子元素（见下），scroll_to_top_of_item / bounds_for_item
        // 只按直接子元素记录，因此可按消息下标精确定位
        let user_ixs: Vec<usize> = self
            .messages
            .iter()
            .enumerate()
            .filter(|(_, message)| message.role == Role::User)
            .map(|(ix, _)| ix)
            .collect();
        // 贴底 = 在读最新一轮：活动项恒为最后一条用户消息。底部视口里可能
        // 同时可见多条用户消息，按「离顶最近」会把高亮钉在更早的轮次上
        let nav_active = if user_ixs.len() >= 2 && self.at_bottom() {
            user_ixs.last().copied()
        // 活动项 = 离视口顶部最近的可见用户消息；都不可见时取视口顶之上最近
        // 的一条（对齐 ZCode resolveConversationTurnNavigatorActiveQueryRowId，
        // 不能用 topmost visible row：长回复的尾巴会把高亮钉在上一轮）
        } else if user_ixs.len() >= 2 {
            let container = self.scroll_handle.bounds();
            let scroll_top = container.top() - self.scroll_handle.offset().y;
            let scroll_bottom = scroll_top + container.size.height;
            let mut nearest_visible = None;
            let mut nearest_distance = f32::MAX;
            let mut last_above = None;
            let mut first_below = None;
            for &ix in &user_ixs {
                let Some(bounds) = self.scroll_handle.bounds_for_item(ix) else {
                    continue;
                };
                let (start, end) = (bounds.top(), bounds.bottom());
                if end >= scroll_top && start <= scroll_bottom {
                    let distance = f32::from(start - scroll_top).abs();
                    if distance < nearest_distance {
                        nearest_distance = distance;
                        nearest_visible = Some(ix);
                    }
                }
                if start <= scroll_top {
                    last_above = Some(ix);
                } else if first_below.is_none() {
                    first_below = Some(ix);
                }
            }
            nearest_visible
                .or(last_above)
                .or(first_below)
                .or(user_ixs.first().copied())
        } else {
            None
        };
        // 活动项变化时 rail 跟随滚动，保持活动横条可见
        if let Some(active) = nav_active
            && self.nav_last_active != Some(active)
        {
            self.nav_last_active = Some(active);
            if let Some(pos) = user_ixs.iter().position(|&ix| ix == active) {
                self.nav_rail_scroll.scroll_to_item(pos);
            }
        }
        // 首帧 paint 前面板宽度是零值：补一帧渲染让导航条出现；
        // paint 后该条件自愈，不会形成渲染循环
        let pane_width = self.scroll_handle.bounds().size.width;
        if user_ixs.len() >= 2 && pane_width <= px(0.) {
            cx.notify();
        }
        // 内容列宽度必须纯布局驱动：paint 测得的面板宽度在面板开合后要滞后一帧
        // 才更新，若用它算内容宽，每次开合面板内容列都会先按旧宽度错排一帧（抖动）。
        // 因此 gutter 只看 turn 数（≥2 轮 = 导航条可能出现就先占住两侧各 48px，
        // 对齐 ZCode w-[calc(100%-6rem)]），内容列恒为 min(860, 剩余宽度)。
        // 导航条本体的显隐仍看测量宽度（12px 小横条晚一帧出现不可感知）。
        let nav_eligible = user_ixs.len() >= 2;
        let pane_wide = pane_width >= px(720.);
        let content_max_w = px(860.);
        // 面板太窄时连 gutter 都留不出，隐藏导航条；turn 数 <2 也没有导航必要
        let show_nav = nav_eligible && pane_wide;

        v_flex()
            .size_full()
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        v_flex()
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
                            .gap_4()
                            .py_4()
                            // 每条消息（及流式指示/空状态）都是滚动容器的直接子行：
                            // 导航条按消息下标 scroll_to_top_of_item 依赖这一结构
                            .children(items.into_iter().map(|item| {
                                div()
                                    .w_full()
                                    .when(nav_eligible, |this| this.px_12())
                                    .child(
                                        div()
                                            .w_full()
                                            .max_w(content_max_w)
                                            .mx_auto()
                                            .px_4()
                                            .child(item),
                                    )
                                    .into_any_element()
                            }))
                            // 工作中指示：跟在最后一条消息之后，随对话一起滚动
                            .when(self.streaming, |this| {
                                this.child(
                                    div()
                                        .w_full()
                                        .when(nav_eligible, |this| this.px_12())
                                        .child(
                                            div()
                                                .w_full()
                                                .max_w(content_max_w)
                                                .mx_auto()
                                                .px_4()
                                                .child(
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
                                                                .text_color(
                                                                    cx.theme().muted_foreground,
                                                                ),
                                                        ),
                                                ),
                                        ),
                                )
                            })
                            .when(self.messages.is_empty(), |this| {
                                this.child(
                                    div()
                                        .w_full()
                                        .when(nav_eligible, |this| this.px_12())
                                        .child(
                                            div()
                                                .w_full()
                                                .max_w(content_max_w)
                                                .mx_auto()
                                                .px_4()
                                                .py_8()
                                                .text_center()
                                                .text_sm()
                                                .text_color(cx.theme().muted_foreground)
                                                .child(
                                                    "空会话。输入消息开始对话，/ 查看命令，@ 引用文件。",
                                                ),
                                        ),
                                )
                            }),
                    )
                    // turn 导航条：左缘竖排小横条，见 render_turn_nav
                    .when(show_nav, |this| {
                        this.child(self.render_turn_nav(&user_ixs, nav_active, cx))
                    })
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

/// token 数自动单位：<1k 原样；k/M 级整除显示整数、否则一位小数
fn fmt_tokens(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        let k = n as f64 / 1_000.0;
        if k.fract().abs() < 0.05 {
            format!("{}k", k.round() as u64)
        } else {
            format!("{k:.1}k")
        }
    } else {
        let m = n as f64 / 1_000_000.0;
        if m.fract().abs() < 0.05 {
            format!("{}M", m.round() as u64)
        } else {
            format!("{m:.1}M")
        }
    }
}

/// 回合脚注的 token 统计段：未缓存输入 · 缓存命中（命中率）· 输出 ·
/// 首字时间 · 解码速度（不含首字；旧记录无 api_ms 退回墙钟）
fn format_turn_stats(stats: &pig_protocol::TurnUsageStats) -> String {
    let total_input = stats.input + stats.cache_read;
    let hit_rate = if total_input > 0 {
        format!(
            " ({:.1}%)",
            stats.cache_read as f64 / total_input as f64 * 100.0
        )
    } else {
        String::new()
    };
    // 速度按纯解码时间算（API 总时长 − 首字等待，不含工具执行/审批等待）；
    // 旧记录无 api_ms 时退回墙钟时间
    let api_ms = if stats.api_ms > 0 {
        stats.api_ms
    } else {
        stats.duration_ms
    };
    // 平均首字 = 首字等待总和 ÷ 请求次数（多步回合一堆 TTFT 取平均；
    // 旧记录无 api_steps 时按一步算）
    let steps = stats.api_steps.max(1);
    let ttft = if stats.ttft_ms > 0 {
        format!(
            " · 首字 {:.1}s",
            stats.ttft_ms as f64 / steps as f64 / 1000.0
        )
    } else {
        String::new()
    };
    let decode_ms = api_ms.saturating_sub(stats.ttft_ms);
    let speed = if decode_ms > 0 {
        format!(
            " · {:.1} tok/s",
            stats.output as f64 / (decode_ms as f64 / 1000.0)
        )
    } else {
        String::new()
    };
    format!(
        " · 输入 {}（未缓存）· 命中 {}{hit_rate} · 输出 {}{ttft}{speed}",
        fmt_tokens(stats.input),
        fmt_tokens(stats.cache_read),
        fmt_tokens(stats.output),
    )
}

/// 导航预览卡文本：按空行分段、段内连续空白折叠为空格，取前 2 段以换行拼接，
/// 超 220 字符截断补「...」（对齐 ZCode conversationTurnNavigatorHelpers 的
/// buildPreviewText：maxPreviewChars 220 / maxPreviewParagraphs 2）
fn nav_preview_text(parts: &[&str], fallback: &str) -> String {
    let joined = parts.join("\n\n");
    let mut paragraphs: Vec<String> = Vec::new();
    let mut current = String::new();
    for line in joined.trim().lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        if words.is_empty() {
            if !current.is_empty() {
                paragraphs.push(std::mem::take(&mut current));
                if paragraphs.len() == 2 {
                    break;
                }
            }
        } else {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(&words.join(" "));
        }
    }
    if paragraphs.len() < 2 && !current.is_empty() {
        paragraphs.push(current);
    }
    if paragraphs.is_empty() {
        return fallback.to_string();
    }
    let text = paragraphs.join("\n");
    const MAX_CHARS: usize = 220;
    if text.chars().count() <= MAX_CHARS {
        text
    } else {
        let truncated: String = text.chars().take(MAX_CHARS - 3).collect();
        format!("{}...", truncated.trim_end())
    }
}

/// 滚动穿透：有滚动条（max_offset > 0，内容超出视口）时吞掉滚轮事件，不穿透到外层
/// 消息列表（这版 gpui 的内置滚动监听不阻断冒泡，不吞的话外层会联动，到顶/到底也不放行）；
/// 没有可滚空间时放行，滚轮直接滚动外层。
fn consume_scroll(
    handle: &ScrollHandle,
) -> impl Fn(&ScrollWheelEvent, &mut Window, &mut App) + 'static {
    let handle = handle.clone();
    move |_, _, cx| {
        if handle.max_offset().y > px(0.) {
            cx.stop_propagation();
        }
    }
}
