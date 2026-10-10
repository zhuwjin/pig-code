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
mod search_card;

use lightbox::*;
use messages::*;
use model::Role;
use model::*;
use read::*;
use search_card::*;

#[derive(Clone, Debug)]
pub enum ThreadEvent {
    /// Cancel a queued message (matched by text)
    CancelQueued(String),
    /// Click on a background subagent notification card: opens the right-side "subagent" tab (read-only full conversation)
    OpenSubagent {
        agent_id: String,
        /// Display title (the notification card's description)
        title: String,
    },
    /// Click on the Read card's path: opens the right-side "file" tab to view the full content;
    /// line = the first line number of the Read output (scrolled to after opening)
    OpenFile { path: String, line: Option<usize> },
    ApprovalReply {
        request_id: String,
        decision: ApprovalDecision,
        /// Feedback (kimi Revise: carried when the plan's "Revise" is submitted; None for other approvals)
        feedback: Option<String>,
    },
    /// Click on the compact divider's "view summary" link: opens the right-side
    /// "compact summary" tab (the clicked compaction point's own summary text)
    OpenCompactSummary { text: String },
    /// Click on the retry-wait row's "Retry now" pill: skips the session's
    /// remaining retry wait
    RetryNow,
    /// Session fork: derives a new session from the history ending at this message's turn (turns = the number of turns kept)
    Fork { turns: usize },
}

/// One in-session search hit: locates a message/segment/byte range within the segment. The range is based on that segment's
/// rendered_text UTF-8 byte offsets, the same coordinate system as set_range_highlights / reveal_range
#[derive(Clone)]
struct SearchMatch {
    msg_ix: usize,
    seg_ix: usize,
    range: Range<usize>,
}

/// A segment's previous search result (see ThreadView.search_cache)
struct SearchSegmentCache {
    snapshot: RenderedText,
    ranges: Vec<Range<usize>>,
}

/// Work duration label: "{prefix} N seconds" / "{prefix} M minutes S seconds"
/// (shared by the running "Working" and the collapsed row's "Worked")
pub(crate) fn fmt_work_duration(secs: u64, prefix: &str) -> String {
    if secs >= 60 {
        rust_i18n::t!(
            "thread.work_duration_minutes",
            prefix = prefix,
            m = secs / 60,
            s = secs % 60
        )
        .to_string()
    } else {
        rust_i18n::t!("thread.work_duration_seconds", prefix = prefix, s = secs).to_string()
    }
}

pub struct ThreadView {
    messages: Vec<ChatMessage>,
    item_index: HashMap<String, usize>,
    scroll_handle: ScrollHandle,
    /// Follow mode: automatically sticks to the bottom during output. The user scrolling up pauses following (a "Latest messages" button pops up),
    /// and reaching the bottom (by any means) or clicking the floating button resumes it
    follow_bottom: bool,
    /// The list offset seen by the previous render: an upward move between frames pauses following
    /// (covers wheel-up, dragging the scrollbar thumb, a track click, keyboard — any means).
    /// Content growth only ever pushes the offset down and a shrink clamps to the still-bottom
    /// position, so an upward delta reliably means user intent
    last_scroll_y: Pixels,
    streaming: bool,
    /// Context compaction in progress (between CompactStarted → ContextCompacted/TurnAborted):
    /// renders a "Compacting context" divider at the end of the list
    compacting: bool,
    /// Live retry-wait state (Event::RetryStatus, superseded by any next event):
    /// transient render-only data, never persisted
    retrying: Option<pig_protocol::RetryStatus>,
    turn_started: Option<std::time::Instant>,
    /// The current turn was rebuilt by replay (turn_id starts with replay-): thinking segments get no real duration
    replay_turn: bool,
    /// Queued messages (FIFO)
    queued: Vec<String>,
    /// Turn nav: the hovered user message index (drives the bars' mountain-style widening and highlight)
    nav_hover: Option<usize>,
    /// Which message the preview card is currently open for (opens after a stable 120ms hover, closes 80ms after leaving)
    nav_card: Option<usize>,
    /// Render data snapshot of the preview card (refreshed frame by frame while open; moved into nav_card_exit at the moment of closing to play the fade-out,
    /// and also the criterion for "switching bars does not replay the enter animation")
    nav_card_last: Option<NavCardData>,
    /// Exit snapshot when the preview card closes (dropped once the fade-out animation finishes)
    nav_card_exit: Option<NavCardData>,
    /// Exit animation generation (goes into the animation element id, replays on each close; the cleanup timer is invalidated by generation)
    nav_card_exit_gen: u64,
    /// Screen bounds of each nav bar (recorded on_prepaint); the preview card side-anchors to them;
    /// render holds only &self, hence the RefCell
    nav_bar_bounds: RefCell<HashMap<usize, Rc<Cell<Bounds<Pixels>>>>>,
    /// The nav rail's own scroll handle (the rail scrolls internally when turns exceed the visible height)
    nav_rail_scroll: ScrollHandle,
    /// The previous frame's active nav item; when the active item changes, the rail scrolls to keep it visible
    nav_last_active: Option<usize>,
    /// Subagent finish sequence counter (SubagentActivity finished bumps it once each, written into the agent card's
    /// finished_seq; used by the Swarm panel to order by finish time). Reset on clear
    agent_finish_seq: u64,
    /// After a nav click, suppress the "back-to-bottom auto-resume follow" once: the jump scroll only takes effect at prepaint,
    /// and before that the offset is still the old value, which would be misread as the user scrolling back to the bottom
    nav_jump: bool,
    /// This session's media directory ({data}/sessions/{id}.media): the source of user message image thumbnails;
    /// None or a missing directory → attachment links degrade to plain text as a whole
    media_dir: Option<std::path::PathBuf>,
    /// Image lightbox overlay (opened by clicking a user message thumbnail; closed by Esc/clicking the mask/the close button)
    lightbox: Option<Lightbox>,
    /// The lightbox's focus handle (the Esc key listener is attached to the card)
    lightbox_focus: FocusHandle,
    /// Whether the in-session search bar is open (Ctrl+F / Esc)
    search_open: bool,
    /// Search input: lazily created on first open (InputState::new needs a Window,
    /// which ThreadView::new cannot get, since ensure_views has no Window to pass on the event-handling chain);
    /// reused across open/close once created, and closing only clears the value. The Subscription stays alive stored in the tuple
    search_input: Option<(Entity<InputState>, Subscription)>,
    /// Which query the current matches were computed for (the input's raw text; both sides are lowercased when matching)
    search_query: String,
    /// All matches, ordered by message/segment/range start
    search_matches: Vec<SearchMatch>,
    /// Active match index (goto_match advances/wraps; the counter shows active+1/total)
    active_match: usize,
    /// Per-segment search cache (key = the segment TextViewState's EntityId): the render
    /// snapshot from the last search + that segment's match ranges. RenderedText's PartialEq compares by (owner, revision);
    /// an unchanged revision = unchanged content, so re-running the same query (streaming TextDone, repeated
    /// Ctrl+F) reuses the match ranges directly and only re-searches segments whose content changed
    search_cache: HashMap<EntityId, SearchSegmentCache>,
    /// Hovered Glob/Grep result row (message_ix, segment_ix, row_ix): drives the
    /// row's blue text highlight (written by the row's on_hover listener;
    /// group_hover proved unreliable on these deeply nested rows)
    search_row_hover: Option<(usize, usize, usize)>,
    _ticker: Task<()>,
}

impl EventEmitter<ThreadEvent> for ThreadView {}

/// Retry-wait row label, built at draw time from the structured status (a
/// language switch re-renders correctly): "Retrying (2/10), in 5s · rate
/// limited". Reason details (network root cause, in-band text) stay in the
/// eventual Error note; the row carries the kind only.
fn retry_wait_label(status: &pig_protocol::RetryStatus) -> String {
    use pig_protocol::RetryReason;
    let mut label = rust_i18n::t!(
        "thread.retrying",
        attempt = status.attempt,
        max = status.max_attempts
    )
    .to_string();
    let secs = status.delay_ms / 1000;
    if secs > 0 {
        label.push_str(rust_i18n::t!("thread.retry_after", seconds = secs).as_ref());
    }
    let reason = match &status.reason {
        RetryReason::RateLimit { .. } => rust_i18n::t!("thread.retry_reason.rate_limit"),
        RetryReason::Server(code) => rust_i18n::t!("thread.retry_reason.server", status = code),
        RetryReason::Network { .. } => rust_i18n::t!("thread.retry_reason.network"),
        RetryReason::EmptyCompletion => rust_i18n::t!("thread.retry_reason.empty"),
        RetryReason::InBand { .. } => rust_i18n::t!("thread.retry_reason.inband"),
    };
    label.push_str(" · ");
    label.push_str(reason.as_ref());
    label
}

/// For self-tests: nav active-item diagnostics data (nav_last_active, offset_y, max_offset_y,
/// container height, each user message row's [top, bottom) content coordinates)
type NavActiveDetail = (Option<usize>, f32, f32, f32, Vec<(usize, f32, f32)>);

/// For self-tests: background subagent notification meta (agent_id, title, elapsed ms, record path, result path)
type TaskNotificationMeta = (String, String, Option<u64>, Option<String>, Option<String>);

impl ThreadView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        // Tick every second: drives the "Working N seconds" timer refresh
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
            last_scroll_y: px(0.),
            streaming: false,
            compacting: false,
            retrying: None,
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
            search_row_hover: None,
            _ticker: ticker,
        }
    }

    /// Bind the session media directory (called by ensure_views at creation): the file source of image attachment thumbnails
    pub fn set_media_dir(&mut self, dir: std::path::PathBuf) {
        self.media_dir = Some(dir);
    }

    fn set_streaming(&mut self, streaming: bool, _cx: &mut Context<Self>) {
        self.streaming = streaming;
        self.turn_started = streaming.then(std::time::Instant::now);
    }

    /// The current scroll offset clamped into the valid range [-max.y, 0]: gpui's wheel handler adds
    /// the delta to the offset immediately and only the next prepaint clamps it, so a raw read can sit
    /// outside the range for one frame (overscroll flick, or wheeling on a list that cannot scroll at
    /// all — which would otherwise read as a phantom upward move and pop the "Latest messages" button)
    fn clamped_offset_y(&self) -> Pixels {
        let max_y = self.scroll_handle.max_offset().y;
        self.scroll_handle.offset().y.clamp(-max_y, px(0.))
    }

    /// Whether currently at the bottom (offset.y ∈ [-max.y, 0], distance to bottom = offset.y + max.y)
    fn at_bottom(&self) -> bool {
        self.clamped_offset_y() + self.scroll_handle.max_offset().y <= px(2.)
    }

    /// Auto-scroll during output: stick to the bottom only in follow mode; do not disturb after the user scrolls up
    fn auto_scroll(&mut self) {
        if self.follow_bottom {
            self.scroll_handle.scroll_to_bottom();
        }
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// For self-tests.
    pub fn debug_queued(&self) -> &[String] {
        &self.queued
    }

    pub fn is_streaming(&self) -> bool {
        self.streaming
    }

    #[allow(dead_code)] // Debug only
    pub fn message_count(&self) -> usize {
        self.messages.len()
    }

    pub fn append_user_message(
        &mut self,
        text: String,
        files: Vec<String>,
        image_nums: Vec<u32>,
        cx: &mut Context<Self>,
    ) {
        // Thumbnails load directly by media index (image_nums); when the media directory is unavailable (sessions that never
        // attached images, etc.) load_user_image returns the default and each image degrades to an "expired" chip
        let mut message = ChatMessage::user(text, files);
        message.images = image_nums
            .into_iter()
            .map(|n| self.load_user_image(n))
            .collect();
        self.messages.push(message);
        // The user sent a message themselves: force back to the bottom and resume following
        self.follow_bottom = true;
        self.auto_scroll();
        cx.notify();
    }

    /// Load the media file `{N}.{ext}` by index → thumbnail data; missing/corrupt bytes → thumb None (fallback chip)
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
        let Some(dims) = pig_utils::image::decode_image_check(&bytes) else {
            return missing();
        };
        let format = match pig_utils::image::sniff_image(&bytes) {
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

    /// For self-tests: the turn nav visibility condition (user message count, message panel width px)
    pub fn debug_nav_state(&self) -> (usize, f32) {
        let turns = self
            .messages
            .iter()
            .filter(|m| m.role == Role::User)
            .count();
        (turns, f32::from(self.scroll_handle.bounds().size.width))
    }

    /// For self-tests: nav active-item diagnostics (fields in [`NavActiveDetail`])
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
        self.retrying = None;
        self.nav_hover = None;
        self.nav_card = None;
        self.nav_card_last = None;
        self.nav_card_exit = None;
        self.nav_bar_bounds.borrow_mut().clear();
        self.nav_last_active = None;
        self.nav_jump = false;
        self.agent_finish_seq = 0;
        self.lightbox = None;
        // Search matches/cache are invalidated along with the messages (highlights hang on segments and are released with them);
        // the search bar itself and the query survive, and replay rebuilds re-run via TextDone
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

    /// Core error note: stores the structured error and renders "⚠ {localized
    /// text}" at draw time (a language switch updates already-shown notes too)
    pub fn add_error_note(&mut self, error: &pig_protocol::CoreError, cx: &mut Context<Self>) {
        self.messages.push(ChatMessage::system_with_kind(
            String::new(),
            SystemNoteKind::Error(error.clone()),
        ));
        self.auto_scroll();
        cx.notify();
    }

    /// Compaction finished: divider style ("context compacted (Nk → Mk tokens) · view summary"; the full summary stays in text for self-test assertions)
    pub fn add_compact_note(
        &mut self,
        note: &str,
        used_before: Option<u64>,
        used_after: Option<u64>,
        summary: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.messages.push(ChatMessage::system_with_kind(
            note.to_string(),
            SystemNoteKind::Compacted {
                used_before,
                used_after,
                summary,
            },
        ));
        self.auto_scroll();
        cx.notify();
    }

    /// Compaction-in-progress flag: true → renders a "Compacting context" divider at the end of the list
    pub fn set_compacting(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.compacting == on {
            return;
        }
        self.compacting = on;
        self.auto_scroll();
        cx.notify();
    }

    /// For self-test assertions: the compaction-in-progress flag.
    pub fn debug_compacting(&self) -> bool {
        self.compacting
    }

    /// For self-test assertions: all system note texts.
    pub fn debug_system_notes(&self) -> Vec<String> {
        self.messages
            .iter()
            .filter(|m| m.role == Role::System)
            .map(|m| m.text.clone())
            .collect()
    }

    /// For self-test assertions.
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
                Segment::TurnChanges { .. } | Segment::Approval { .. } | Segment::Plan { .. } => {}
            }
        }
        (tool_done, text, thinking, tool_output)
    }

    /// Whether a tool card of the given tool ever appeared in any message (for self-tests).
    pub fn debug_has_tool_call(&self, tool: &str) -> bool {
        self.messages.iter().any(|m| {
            m.segments.iter().any(|s| match s {
                Segment::ToolCall { tool: name, .. } => name == tool,
                _ => false,
            })
        })
    }

    /// For self-test stall diagnostics: a compact per-message layout summary
    /// (role + segment kinds with ToolCall done/error flags) so a timeout
    /// assert can print what actually landed instead of a bare "timed out".
    pub fn debug_layout(&self) -> Vec<String> {
        self.messages
            .iter()
            .map(|m| {
                let segs: Vec<String> = m
                    .segments
                    .iter()
                    .map(|s| match s {
                        Segment::Thinking { text, .. } => {
                            format!("Thinking({}ch)", text.chars().count())
                        }
                        Segment::Markdown { text, .. } => {
                            format!("Markdown({}ch)", text.chars().count())
                        }
                        Segment::ToolCall {
                            tool,
                            done,
                            is_error,
                            ..
                        } => format!("Tool({tool},done={done},err={is_error})"),
                        Segment::Approval { .. } => "Approval".into(),
                        Segment::Plan { .. } => "Plan".into(),
                        Segment::TurnChanges { rows, .. } => {
                            format!("TurnChanges({}rows)", rows.len())
                        }
                    })
                    .collect();
                let role = match m.role {
                    Role::User => {
                        format!("User({:?})", m.text.chars().take(50).collect::<String>())
                    }
                    Role::Assistant => "Assistant".to_string(),
                    Role::System => "System".to_string(),
                };
                format!("{role}[{}]", segs.join(","))
            })
            .collect()
    }

    /// For self-tests: expand the most recent tool card of the given tool (exercising the code-card render path), returning whether one was found.
    /// After expanding, scroll back to the bottom: the card's extra height pushes the viewport off the bottom, and follow_bottom semantics keep it pinned
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
            // During the expand/collapse animation the content keeps growing taller; if the stick-to-bottom flag
            // is applied mid-animation it stops halfway, so pin to the bottom once more after the animation ends
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

    /// The first Agent/AgentSwarm tool card's (summary, live_note text, done) (for
    /// self-tests). The live_note is the segment-level one (foreground progress,
    /// core-provided raw text); background cards' notes live on the card.
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

    /// Whether a background subagent's synthetic notification user message ever appeared (for self-tests).
    pub fn debug_has_task_notification(&self) -> bool {
        self.messages
            .iter()
            .any(|m| m.role == Role::User && as_task_notification(&m.text).is_some())
    }

    /// The most recent background subagent notification's meta (fields in [`TaskNotificationMeta`]; for self-tests,
    /// None with no notification or a notification missing agent_id). Title = description (falling back to "background subagent"),
    /// the same convention as the bubble rendering.
    pub fn debug_task_notification_meta(&self) -> Option<TaskNotificationMeta> {
        self.messages.iter().rev().find_map(|m| {
            if m.role != Role::User {
                return None;
            }
            let note = as_task_notification(&m.text)?;
            let title = note
                .description
                .filter(|d| !d.is_empty())
                .unwrap_or_else(|| rust_i18n::t!("thread.bg_subagent").to_string());
            Some((
                note.agent_id?,
                title,
                note.duration_ms,
                note.record,
                note.result,
            ))
        })
    }

    /// The first agent card's (agent_id, subtitle text) (for self-tests; None without SubagentCard metadata).
    /// Subtitle = `{profile} · {model}`, the same convention as the agent card rendering.
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

    /// The most recent agent card's (agent_id, done, finished) (for self-tests: verifies the background card
    /// run-state machine; tool-call end ≠ subagent end). None without agent cards.
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

    /// The currently pending approval's request_id (for self-tests).
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

    /// Decide a pending approval card through the same path as clicking the button (for self-tests).
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
        self.decide_approval(mix, six, decision, None, cx);
        true
    }

    /// Decide an approval card targeted by request_id (the approval bar path): with concurrent approvals queued, each one
    /// gets its own answer regardless of arrival order; a missing id (already decided/late event) is a no-op.
    /// feedback is non-None only on the plan "Revise" path (None for other approvals)
    pub fn decide_approval_by_id(
        &mut self,
        request_id: &str,
        decision: ApprovalDecision,
        feedback: Option<String>,
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
        self.decide_approval(mix, six, decision, feedback, cx);
        true
    }

    fn decide_approval(
        &mut self,
        message_ix: usize,
        segment_ix: usize,
        decision: ApprovalDecision,
        feedback: Option<String>,
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
                feedback,
            });
        }
        cx.notify();
    }

    /// Segment-level (Thinking/ToolCall/TurnChanges) expand/collapse animation state
    pub(crate) fn expand_anim_at(
        &mut self,
        message_ix: usize,
        segment_ix: usize,
    ) -> Option<&mut ExpandAnim> {
        let segment = self
            .messages
            .get_mut(message_ix)?
            .segments
            .get_mut(segment_ix)?;
        match segment {
            Segment::Thinking { expand_anim, .. }
            | Segment::ToolCall { expand_anim, .. }
            | Segment::TurnChanges { expand_anim, .. } => Some(expand_anim),
            _ => None,
        }
    }

    /// Animation driver for expand/collapse toggling: gen+1 replays the animation; collapse enters collapsing (the content stays
    /// mounted to play the slide-shut) and unmounts when the timer expires; re-expanding mid-way mismatches the generation and is auto-invalidated
    pub(crate) fn drive_expand_anim(
        &mut self,
        message_ix: usize,
        segment_ix: usize,
        expanded_now: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(anim) = self.expand_anim_at(message_ix, segment_ix) else {
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
                if let Some(anim) = this.expand_anim_at(message_ix, segment_ix)
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

    /// Animation wrap for expanded/collapsed content: expand = the content slides open from 0 height + fades in; collapse = it stays mounted
    /// to slide shut and fade out (unmounting is the timer in drive_expand_anim). Implementation in crate::anim (shared with the sidebar
    /// workspace open/close); the id includes gen, replaying on each toggle
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
        // User message selection handles are created lazily: the subscription drives live highlighting during the drag,
        // is stored with the ChatMessage, and is released with it on clear()
        for message in &mut self.messages {
            if message.role == Role::User && message.selection.is_none() {
                let handle = TextSelectionHandle::new(message.text.clone(), cx);
                let subscription = handle.refresh_window_on_change(window, cx);
                message.selection = Some((handle, subscription));
            }
            // Notification card UI state lazily created + result file size probing (once per message, avoiding a stat every frame)
            if message.notification_ui.is_none()
                && let Some(note) = as_task_notification(&message.text)
            {
                // The size probe targets result.md (core output always carries the result attribute)
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
            // Read/Bash tool card UI state lazily created (wrap/copy/highlight caches; the render path only reads)
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
        let working_label =
            fmt_work_duration(working_secs, rust_i18n::t!("thread.working").as_ref());

        // Reaching the bottom (scroll wheel/dragging the scrollbar/keyboard, any means) auto-resumes following
        if !self.follow_bottom && self.at_bottom() {
            // A nav jump's offset only updates at prepaint, so this frame still reads the old position;
            // skip this resume and re-judge from the real position next frame
            if self.nav_jump {
                self.nav_jump = false;
            } else {
                self.follow_bottom = true;
            }
        }
        // Dragging the scrollbar thumb, a track click, or keyboard scrolling move the offset directly
        // without a wheel event: pause following on any upward move between frames so auto_scroll stops
        // fighting the user mid-stream. The 1px threshold rides out float jitter. The read goes through
        // clamped_offset_y: a wheel delta lands on the offset before the next prepaint clamps it, and
        // the transient out-of-range value must not read as an upward move
        let offset_y = self.clamped_offset_y();
        if self.follow_bottom && offset_y > self.last_scroll_y + px(1.) && !self.at_bottom() {
            self.follow_bottom = false;
        }
        self.last_scroll_y = offset_y;

        // Turn nav: one user message = one turn entry. Every message in the message list is
        // a direct child of the scroll container (see below); scroll_to_top_of_item / bounds_for_item
        // only track direct children, so messages can be precisely located by index
        let user_ixs: Vec<usize> = self
            .messages
            .iter()
            .enumerate()
            .filter(|(_, message)| message.role == Role::User)
            .map(|(ix, _)| ix)
            .collect();
        // Pinned to the bottom = reading the latest turn: the active item is always the last user message. The bottom viewport may
        // show several user messages at once, and picking the "nearest to the top" would pin the highlight on an earlier turn
        let nav_active = if user_ixs.len() >= 2 && self.at_bottom() {
            user_ixs.last().copied()
        // Active item = the visible user message nearest the viewport top; when none is visible, take the nearest one
        // above the viewport top (aligned with ZCode resolveConversationTurnNavigatorActiveQueryRowId;
        // topmost visible row must not be used: a long reply's tail would pin the highlight on the previous turn)
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
        // When the active item changes, the rail scrolls along to keep the active bar visible
        if let Some(active) = nav_active
            && self.nav_last_active != Some(active)
        {
            self.nav_last_active = Some(active);
            if let Some(pos) = user_ixs.iter().position(|&ix| ix == active) {
                self.nav_rail_scroll.scroll_to_item(pos);
            }
        }
        // Before the first paint the panel width is zero: schedule an extra frame so the nav appears;
        // after paint the condition heals itself, so no render loop forms
        let pane_width = self.scroll_handle.bounds().size.width;
        if user_ixs.len() >= 2 && pane_width <= px(0.) {
            cx.notify();
        }
        // The content column width must be purely layout-driven: the panel width measured at paint lags one frame
        // after a panel toggle, and using it for the content width would mis-lay-out the column at the old width for a frame on every toggle (jitter).
        // So the gutter depends only on the turn count (≥2 turns = reserve 48px on each side whenever the nav could appear,
        // aligned with ZCode w-[calc(100%-6rem)]), and the content column is always min(860, remaining width).
        // The nav itself still shows/hides by the measured width (a 12px bar appearing one frame late is imperceptible).
        let nav_eligible = user_ixs.len() >= 2;
        let pane_wide = pane_width >= px(720.);
        let content_max_w = px(860.);
        // When the panel is too narrow there is no room even for the gutter, so hide the nav; <2 turns needs no navigation either
        let show_nav = nav_eligible && pane_wide;

        v_flex()
            .size_full()
            // In-session search bar: a fixed row above the message list (opened by Ctrl+F)
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
                            // User scrolls up: pause following and float the "Latest messages" button (events are not swallowed; the list scrolls as usual).
                            // Scrolling up is meaningless when the content is not taller (max_offset=0, unscrollable), so keep following;
                            // otherwise even a small scroll in a short session would float the button
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
                            // Every message (and the streaming indicator/empty state) is a direct child row of the scroll container:
                            // the nav's scroll_to_top_of_item by message index depends on this structure
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
                            // Working indicator: follows the last message and scrolls with the conversation
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
                            // Compaction-in-progress divider: also appended at the end of the list (auto-compaction happens mid-turn,
                            // can appear alongside the working indicator, and the divider going last = the latest state)
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
                                                    ShimmerText::new(rust_i18n::t!(
                                                        "thread.compacting"
                                                    ))
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
                            // Retry-wait row: transient live state (superseded by any
                            // next event); the pill skips the remaining wait
                            .when_some(self.retrying.as_ref(), |this, retry| {
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
                                                        .id("retry-wait-row")
                                                        .test_support()
                                                        .gap_2()
                                                        .child(
                                                            Spinner::new()
                                                                .icon(AssetIconName::LoaderCircle)
                                                                .color(cx.theme().muted_foreground),
                                                        )
                                                        .child(
                                                            div()
                                                                .text_xs()
                                                                .text_color(
                                                                    cx.theme().muted_foreground,
                                                                )
                                                                .child(retry_wait_label(retry)),
                                                        )
                                                        .child(
                                                            div()
                                                                .id("retry-now-button")
                                                                .test_support()
                                                                .cursor_pointer()
                                                                .px_2()
                                                                .py_0p5()
                                                                .rounded_full()
                                                                .border_1()
                                                                .border_color(cx.theme().border)
                                                                .hover(|this| {
                                                                    this.bg(cx
                                                                        .theme()
                                                                        .accent
                                                                        .opacity(0.5))
                                                                })
                                                                .child(
                                                                    div()
                                                                        .text_xs()
                                                                        .text_color(
                                                                            cx.theme().foreground,
                                                                        )
                                                                        .child(
                                                                            rust_i18n::t!(
                                                                                "thread.retry_now"
                                                                            )
                                                                            .to_string(),
                                                                        ),
                                                                )
                                                                .on_click(cx.listener(
                                                                    |_, _, _, cx| {
                                                                        cx.emit(
                                                                            ThreadEvent::RetryNow,
                                                                        );
                                                                    },
                                                                )),
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
                                                .child(rust_i18n::t!("thread.empty_hint")),
                                        ),
                                )
                            }),
                    )
                    // Overlay scrollbar for the message list: reads the tracked scroll handle's
                    // bounds/offset/content size. Default Scrolling mode (appears while scrolling,
                    // fades when idle) — the same overlay style as the other panels. Hidden while
                    // the lightbox covers the area: its mask on_click does not stop mousedown, so
                    // the thumb would stay draggable through the overlay
                    .when(self.lightbox.is_none(), |this| {
                        this.child(Scrollbar::vertical(&self.scroll_handle))
                    })
                    // Turn nav: small vertical bars on the left edge, see render_turn_nav
                    .when(show_nav, |this| {
                        this.child(self.render_turn_nav(&user_ixs, nav_active, cx))
                    })
                    // When not following, float the "Latest messages" button: click to return to the bottom and resume following
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
                                        // The default hitbox does not block the layer below: clicks would pass through to the
                                        // tool cards/scroll area underneath; block the pass-through while the wheel still passes through to the list
                                        .block_mouse_except_scroll()
                                        .child(
                                            Icon::new(AssetIconName::ArrowDown)
                                                .size_4()
                                                .text_color(cx.theme().foreground),
                                        )
                                        .child(
                                            div().text_sm().child(rust_i18n::t!("thread.latest")),
                                        )
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.follow_bottom = true;
                                            this.scroll_handle.scroll_to_bottom();
                                            cx.notify();
                                        })),
                                ),
                        )
                    })
                    // Image lightbox: covers the message area (rendered last = topmost)
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
                                        .child(
                                            rust_i18n::t!(
                                                "thread.queued",
                                                text = text.chars().take(20).collect::<String>()
                                            )
                                            .to_string(),
                                        ),
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
