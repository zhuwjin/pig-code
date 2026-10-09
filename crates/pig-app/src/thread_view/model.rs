use super::*;

pub(crate) use crate::anim::{EXPAND_ANIM_DUR, ExpandAnim};

/// Render data of a nav preview card (message index, bar bounds, user preview, assistant preview, whether it is real reply text)
pub(crate) type NavCardData = (usize, Bounds<Pixels>, String, String, bool);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    System,
}

/// Agent card (written into Agent/AgentSwarm tool cards by `Event::SubagentCard`; live direct delivery plus
/// rebuild from the persisted rollout on replay): metadata + background run state. With no cards (the transient state in live before the SubagentCard
/// event arrives) it falls back to the standard tool card style
#[derive(Clone)]
pub struct AgentCardMeta {
    pub agent_id: String,
    pub profile: String,
    pub description: String,
    /// "{provider_name} · {model}" (may carry a reasoning-tier suffix)
    pub model: String,
    /// This run is backgrounded: a background Agent/AgentSwarm's tool call returns its receipt immediately, so done does not mean
    /// the subagent has ended; the run state is driven by the subagent's real lifecycle (SubagentActivity)
    pub background: bool,
    /// The background subagent has ended (set by SubagentActivity finished; replay re-emits it from core);
    /// foreground cards ignore it; the foreground run state follows the tool call's done
    pub finished: bool,
    /// Finish sequence number (arrival order of SubagentActivity finished): the Swarm panel uses it to move finished
    /// subagents to the front (earlier finishers first); replay lacks this event and it stays None → keeps launch order
    pub finished_seq: Option<u64>,
    /// Live progress row of the background subagent (written by SubagentActivity item, cleared on finished);
    /// the foreground card's progress goes through SubagentProgress into the segment-level live_note, not this field
    pub live_note: Option<AgentLiveNote>,
}

/// One background-subagent activity line, stored raw and localized at render
/// time (a language switch updates running cards too); the text is already
/// whitespace-normalized and truncated
#[derive(Clone)]
pub(crate) enum AgentLiveNote {
    /// Tool activity: "{name} {text}"; a missing name falls back to the
    /// localized "Tool" word at render time
    Tool { name: Option<String>, text: String },
    /// Assistant activity: the first line of the body, no prefix
    Text { line: String },
}

impl AgentLiveNote {
    /// Render-time display text
    pub fn display(&self) -> String {
        match self {
            AgentLiveNote::Tool { name, text } => {
                let name = name
                    .clone()
                    .unwrap_or_else(|| rust_i18n::t!("thread.tool_fallback").to_string());
                format!("{name} {text}")
            }
            AgentLiveNote::Text { line } => line.clone(),
        }
    }
}

pub enum Segment {
    Thinking {
        text: String,
        open: bool,
        /// True once the user has manually expanded/collapsed; automatic folding no longer overrides it
        pinned: bool,
        /// Stopwatch start (the moment the first delta arrives)
        started: std::time::Instant,
        /// Duration frozen when thinking ends; history segments rebuilt by replay have no real clock and keep None (shown as "lasted a few seconds")
        duration: Option<std::time::Duration>,
        /// Scroll handle of the expanded body (track_scroll keeps the scroll position persistent)
        body_scroll: ScrollHandle,
        /// Horizontal scroll handle of the in-progress header rolling output line (tail-pinned to show the latest content)
        ticker_scroll: ScrollHandle,
        /// Vertical-roll state machine of the rolling output line (on line change the old line rolls up and out, the new one rolls in from below)
        ticker: TickerRoll,
        /// Expand/collapse animation state (see ExpandAnim)
        expand_anim: ExpandAnim,
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
        /// The turn was aborted while this call was still running: an empty
        /// output renders the localized "Stopped" placeholder at draw time (a
        /// non-empty partial output is shown as-is)
        stopped: bool,
        expanded: bool,
        /// This run's edit diff for write/modify tools (inline diff card)
        edit: Option<EditDiff>,
        /// Live progress row of the foreground subagent (written by SubagentProgress, cleared on ToolCallEnd);
        /// a separate field rather than overwriting summary: the original summary ("subagent explore: …") must survive while running.
        /// Replay lacks this event; always None
        live_note: Option<String>,
        /// Agent card list (appended per item_id by SubagentCard events: one for Agent, one
        /// per subagent for AgentSwarm; when non-empty, rendered in the agent card style; click opens the right-side subagent conversation tab;
        /// live direct delivery plus rebuild from rollout records on replay)
        agent_cards: Vec<AgentCardMeta>,
        /// UI state of the Read tool code card (lazily created before render; see read.rs)
        read_ui: Option<ReadCardUi>,
        /// UI state of the Bash tool code card (command card + output card; lazily created before render, see bash.rs)
        bash_ui: Option<BashCardUi>,
        /// Expand/collapse animation state (see ExpandAnim)
        expand_anim: ExpandAnim,
        /// Scroll handle of the expanded body (track_scroll keeps the scroll position persistent)
        body_scroll: ScrollHandle,
    },
    /// The turn's file-changes panel shown at the end of a turn (same as the ZCode turn header file changes)
    TurnChanges {
        rows: Vec<TurnFileRow>,
        open: bool,
        /// Expand/collapse animation state (see ExpandAnim)
        expand_anim: ExpandAnim,
    },
    /// ExitPlanMode plan card (same as kimi "Plan pending/approved"):
    /// built at ToolCallBegin(tool=ExitPlanMode) by parsing the plan out of the detail JSON,
    /// the same path for live and replay; a collapsed one-line row with three states, the chevron expands to the full plan
    Plan {
        state: Entity<TextViewState>,
        /// The decision has landed (ToolCallEnd arrived)
        done: bool,
        /// Decision outcome: the receipt contains "Plan approved"
        approved: bool,
        is_error: bool,
        /// Expanded state (collapsed to one line by default)
        open: bool,
        expand_anim: ExpandAnim,
        /// Scroll handle of the expanded body (track_scroll keeps the scroll position persistent)
        body_scroll: ScrollHandle,
    },
    Approval {
        request_id: String,
        decision: Option<ApprovalDecision>,
    },
}

/// UI state of the Read tool code card (lazily created before render; kept for the segment's lifetime)
pub struct ReadCardUi {
    /// Word wrap (off by default: horizontal scrolling)
    pub wrap: bool,
    /// Copy button feedback (swaps to a check; app convention is not to revert)
    pub copied: bool,
    /// Horizontal scroll handle for no-wrap mode
    pub h_scroll: ScrollHandle,
    /// Parsing + highlight cache (built on first expand; RefCell: render goes through &self as an immutable borrow.
    /// Theme switches recompute via Arc pointer equality on HighlightedCode.theme)
    pub cache: std::cell::RefCell<Option<std::rc::Rc<ReadCardContent>>>,
}

impl ReadCardUi {
    pub fn new() -> Self {
        Self {
            wrap: false,
            copied: false,
            h_scroll: ScrollHandle::new(),
            cache: std::cell::RefCell::new(None),
        }
    }
}

/// Parsed + highlighted result of the Read output (contents of ReadCardUi.cache)
pub struct ReadCardContent {
    /// (file line number, line content)
    pub lines: Vec<(usize, String)>,
    /// Trailing annotation lines ([truncated…]/[file info…]/[warning…])
    pub notes: Vec<String>,
    /// Concatenated text for highlighting (line contents joined with \n, without the line-number prefix)
    pub code: String,
    pub highlighted: crate::code_view::HighlightedCode,
    /// Explicit content width in no-wrap mode (drives horizontal scrolling; measurement in code_view)
    pub max_line_width: Pixels,
}

/// UI state of one code subcard (the Bash card's command/output cards, see bash.rs)
pub struct SubCardUi {
    /// Word wrap (off by default: horizontal scrolling)
    pub wrap: bool,
    /// Copy button feedback (swaps to a check; app convention is not to revert)
    pub copied: bool,
    /// Vertical scroll handle
    pub v_scroll: ScrollHandle,
    /// Horizontal scroll handle for no-wrap mode
    pub h_scroll: ScrollHandle,
}

impl SubCardUi {
    pub fn new() -> Self {
        Self {
            wrap: false,
            copied: false,
            v_scroll: ScrollHandle::new(),
            h_scroll: ScrollHandle::new(),
        }
    }
}

/// UI state of the Bash tool code card (lazily created before render; kept for the segment's lifetime)
pub struct BashCardUi {
    /// Command card state
    pub cmd: SubCardUi,
    /// Output card state
    pub out: SubCardUi,
    /// Content cache of the command (bash highlight) and output (plain text) (RefCell: render borrows read-only;
    /// theme switches recompute via Arc pointer equality on PreparedCode.highlighted.theme)
    pub cache: std::cell::RefCell<Option<std::rc::Rc<BashCardContent>>>,
}

impl BashCardUi {
    pub fn new() -> Self {
        Self {
            cmd: SubCardUi::new(),
            out: SubCardUi::new(),
            cache: std::cell::RefCell::new(None),
        }
    }
}

/// Bash card content cache: command (bash syntax highlight) + output ("text" plain text, width-measured only)
pub struct BashCardContent {
    pub cmd: crate::code_view::PreparedCode,
    pub out: crate::code_view::PreparedCode,
}

/// Vertical-roll timing of the thinking rolling line (ZCode QueuedSummaryContent constants): 300ms roll + 500ms hold
pub(crate) const TICKER_ROLL_TRANSITION: std::time::Duration =
    std::time::Duration::from_millis(300);
/// Minimum interval between two rolls (300 roll + 500 hold)
pub(crate) const TICKER_ROLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(800);
/// When the timer is late by more than this, stale intermediate queued entries are skipped and only the latest one plays
pub(crate) const TICKER_ROLL_DRIFT_SKIP: std::time::Duration =
    std::time::Duration::from_millis(250);
/// Vertical-roll offset ≈ 0.8em (text_sm 14px)
pub(crate) const TICKER_ROLL_OFFSET_PX: f32 = 11.0;

/// Vertical-roll state machine of the thinking rolling line (same as ZCode QueuedSummaryContent):
/// the rolling line = the last non-empty line of the accumulated thinking text, and its line number is the roll key. Unchanged number → refresh the
/// text in place; changed number → roll vertically (old line rolls up and out, new line rolls in from below, 300ms), then hold at least
/// 500ms before rolling the next one; new lines arriving during the hold are queued (at most 2: the next one + a replaceable
/// latest one); when timer drift exceeds the threshold, intermediate entries are skipped and the latest plays directly.
#[derive(Default)]
pub(crate) struct TickerRoll {
    /// Currently displayed line (line number, single-line-collapsed text)
    pub displayed: Option<(usize, String)>,
    /// The previous line while exiting (overlaid for 300ms after the roll-in)
    pub exiting: Option<(usize, String)>,
    /// Pending roll queue: [0] = the next entry (not overwritable), [1] = the queue-jumping entry (new overwrites old)
    pub(crate) queue: Vec<(usize, String)>,
    /// Less than one interval since the last roll-in (the 800ms timer is running)
    pub(crate) rolling: bool,
    /// Timer generation: promote/reset each bump it by 1, invalidating stale in-flight timers
    pub(crate) generation: u64,
    /// Wall-clock moment of the last roll-in (timer drift detection)
    pub(crate) promoted_at: Option<std::time::Instant>,
    /// Whether the current line rolled in (the first line appears directly, no animation)
    pub rolled_in: bool,
}

impl TickerRoll {
    /// Feed the latest target line; returns true = an immediate roll happened (the caller must start the roll-interval timer)
    pub(crate) fn feed(&mut self, target: (usize, String)) -> bool {
        match &mut self.displayed {
            // The first line displays directly, without an enter animation (ZCode AnimatePresence initial={false})
            None => {
                self.displayed = Some(target);
                false
            }
            // Same line number: appended to the same line, refresh the text in place
            Some((ix, text)) if *ix == target.0 => {
                *text = target.1;
                false
            }
            _ => {
                if self.rolling {
                    // Enqueued during the hold: same key overwrites; otherwise keep the first entry, and the new entry takes/swaps the second slot
                    if let Some(slot) = self.queue.iter_mut().find(|(ix, _)| *ix == target.0) {
                        *slot = target;
                    } else if self.queue.len() < 2 {
                        self.queue.push(target);
                    } else {
                        self.queue[1] = target;
                    }
                    false
                } else {
                    self.promote(target);
                    true
                }
            }
        }
    }

    /// The roll-interval timer fired: clear the exiting line, trim the queue for drift, then roll in the next entry;
    /// returns true = a new line rolled (the caller renews the timer)
    pub(crate) fn fire(&mut self, generation: u64, now: std::time::Instant) -> bool {
        if generation != self.generation {
            return false;
        }
        self.exiting = None;
        self.rolling = false;
        // The timer fires late when the main thread is busy: replaying stale lines one by one would show the user
        // a string of stale states once the stutter clears, feeling even more janky; skip intermediate entries and play the latest directly
        let drifted = self
            .promoted_at
            .is_some_and(|t| now.duration_since(t) > TICKER_ROLL_INTERVAL + TICKER_ROLL_DRIFT_SKIP);
        if drifted && self.queue.len() > 1 {
            let last = self.queue.pop().expect("len > 1");
            self.queue.clear();
            self.queue.push(last);
        }
        if self.queue.is_empty() {
            return false;
        }
        let next = self.queue.remove(0);
        self.promote(next);
        true
    }

    /// Reset to the latest line on expand/collapse toggle (ZCode: the rolling line unmounts while expanded and remounts
    /// on the latest line when back to collapsed, without replaying the roll); bump the generation by 1 to invalidate in-flight timers
    pub(crate) fn reset_to(&mut self, target: Option<(usize, String)>) {
        self.displayed = target;
        self.exiting = None;
        self.queue.clear();
        self.rolling = false;
        self.promoted_at = None;
        self.rolled_in = false;
        self.generation += 1;
    }

    fn promote(&mut self, next: (usize, String)) {
        self.exiting = self.displayed.take();
        self.displayed = Some(next);
        self.rolled_in = true;
        self.rolling = true;
        self.promoted_at = Some(std::time::Instant::now());
        self.generation += 1;
    }
}

/// The rolling line's target line: the last non-empty trimmed line of the accumulated thinking text collapsed to a single line, returning (line number, text);
/// the line number is the vertical-roll key (same as ZCode resolveReasoningStreamingSummary).
/// lines() splits on \n only: a bare carriage return \r (with no following \n) stays inside the line, yet the render layer breaks on it,
/// splitting the rolling line into several lines; all tab/return-class whitespace is collapsed to single spaces
pub(crate) fn ticker_target_line(text: &str) -> Option<(usize, String)> {
    // Lines is a double-ended iterator but not after enumerate; count the total lines first, then search from the tail
    let total = text.lines().count();
    text.lines()
        .rev()
        .enumerate()
        .find(|(_, l)| !l.trim().is_empty())
        .map(|(back, l)| {
            (
                total - 1 - back,
                l.split_whitespace().collect::<Vec<_>>().join(" "),
            )
        })
}

/// One file row in the per-turn changes panel
pub struct TurnFileRow {
    pub(crate) edit: EditDiff,
}

/// Image attachments of a user message: thumbnails are loaded by the event's image_nums (the media directory's file-name index N, mapping to `{N}.{ext}`);
/// the indexes match the image order (the UI display numbering starts at 1 in order, see message_image_number)
pub struct UserImage {
    /// Thumbnail (load/decode failure = None → renders a fallback text chip)
    pub(crate) thumb: Option<std::sync::Arc<Image>>,
    /// Original image dimensions (for aspect-preserving thumbnail scaling)
    pub(crate) dims: (u32, u32),
}

/// Current state of the image lightbox (the large-image overlay opened by clicking a thumbnail)
pub(crate) struct Lightbox {
    pub(crate) image: std::sync::Arc<Image>,
    /// Top label ("Image N")
    pub(crate) label: String,
    /// Original image dimensions (for aspect-fitting into the window's usable area)
    pub(crate) dims: (u32, u32),
    /// Source message index and image index within the message
    pub(crate) position: (usize, usize),
    /// Zoom factor relative to the window-fitted size
    pub(crate) zoom: f32,
    /// Pan offset relative to the viewport center (pixels)
    pub(crate) pan: (f32, f32),
    /// Mouse position and image offset when the drag started
    pub(crate) drag_start: Option<((f32, f32), (f32, f32))>,
    /// Whether the current drag started inside the image bounds
    pub(crate) drag_capture: bool,
    /// Whether this gesture has already moved, so a drag is not mistaken for a click
    pub(crate) drag_moved: bool,
}

/// Turn work-row state (aligned with ZCode AssistantHistoryStatus): lands on that turn's
/// assistant message when the turn ends, collapsing the work segments (thinking blocks + tool cards) into one "Worked for N seconds ›" line
#[derive(Clone, Copy)]
pub enum WorkState {
    /// Completed normally; duration None marks a history turn without TurnStats in replay (label "Processed")
    Completed {
        duration: Option<std::time::Duration>,
    },
    /// Interrupted or error ending (label "Stopped", consistent with the footer wording)
    Stopped,
}

/// Turn-end footer line: raw data kept in the model and localized at render
/// time, so switching the language re-renders already-finished turns too
pub enum Footer {
    /// "Turn ended · took Ns" + the optional usage stats section
    TurnEnd {
        duration_ms: u64,
        stats: Option<pig_protocol::TurnUsageStats>,
    },
    /// Turn interrupted or errored ("Stopped")
    Stopped,
}

pub struct ChatMessage {
    pub role: Role,
    pub text: String,
    /// System note kind (only meaningful for the System role): plain text / "Context compacted" divider
    /// (the compact divider renders as a divider row with a "view summary" link; the full summary stays in text for self-assertions and debugging)
    pub system_kind: SystemNoteKind,
    /// Selection handle + refresh subscription of a user message (drives live highlighting during drag-selection); only the User role has it
    pub selection: Option<(TextSelectionHandle, Subscription)>,
    /// Long user-bubble expanded state: false = clipped to the collapsed cap with an
    /// expand toggle below the bubble. Initialized at construction from the line
    /// estimate (`user_msg_is_long`), so the same estimate at render never disagrees;
    /// only meaningful for the User role
    pub user_open: bool,
    /// Height-tween state for the long-bubble expand/collapse (see UserMsgAnim)
    pub user_anim: UserMsgAnim,
    pub files: Vec<String>,
    /// Image attachments of a user message (loaded by image_nums; non-empty only for the User role)
    pub images: Vec<UserImage>,
    pub segments: Vec<Segment>,
    pub footer: Option<Footer>,
    /// Turn work-row state (only meaningful for the Assistant role): None = the turn has not ended or has no work segments
    pub work_state: Option<WorkState>,
    /// Work-row expanded state (true = work segments visible inline; false = collapsed into one line)
    pub work_open: bool,
    /// The action row's "Copy" was clicked (button swaps to a check + success color, reverting after 1.2s; same as ZCode)
    pub copied: bool,
    /// Revert timer generation: each click bumps it by 1; a mismatched generation at expiry is discarded (rapid clicks do not cause premature reverts)
    pub copied_gen: u64,
    /// UI state of the background subagent notification card (lazily created before render; Some only for <task-notification> messages)
    pub notification_ui: Option<NotificationUi>,
}

/// System note kind
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum SystemNoteKind {
    Plain,
    /// "Context compacted" divider: label + "before → after" token counts + a blue
    /// "view summary" link opening the right-side summary panel (the full summary
    /// also stays in text for self-assertions and debugging)
    Compacted {
        /// Pre-/post-compaction context usage watermark (None = unknown, e.g. no
        /// Usage sample yet or a pre-field rollout record): the pair drives the
        /// "（Nk → Mk tokens）" suffix; when either side is missing only the label shows
        used_before: Option<u64>,
        used_after: Option<u64>,
        /// Bare model summary for the right-side panel (None = truncation fallback;
        /// the panel then shows the full note)
        summary: Option<String>,
    },
    /// Core error note: renders "⚠ {localized error}" with the text built at
    /// draw time (a language switch updates it too); `text` stays empty
    Error(pig_protocol::CoreError),
}

/// UI state of the background subagent notification card (stored with the message, released with it on clear())
pub struct NotificationUi {
    /// Expanded state of the "raw payload" collapsed area
    pub payload_open: bool,
    /// "Copy path" was clicked (button swaps to "Copied")
    pub copied: bool,
    /// Record file size cache: None = no record attribute; Some(None) = the file is gone;
    /// Some(Some(n)) = bytes (probed once before render, avoiding a stat every frame)
    pub record_size: Option<Option<u64>>,
    /// Scroll handle of the payload expanded area
    pub payload_scroll: ScrollHandle,
}

impl ChatMessage {
    pub(crate) fn user(text: String, files: Vec<String>) -> Self {
        Self {
            role: Role::User,
            user_open: !user_msg_is_long(&text),
            user_anim: UserMsgAnim::default(),
            text,
            system_kind: SystemNoteKind::Plain,
            selection: None,
            files,
            images: vec![],
            segments: vec![],
            footer: None,
            work_state: None,
            work_open: false,
            copied: false,
            copied_gen: 0,
            notification_ui: None,
        }
    }

    pub(crate) fn system(text: String) -> Self {
        Self::system_with_kind(text, SystemNoteKind::Plain)
    }

    pub(crate) fn system_with_kind(text: String, kind: SystemNoteKind) -> Self {
        Self {
            role: Role::System,
            user_open: true,
            user_anim: UserMsgAnim::default(),
            text,
            system_kind: kind,
            selection: None,
            files: vec![],
            images: vec![],
            segments: vec![],
            footer: None,
            work_state: None,
            work_open: false,
            copied: false,
            copied_gen: 0,
            notification_ui: None,
        }
    }

    pub(crate) fn assistant() -> Self {
        Self {
            role: Role::Assistant,
            user_open: true,
            user_anim: UserMsgAnim::default(),
            text: String::new(),
            system_kind: SystemNoteKind::Plain,
            selection: None,
            files: vec![],
            images: vec![],
            segments: vec![],
            footer: None,
            work_state: None,
            work_open: false,
            copied: false,
            copied_gen: 0,
            notification_ui: None,
        }
    }
}

/// Long user-bubble collapse constants (ZCode's ConversationUserInputBody):
/// tall bubbles collapse by default; the clip tweens between the cap and the
/// natural height and a floating icon pill toggles it.
/// Collapsed cap: whole rendered lines. ZCode pins a raw 120px, which at our
/// metrics (0.875rem font × phi line height ≈ 22.65px) lands 6px into line 6,
/// leaving a partial-glyph sliver at the tail — so the cap snaps to this many
/// full lines instead.
pub(crate) const USER_MSG_COLLAPSED_LINES: usize = 5;
/// text_sm font size in rems (mirrors the bubble's .text_sm()).
const USER_MSG_TEXT_FONT_REMS: f32 = 0.875;
/// Collapsed cap in px: N lines × the per-line rounded height (font × phi,
/// rounded like TextStyle::line_height_in_pixels), so the clip never cuts a
/// line in half regardless of the rem scale.
pub(crate) fn user_msg_collapsed_cap_px(rem_px: f32) -> f32 {
    let line_px = (USER_MSG_TEXT_FONT_REMS * rem_px * 1.618_034).round();
    USER_MSG_COLLAPSED_LINES as f32 * line_px
}
/// Wrap estimate: display columns per bubble line (the bubble spans ≈80% of the
/// message area at typical widths; CJK counts double, the same weight heuristic
/// as `show_full_input`). A misestimate only shifts collapse onset slightly.
const USER_MSG_WRAP_COLS: usize = 80;
/// Collapse when the estimate reaches this many lines (ZCode measures the real
/// scrollHeight > 121px ≈ 6 lines; +1 line of slack because our line count is
/// an estimate, keeping borderline-fitting messages toggle-free).
const USER_MSG_COLLAPSE_THRESHOLD: usize = 7;

/// Whether a user bubble's text is estimated tall enough to collapse by default.
/// Pure function of the text so the constructor default and the render-time
/// toggle agree; live and replay share it (replayed history also starts collapsed).
pub(crate) fn user_msg_is_long(text: &str) -> bool {
    user_msg_estimated_lines(text) >= USER_MSG_COLLAPSE_THRESHOLD
}

fn user_msg_estimated_lines(text: &str) -> usize {
    text.lines()
        .map(|line| {
            let cols: usize = line
                .chars()
                .map(|ch| if ch.is_ascii() { 1 } else { 2 })
                .sum();
            cols.div_ceil(USER_MSG_WRAP_COLS).max(1)
        })
        .sum()
}

/// Height-tween state of a long user bubble's expand/collapse (ZCode animates
/// `max-height` for 300ms ease-out both ways): `generation` bumps on each toggle
/// (part of the animation element id, replaying it), `measured_h` continuously
/// tracks the text's natural height via the inner element's prepaint bounds.
pub struct UserMsgAnim {
    pub generation: u64,
    pub measured_h: std::rc::Rc<std::cell::Cell<f32>>,
}

impl Default for UserMsgAnim {
    fn default() -> Self {
        Self {
            generation: 0,
            measured_h: std::rc::Rc::new(std::cell::Cell::new(0.)),
        }
    }
}

/// Parsed result of the synthetic user message core injects when a background subagent finishes/fails (identical text in live and replay).
/// The opening tag carries structured attributes (`<task-notification agent_id=".." status=".." …>`);
/// each attribute parses independently and a missing one is None (parse robustness), with the render side defaulting per field.
pub(crate) struct TaskNotification {
    pub(crate) agent_id: Option<String>,
    pub(crate) status: Option<String>,
    pub(crate) turns: Option<String>,
    pub(crate) description: Option<String>,
    /// The subagent's actual elapsed time (milliseconds)
    pub(crate) duration_ms: Option<u64>,
    /// Absolute path of the subagent context JSONL record file
    pub(crate) record: Option<String>,
    /// Absolute path of the subagent's full-result file ({agent_id}.result.md; the file row prefers it)
    pub(crate) result: Option<String>,
}

/// Recognizes a notification when the whole text is wrapped in `<task-notification…>…</task-notification>` and parses
/// the opening-tag attributes. Used only for display-layer routing: the message text (tags included) is untouched, and the payload collapsed area also renders the original text.
pub(crate) fn as_task_notification(text: &str) -> Option<TaskNotification> {
    let rest = text.trim().strip_prefix("<task-notification")?;
    // The prefix must be immediately followed by '>' or whitespace (guards against misreading <task-notification-foo>)
    if !rest.starts_with('>') && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let (attrs, after) = rest.split_once('>')?;
    // Closing-tag check (strip_suffix only strips the outermost one: inner same-name tags do not affect recognition)
    after.strip_suffix("</task-notification>")?;
    Some(TaskNotification {
        agent_id: notification_attr(attrs, "agent_id"),
        status: notification_attr(attrs, "status"),
        turns: notification_attr(attrs, "turns"),
        description: notification_attr(attrs, "description"),
        duration_ms: notification_attr(attrs, "duration_ms").and_then(|v| v.parse().ok()),
        record: notification_attr(attrs, "record"),
        result: notification_attr(attrs, "result"),
    })
}

/// Extracts `name="value"` from the opening-tag attribute section (simple substring search; values contain no quotes, core sanitizes them)
pub(crate) fn notification_attr(attrs: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let start = attrs.find(&needle)? + needle.len();
    let value = &attrs[start..];
    let end = value.find('"')?;
    Some(value[..end].to_string())
}

/// Notification-card duration formatting: <60s → "X.X s"; ≥60s → "m min ss s"
pub(crate) fn format_notification_duration(ms: u64) -> String {
    if ms < 60_000 {
        rust_i18n::t!("thread.notif_duration_seconds", n = ms as f64 / 1000.0 : {:.1}).to_string()
    } else {
        rust_i18n::t!(
            "thread.notif_duration_minutes",
            m = ms / 60_000,
            s = (ms % 60_000) / 1000 : {:02}
        )
        .to_string()
    }
}

/// Elides the middle of a record file path (keeps the first character and the last two segments): the …/sessions/{sid}.agents/{id}.jsonl shape
pub(crate) fn elide_record_path(path: &str) -> String {
    const MAX_CHARS: usize = 48;
    if path.chars().count() <= MAX_CHARS {
        return path.to_string();
    }
    let mut tail = path.rsplit('/');
    let (Some(file), Some(parent)) = (tail.next(), tail.next()) else {
        return path.to_string();
    };
    let head = path.chars().next().unwrap_or('…');
    format!("{head}…/{parent}/{file}")
}

/// File size formatting (<1KB shows B, otherwise one-decimal KB/MB)
pub(crate) fn format_file_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / 1048576.0)
    }
}
