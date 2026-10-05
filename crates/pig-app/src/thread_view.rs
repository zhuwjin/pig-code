use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;

use gpui_kit::assets::IconName as AssetIconName;
use gpui_kit::base::{
    Align, ElementExt as _, Placement, Positioner, ScrollableMask, Scrollbar, ScrollbarMode,
    SelectableText, TextSelectionHandle,
};
use gpui_kit::component::Sizable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::shimmer::ShimmerText;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::text::{
    RangeHighlight, RenderedText, TextView, TextViewState, TextViewStyle,
};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use gpui_kit::{Overflow, StyleRefinement};
use pig_protocol::{ApprovalDecision, EditDiff, Event};

use crate::code_view::{
    CODE_LINE_H, CODE_SCROLLBAR_LANE, PreparedCode, code_line, code_line_row, gutter_width,
    highlight_code, lang_name_for_path, measure_max_line_width,
};

mod bash;
mod cards;
mod diffs;
mod lightbox;
mod messages;
mod model;
mod read;
mod reduce;
mod search;

use lightbox::*;
use messages::*;
use model::Role;
use model::*;
use read::*;

#[derive(Clone, Debug)]
pub enum ThreadEvent {
    /// 计划模式：用户点了「执行计划」
    ExecutePlan,
    /// 取消排队消息（文本匹配）
    CancelQueued(String),
    /// 点击后台子代理通知卡：打开右侧「子代理」tab（只读完整对话）
    OpenSubagent {
        agent_id: String,
        /// 展示标题（通知卡的 description）
        title: String,
    },
    /// 点击 Read 卡的路径：打开右侧「文件」tab 查看完整内容；
    /// line = Read 输出首行号（打开后滚动定位）
    OpenFile { path: String, line: Option<usize> },
    ApprovalReply {
        request_id: String,
        decision: ApprovalDecision,
    },
    /// 会话分叉：以该消息所在回合为止的历史派生新会话（turns = 保留回合数）
    Fork { turns: usize },
}

/// 会话内搜索的一次命中：定位到消息/段/段内字节区间。区间基于该段
/// rendered_text 的 UTF-8 字节偏移，与 set_range_highlights / reveal_range
/// 同一坐标系
#[derive(Clone)]
struct SearchMatch {
    msg_ix: usize,
    seg_ix: usize,
    range: Range<usize>,
}

/// 一段的上次搜索结果（见 ThreadView.search_cache）
struct SearchSegmentCache {
    snapshot: RenderedText,
    ranges: Vec<Range<usize>>,
}

/// 工作时长文案：「{prefix} N 秒」/「{prefix} M 分 S 秒」
///（运行中的「工作中」与折叠行的「已工作」共用）
pub(crate) fn fmt_work_duration(secs: u64, prefix: &str) -> String {
    if secs >= 60 {
        format!("{prefix} {} 分 {} 秒", secs / 60, secs % 60)
    } else {
        format!("{prefix} {secs} 秒")
    }
}

pub struct ThreadView {
    messages: Vec<ChatMessage>,
    item_index: HashMap<String, usize>,
    scroll_handle: ScrollHandle,
    /// 跟随模式：输出时自动贴底。用户上翻暂停跟随（浮出「最新消息」按钮），
    /// 回到底部（任意方式）或点击浮钮后恢复
    follow_bottom: bool,
    streaming: bool,
    /// 上下文压缩进行中（CompactStarted → ContextCompacted/TurnAborted 之间）：
    /// 列表末尾渲染「正在压缩上下文」分隔条
    compacting: bool,
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
    /// 预览卡的渲染数据快照（打开期间逐帧刷新；关闭一刻移入 nav_card_exit 播淡出，
    /// 也是「切换横条不重播入场动画」的判据）
    nav_card_last: Option<NavCardData>,
    /// 预览卡关闭时的出场快照（淡出动画播完即弃）
    nav_card_exit: Option<NavCardData>,
    /// 出场动画代次（进动画元素 id，每次关闭重播；清理计时器按代次作废）
    nav_card_exit_gen: u64,
    /// 各导航横条的屏幕 bounds（on_prepaint 记录），预览卡按它做侧边锚定；
    /// render 只持 &self，故用 RefCell
    nav_bar_bounds: RefCell<HashMap<usize, Rc<Cell<Bounds<Pixels>>>>>,
    /// 导航条自身的滚动句柄（turn 数超出可见高度时 rail 内部滚动）
    nav_rail_scroll: ScrollHandle,
    /// 上一帧的活动导航项；活动项变化时让 rail 滚动到可见
    nav_last_active: Option<usize>,
    /// 子代理完成次序计数器（SubagentActivity finished 逐个 +1，写入代理卡的
    /// finished_seq；Swarm 面板按完成先后排序用）。随 clear 重置
    agent_finish_seq: u64,
    /// 导航点击后抑制一次「回到底部自动恢复跟随」：跳转滚动在 prepaint 才生效，
    /// 生效前 offset 仍是旧值，贴着底会被误判成用户滚回了底部
    nav_jump: bool,
    /// 本会话的媒体目录（{data}/sessions/{id}.media）：用户消息图片缩略图来源；
    /// None/目录不存在 → 附件链接整体按原文本降级显示
    media_dir: Option<std::path::PathBuf>,
    /// 图片灯箱覆盖层（点用户消息缩略图打开；Esc/点遮罩/关闭钮关闭）
    lightbox: Option<Lightbox>,
    /// 灯箱的焦点 handle（Esc 键监听挂在卡片上）
    lightbox_focus: FocusHandle,
    /// 会话内搜索条是否打开（Ctrl+F / Esc）
    search_open: bool,
    /// 搜索输入框：首开时惰性创建（InputState::new 需要 Window，
    /// ThreadView::new 拿不到——ensure_views 在事件处理链里没有 Window 可传）；
    /// 创建后跨开关复用，关闭只清值。Subscription 随元组存放保活
    search_input: Option<(Entity<InputState>, Subscription)>,
    /// 当前命中是为哪个 query 算出的（输入框原文；匹配时双方再小写化）
    search_query: String,
    /// 全部命中，按消息/段/区间起点顺序
    search_matches: Vec<SearchMatch>,
    /// 活动命中下标（goto_match 前进/回绕；计数显示 active+1/total）
    active_match: usize,
    /// 每段的搜索缓存（key = 段 TextViewState 的 EntityId）：上次搜索时的
    /// 渲染快照 + 该段命中区间。RenderedText 的 PartialEq 按 (owner, revision)
    /// 比较——revision 没变 = 内容没变，同 query 重跑（流式 TextDone、重复
    /// Ctrl+F）时直接复用命中区间，只对内容变了的段重新查找
    search_cache: HashMap<EntityId, SearchSegmentCache>,
    _ticker: Task<()>,
}

impl EventEmitter<ThreadEvent> for ThreadView {}

/// 自测用：导航条活动项排查数据（nav_last_active, offset_y, max_offset_y,
/// 容器高, 各用户消息行的 [top, bottom) 内容坐标）
type NavActiveDetail = (Option<usize>, f32, f32, f32, Vec<(usize, f32, f32)>);

/// 自测用：后台子代理通知 meta（agent_id, 标题, 耗时毫秒, 记录路径, 结果路径）
type TaskNotificationMeta = (String, String, Option<u64>, Option<String>, Option<String>);

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
            compacting: false,
            plan_pending: false,
            turn_started: None,
            replay_turn: false,
            queued: Vec::new(),
            nav_hover: None,
            nav_card: None,
            nav_card_last: None,
            nav_card_exit: None,
            nav_card_exit_gen: 0,
            nav_bar_bounds: RefCell::new(HashMap::new()),
            nav_rail_scroll: ScrollHandle::new(),
            nav_last_active: None,
            agent_finish_seq: 0,
            nav_jump: false,
            media_dir: None,
            lightbox: None,
            lightbox_focus: cx.focus_handle(),
            search_open: false,
            search_input: None,
            search_query: String::new(),
            search_matches: Vec::new(),
            active_match: 0,
            search_cache: HashMap::new(),
            _ticker: ticker,
        }
    }

    /// 绑定会话媒体目录（ensure_views 创建时调用）：图片附件缩略图的文件来源
    pub fn set_media_dir(&mut self, dir: std::path::PathBuf) {
        self.media_dir = Some(dir);
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
        // 事件文本末尾的附件链接 → 缩略图；media 目录不可用（没附过图片的会话等）
        // → 不拆分，原文整体保留（链接降级为文本显示）
        let media_ready = self.media_dir.as_ref().is_some_and(|dir| dir.is_dir());
        let (body, indices) = if media_ready {
            split_image_links(&text)
        } else {
            (text, vec![])
        };
        let mut message = ChatMessage::user(body, files);
        message.images = indices
            .into_iter()
            .map(|n| self.load_user_image(n))
            .collect();
        self.messages.push(message);
        // 用户自己发消息：强制回到底部并恢复跟随
        self.follow_bottom = true;
        self.auto_scroll();
        cx.notify();
    }

    /// 按序号加载媒体文件 `{N}.{ext}` → 缩略图数据；丢失/坏字节 → thumb None（降级 chip）
    fn load_user_image(&self, n: u32) -> UserImage {
        let missing = || UserImage {
            thumb: None,
            dims: (0, 0),
        };
        let Some(dir) = &self.media_dir else {
            return missing();
        };
        let file = ["png", "jpg"]
            .into_iter()
            .map(|ext| dir.join(format!("{n}.{ext}")))
            .find(|path| path.exists());
        let Some(file) = file else {
            return missing();
        };
        let Ok(bytes) = std::fs::read(&file) else {
            return missing();
        };
        let Some(dims) = pig_core::tool::decode_image_check(&bytes) else {
            return missing();
        };
        let format = match pig_core::tool::sniff_image(&bytes) {
            Some("image/jpeg") => ImageFormat::Jpeg,
            _ => ImageFormat::Png,
        };
        UserImage {
            thumb: Some(std::sync::Arc::new(Image {
                format,
                bytes,
                id: gpui_kit::hash(&file),
            })),
            dims,
        }
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

    /// 自测用：导航条活动项排查（字段见 [`NavActiveDetail`]）
    pub fn debug_nav_active_detail(&self) -> NavActiveDetail {
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
        self.compacting = false;
        self.nav_hover = None;
        self.nav_card = None;
        self.nav_card_last = None;
        self.nav_card_exit = None;
        self.nav_bar_bounds.borrow_mut().clear();
        self.nav_last_active = None;
        self.nav_jump = false;
        self.agent_finish_seq = 0;
        self.lightbox = None;
        // 搜索命中/缓存随消息一并失效（高亮挂在段上，随段释放）；
        // 搜索条本身与 query 保留，回放重建经 TextDone 重跑
        self.search_matches.clear();
        self.search_cache.clear();
        self.active_match = 0;
        cx.notify();
    }

    pub fn add_system_note(&mut self, text: &str, cx: &mut Context<Self>) {
        self.messages.push(ChatMessage::system(text.to_string()));
        self.auto_scroll();
        cx.notify();
    }

    /// 压缩完成：分隔条样式（渲染为「🗄 上下文已压缩」，摘要全文留在 text 供自测断言）
    pub fn add_compact_note(&mut self, note: &str, cx: &mut Context<Self>) {
        self.messages.push(ChatMessage::system_with_kind(
            note.to_string(),
            SystemNoteKind::Compacted,
        ));
        self.auto_scroll();
        cx.notify();
    }

    /// 压缩进行中标记：true → 列表末尾渲染「正在压缩上下文」分隔条
    pub fn set_compacting(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.compacting == on {
            return;
        }
        self.compacting = on;
        self.auto_scroll();
        cx.notify();
    }

    /// 供自测断言用：压缩进行中标记。
    pub fn debug_compacting(&self) -> bool {
        self.compacting
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

    /// 自测用：展开最近一张指定工具的工具卡（展开代码卡渲染路径），返回是否找到。
    /// 展开后滚回底部：卡片加高会把视口顶离底部，follow_bottom 语义下保持贴底
    pub fn debug_expand_tool(&mut self, tool: &str, cx: &mut Context<Self>) -> bool {
        let found = self
            .messages
            .iter_mut()
            .rev()
            .flat_map(|m| m.segments.iter_mut())
            .find_map(|s| match s {
                Segment::ToolCall {
                    tool: t, expanded, ..
                } if t.as_str() == tool => {
                    *expanded = true;
                    Some(())
                }
                _ => None,
            })
            .is_some();
        if found {
            self.follow_bottom = true;
            self.scroll_handle.scroll_to_bottom();
            // 开合动画期间内容持续长高，贴底标记若应用在动画半途会停在半路——
            // 动画结束后再补一次贴底
            cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(EXPAND_ANIM_DUR + std::time::Duration::from_millis(50))
                    .await;
                this.update(cx, |this, _| {
                    if this.follow_bottom {
                        this.scroll_handle.scroll_to_bottom();
                    }
                })
                .ok();
            })
            .detach();
            cx.notify();
        }
        found
    }

    /// 首张 Agent/AgentSwarm 工具卡片的 (summary, live_note, done)（自测用）。
    pub fn debug_agent_card(&self) -> Option<(String, Option<String>, bool)> {
        self.messages
            .iter()
            .flat_map(|m| &m.segments)
            .find_map(|s| match s {
                Segment::ToolCall {
                    tool,
                    summary,
                    live_note,
                    done,
                    ..
                } if tool == "Agent" || tool == "AgentSwarm" => {
                    Some((summary.clone(), live_note.clone(), *done))
                }
                _ => None,
            })
    }

    /// 是否出现过后台子代理的合成通知用户消息（自测用）。
    pub fn debug_has_task_notification(&self) -> bool {
        self.messages
            .iter()
            .any(|m| m.role == Role::User && as_task_notification(&m.text).is_some())
    }

    /// 最近一条后台子代理通知的 meta（字段见 [`TaskNotificationMeta`]；自测用，
    /// 无通知/通知缺 agent_id 为 None）。标题 = description（缺省回退「后台子代理」），
    /// 与气泡渲染同口径。
    pub fn debug_task_notification_meta(&self) -> Option<TaskNotificationMeta> {
        self.messages.iter().rev().find_map(|m| {
            if m.role != Role::User {
                return None;
            }
            let note = as_task_notification(&m.text)?;
            let title = note
                .description
                .filter(|d| !d.is_empty())
                .unwrap_or_else(|| "后台子代理".to_string());
            Some((
                note.agent_id?,
                title,
                note.duration_ms,
                note.record,
                note.result,
            ))
        })
    }

    /// 首张代理卡的 (agent_id, 副标题文本)（自测用；无 SubagentCard 元信息为 None）。
    /// 副标题 = `{profile} · {model}`，与代理卡渲染同口径。
    pub fn debug_agent_card_meta(&self) -> Option<(String, String)> {
        self.messages
            .iter()
            .flat_map(|m| &m.segments)
            .find_map(|s| match s {
                Segment::ToolCall { agent_cards, .. } => agent_cards.first().map(|card| {
                    (
                        card.agent_id.clone(),
                        format!("{} · {}", card.profile, card.model),
                    )
                }),
                _ => None,
            })
    }

    /// 最近一张代理卡的 (agent_id, done, finished)（自测用：验证后台卡
    /// 运行态机——工具收尾≠子代理结束）。无代理卡为 None。
    pub fn debug_agent_card_state(&self) -> Option<(String, bool, bool)> {
        self.messages
            .iter()
            .rev()
            .flat_map(|m| m.segments.iter().rev())
            .find_map(|s| match s {
                Segment::ToolCall {
                    agent_cards, done, ..
                } => agent_cards
                    .last()
                    .map(|card| (card.agent_id.clone(), *done, card.finished)),
                _ => None,
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

    /// 按 request_id 定向决议审批卡（审批条路径）：并发审批排队时各笔
    /// 各答各的，不受到达顺序影响；id 不在（已决议/迟到事件）则无操作。
    pub fn decide_approval_by_id(
        &mut self,
        request_id: &str,
        decision: ApprovalDecision,
        cx: &mut Context<Self>,
    ) -> bool {
        let found = self.messages.iter().enumerate().find_map(|(mix, m)| {
            m.segments.iter().enumerate().find_map(|(six, s)| match s {
                Segment::Approval {
                    request_id: id,
                    decision: None,
                } if id == request_id => Some((mix, six)),
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

    /// 段级（Thinking/ToolCall/TurnChanges）或 turn 改动面板文件行级
    ///（row_ix = Some）的开合动画态
    pub(crate) fn expand_anim_at(
        &mut self,
        message_ix: usize,
        segment_ix: usize,
        row_ix: Option<usize>,
    ) -> Option<&mut ExpandAnim> {
        let segment = self
            .messages
            .get_mut(message_ix)?
            .segments
            .get_mut(segment_ix)?;
        match row_ix {
            Some(rix) => match segment {
                Segment::TurnChanges { rows, .. } => {
                    rows.get_mut(rix).map(|row| &mut row.expand_anim)
                }
                _ => None,
            },
            None => match segment {
                Segment::Thinking { expand_anim, .. }
                | Segment::ToolCall { expand_anim, .. }
                | Segment::TurnChanges { expand_anim, .. } => Some(expand_anim),
                _ => None,
            },
        }
    }

    /// 开合切换的动画驱动：gen+1 重播动画；收起时进入 collapsing（内容保持
    /// 挂载播滑收），计时器到期卸载——期间又展开的代次不符自动作废
    pub(crate) fn drive_expand_anim(
        &mut self,
        message_ix: usize,
        segment_ix: usize,
        row_ix: Option<usize>,
        expanded_now: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(anim) = self.expand_anim_at(message_ix, segment_ix, row_ix) else {
            return;
        };
        anim.generation += 1;
        anim.collapsing = !expanded_now;
        if expanded_now {
            return;
        }
        let generation = anim.generation;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(EXPAND_ANIM_DUR + std::time::Duration::from_millis(50))
                .await;
            this.update(cx, |this, cx| {
                if let Some(anim) = this.expand_anim_at(message_ix, segment_ix, row_ix)
                    && anim.collapsing
                    && anim.generation == generation
                {
                    anim.collapsing = false;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// 展开/收起内容的动画包装：展开 = 内容从 0 高滑开 + 淡入；收起 = 保持挂载
    /// 滑收淡出（卸载见 drive_expand_anim 的计时器）。实现见 crate::anim（侧栏
    /// 工作区开合同款共用）；id 含 gen，每次开合重播
    pub(crate) fn expand_anim_wrap(
        &self,
        id: String,
        anim: &ExpandAnim,
        content: AnyElement,
    ) -> AnyElement {
        crate::anim::expand_anim_wrap(id, anim, content)
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
            // 通知卡 UI 态惰性创建 + 结果文件大小探测（每条消息只做一次，避免每帧 stat）
            if message.notification_ui.is_none()
                && let Some(note) = as_task_notification(&message.text)
            {
                // 文件大小探测的目标：result.md（core 产物恒带 result 属性）
                let record_size = note
                    .result
                    .as_ref()
                    .map(|path| std::fs::metadata(path).ok().map(|m| m.len()));
                message.notification_ui = Some(NotificationUi {
                    payload_open: false,
                    copied: false,
                    record_size,
                    payload_scroll: ScrollHandle::new(),
                });
            }
            // Read/Bash 工具卡的 UI 态惰性创建（换行/复制/高亮缓存；render 路径只读）
            for segment in &mut message.segments {
                match segment {
                    Segment::ToolCall { tool, read_ui, .. }
                        if tool == "Read" && read_ui.is_none() =>
                    {
                        *read_ui = Some(ReadCardUi::new());
                    }
                    Segment::ToolCall { tool, bash_ui, .. }
                        if tool == "Bash" && bash_ui.is_none() =>
                    {
                        *bash_ui = Some(BashCardUi::new());
                    }
                    _ => {}
                }
            }
        }
        let mut items = Vec::with_capacity(self.messages.len());
        for ix in 0..self.messages.len() {
            items.push(self.render_message(ix, window, cx));
        }

        let working_secs = self
            .turn_started
            .map(|t| t.elapsed().as_secs())
            .unwrap_or(0);
        let working_label = fmt_work_duration(working_secs, "工作中");

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
            // 会话内搜索条：消息列表之上的固定行（Ctrl+F 打开）
            .when(self.search_open, |this| {
                this.when_some(self.render_search_bar(cx), ParentElement::child)
            })
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
                            // 用户上翻：暂停跟随并浮出「最新消息」按钮（不吞事件，列表照常滚动）。
                            // 内容没超高（max_offset=0，不可滚动）时上翻无意义——保持跟随，
                            // 否则短会话里滚一下也会浮出按钮
                            .on_scroll_wheel(cx.listener(
                                |this, event: &ScrollWheelEvent, window, cx| {
                                    let delta = event.delta.pixel_delta(window.line_height());
                                    if delta.y > px(0.)
                                        && this.follow_bottom
                                        && this.scroll_handle.max_offset().y > px(0.)
                                    {
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
                            // 压缩进行中分隔条：同挂列表末尾（自动压缩发生在回合中，
                            // 与工作中指示可同时出现，分隔条排最后 = 最新状态）
                            .when(self.compacting, |this| {
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
                                                .child(render_compact_divider(
                                                    ShimmerText::new("正在压缩上下文")
                                                        .id("compacting-shimmer")
                                                        .text_sm()
                                                        .text_color(cx.theme().foreground)
                                                        .into_any_element(),
                                                    cx,
                                                ))
                                                .id("compacting-divider")
                                                .test_support(),
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
                                        // 默认 hitbox 不拦下层：点击会穿透到下面的
                                        // 工具卡/滚动区——挡掉穿透，滚轮仍透传给列表
                                        .block_mouse_except_scroll()
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
                    })
                    // 图片灯箱：覆盖消息区（最后渲染 = 最顶层）
                    .when(self.lightbox.is_some(), |this| {
                        this.child(self.render_lightbox(window, cx))
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

#[cfg(test)]
mod tests;
