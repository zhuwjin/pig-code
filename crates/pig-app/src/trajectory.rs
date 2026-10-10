//! Model io trace panel: reads the current session's
//! `{session_id}.model-io.jsonl` (pig-core persists one record per
//! main-session step; the UI reads the file directly without going through
//! the protocol). Layout follows ZCode's "model io trace": grouped per call
//! (index / source / finish-reason pill / IN·OUT tokens / duration / clock
//! time), with "Input" and "Output" cards under each call; inside a card
//! each message is one row (colored role label + single-line preview +
//! chevron), each row expands independently to show the full text, and an
//! expanded row's header carries the duration, timestamp and a copy button.

use std::collections::HashSet;

use super::*;

/// Panel state (AppView.trajectory, None = not open)
pub(crate) struct TrajectoryState {
    pub(crate) records: Vec<pig_core::model_io::ModelIoRecord>,
    /// File read failure (shown when records are empty)
    pub(crate) error: Option<String>,
    /// Keys of expanded message rows ("{turn}:{row_ix}"; turn uniquely
    /// identifies a call, so expanded state survives refreshes)
    pub(crate) expanded: HashSet<String>,
    /// Keys of rows whose LONG body was expanded past the collapsed clip
    /// ("{turn}:{row_ix}:full"); long bodies clip to a whole-line pixel cap
    /// by default with an expand pill floating over a bottom fade band
    /// (no "truncated" marker, ZCode-style)
    pub(crate) expanded_full: HashSet<String>,
}

/// Collapse gate for an expanded row's body: estimated visual lines past
/// which the body clips (the text itself is never cut — the clip is a
/// whole-line pixel cap)
const TRAJ_COLLAPSED_LINES: usize = 12;
/// Wrap estimate for the gate: display columns per body row at the right
/// panel's default 300px width (text_xs mono ≈ 7.2px per ASCII cell; CJK
/// counts double — the same weight heuristic as the user-message estimate).
/// A misestimate only shifts the collapse onset slightly
const TRAJ_WRAP_COLS: usize = 34;
/// text_xs font size in rems (mirrors the body rows' .text_xs())
const TRAJ_TEXT_FONT_REMS: f32 = 0.75;

/// Collapsed cap in px: N lines × the per-line rounded height (font × phi,
/// rounded like TextStyle::line_height_in_pixels), so the clip never cuts a
/// line in half (same trick as the user-message bubble cap)
fn traj_collapsed_cap_px(rem_px: f32) -> f32 {
    let line_px = (TRAJ_TEXT_FONT_REMS * rem_px * 1.618_034).round();
    TRAJ_COLLAPSED_LINES as f32 * line_px
}

/// Whether an expanded row's body is estimated tall enough to clip: each
/// hard line contributes its wrap-aware visual rows (pure function of the
/// text, so refreshes/replays agree)
fn traj_body_is_long(text: &str) -> bool {
    text.lines()
        .map(|line| {
            let cols: usize = line
                .chars()
                .map(|ch| if ch.is_ascii() { 1 } else { 2 })
                .sum();
            cols.div_ceil(TRAJ_WRAP_COLS).max(1)
        })
        .sum::<usize>()
        > TRAJ_COLLAPSED_LINES
}

impl TrajectoryState {
    /// Load this session's model io trace from the current data directory
    /// (missing file = empty list, not an error)
    pub(crate) fn load(session_id: &str) -> Self {
        let sessions_dir = pig_utils::data_dir().join("sessions");
        let path = pig_core::model_io::model_io_path(&sessions_dir, session_id);
        if !path.exists() {
            return Self {
                records: vec![],
                error: None,
                expanded: HashSet::new(),
                expanded_full: HashSet::new(),
            };
        }
        Self {
            records: pig_core::model_io::read_all(&path),
            error: None,
            expanded: HashSet::new(),
            expanded_full: HashSet::new(),
        }
    }
}

/// Visual role of a row: decides the label text and color (six kinds, aligned
/// with ZCode's trajectoryRoleTextClass)
#[derive(Clone, Copy, PartialEq, Eq)]
enum VisualRole {
    System,
    User,
    Assistant,
    Reasoning,
    ToolCall,
    ToolResult,
}

impl VisualRole {
    fn label(self) -> std::borrow::Cow<'static, str> {
        match self {
            Self::System => rust_i18n::t!("trajectory.kind_system"),
            Self::User => rust_i18n::t!("trajectory.kind_user"),
            Self::Assistant => rust_i18n::t!("trajectory.kind_assistant"),
            Self::Reasoning => rust_i18n::t!("trajectory.kind_reasoning"),
            Self::ToolCall => rust_i18n::t!("trajectory.kind_tool_call"),
            Self::ToolResult => rust_i18n::t!("trajectory.kind_tool_result"),
        }
    }

    /// Label color (ZCode's dark/light hex pair + 80% opacity; system uses
    /// the theme's gray)
    fn color(self, cx: &App) -> Hsla {
        let dark = cx.theme().is_dark();
        let hex = match self {
            Self::System => return cx.theme().muted_foreground,
            Self::User => {
                if dark {
                    0x60a5fa
                } else {
                    0x2563eb
                }
            }
            Self::Assistant => {
                if dark {
                    0x2dd4bf
                } else {
                    0x0f766e
                }
            }
            Self::Reasoning => {
                if dark {
                    0xa78bfa
                } else {
                    0x7c3aed
                }
            }
            Self::ToolCall => {
                if dark {
                    0xf59e0b
                } else {
                    0xd97706
                }
            }
            Self::ToolResult => {
                if dark {
                    0x38bdf8
                } else {
                    0x0284c7
                }
            }
        };
        let color: Hsla = rgb(hex).into();
        color.opacity(0.8)
    }
}

/// Input message → visual role (tool maps to "tool result"; unknown roles
/// are treated as system gray)
fn input_role(msg: &pig_core::model_io::ModelIoMessage) -> VisualRole {
    match msg.role.as_str() {
        "system" => VisualRole::System,
        "user" => VisualRole::User,
        "assistant" => VisualRole::Assistant,
        "tool" => VisualRole::ToolResult,
        _ => VisualRole::System,
    }
}

/// Call source label (the source field reserves extensions like
/// subagent/compact)
fn source_label(source: &str) -> std::borrow::Cow<'static, str> {
    match source {
        "main" => rust_i18n::t!("trajectory.source_main"),
        "subagent" => rust_i18n::t!("trajectory.source_subagent"),
        "compact" => rust_i18n::t!("trajectory.source_compact"),
        other => std::borrow::Cow::Owned(other.to_string()),
    }
}

/// Thousands-separated number (aligned with ZCode's toLocaleString: 48,442)
fn fmt_num(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (ix, ch) in digits.chars().enumerate() {
        if ix > 0 && (digits.len() - ix).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Duration (ZCode formatTrajectoryDuration: milliseconds below 1s; two
/// decimal places below 10s; otherwise one)
fn fmt_duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 10_000 {
        format!("{:.2}s", ms as f64 / 1000.0)
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

/// Millisecond timestamp → local time (returns None when the local offset
/// fails; callers degrade to an empty string)
fn local_dt(ts_ms: u64) -> Option<time::OffsetDateTime> {
    let utc = time::OffsetDateTime::from_unix_timestamp_nanos(ts_ms as i128 * 1_000_000).ok()?;
    let offset = time::UtcOffset::current_local_offset().ok()?;
    Some(utc.to_offset(offset))
}

/// 12-hour clock and AM/PM (0 → 12 AM, 12 → 12 PM)
fn hour12(hour: u8) -> (u8, &'static str) {
    let ampm = if hour < 12 { "AM" } else { "PM" };
    (
        match hour % 12 {
            0 => 12,
            h => h,
        },
        ampm,
    )
}

/// Clock time (aligned with ZCode formatTrajectoryClockTime: 02:41:48 PM)
fn fmt_clock(ts_ms: u64) -> String {
    let Some(dt) = local_dt(ts_ms) else {
        return String::new();
    };
    let (h, ampm) = hour12(dt.hour());
    format!("{h:02}:{:02}:{:02} {ampm}", dt.minute(), dt.second())
}

/// Expanded-row timestamp (aligned with ZCode formatTrajectoryDateTime:
/// 10/4/2026, 2:42:08 PM)
fn fmt_datetime(ts_ms: u64) -> String {
    let Some(dt) = local_dt(ts_ms) else {
        return String::new();
    };
    let (h, ampm) = hour12(dt.hour());
    format!(
        "{}/{}/{}, {}:{:02}:{:02} {ampm}",
        u8::from(dt.month()),
        dt.day(),
        dt.year(),
        h,
        dt.minute(),
        dt.second()
    )
}

/// Single-line preview: all whitespace (newlines included) collapses into
/// one space (same as ZCode messagePreview)
fn preview(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Message row render parameters (rec_ix + row_ix form the element id; key
/// is the expanded-state key)
struct RowSpec {
    rec_ix: usize,
    /// Row index inside the card (continuously numbered across the "Input"
    /// and "Output" cards, doubling as the zebra stripe parity)
    row_ix: usize,
    key: String,
    role: VisualRole,
    preview: String,
    full: String,
    duration_ms: u64,
    ts_ms: u64,
}

impl AppView {
    /// Reread the current session's persisted model io trace records (on tab
    /// open / session switch / turn completion / manual refresh)
    pub(crate) fn reload_trajectory(&mut self) {
        if let Some(session_id) = self.current.clone() {
            self.trajectory = Some(TrajectoryState::load(&session_id));
        }
    }

    /// Title bar menu entry: opens the right "trajectory" tab
    /// (open_right_tab rereads the data internally)
    pub(crate) fn open_trajectory(&mut self, cx: &mut Context<Self>) {
        self.open_right_tab(RightTab::Trajectory, cx);
    }

    /// Right panel "trajectory" tab content: summary subtitle + a list of
    /// call cards in chronological order
    pub(crate) fn render_trajectory_panel(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(state) = &self.trajectory else {
            // Opening the tab rereads; this is the fallback for the
            // no-open-session case
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(rust_i18n::t!("trajectory.empty"))
                .into_any_element();
        };
        // Subtitle: call count · total tokens (IN including cache hits +
        // OUT) · models seen (order-preserving dedup)
        let calls = state.records.len();
        let (in_total, out_total) = state.records.iter().fold((0u64, 0u64), |(i, o), r| {
            (i + r.usage.input + r.usage.cache_read, o + r.usage.output)
        });
        let mut models: Vec<&str> = Vec::new();
        for record in &state.records {
            if !record.model.is_empty() && !models.contains(&record.model.as_str()) {
                models.push(record.model.as_str());
            }
        }
        let summary = if models.is_empty() {
            rust_i18n::t!(
                "trajectory.summary",
                calls = calls,
                tokens = fmt_num(in_total + out_total)
            )
            .to_string()
        } else {
            rust_i18n::t!(
                "trajectory.summary_models",
                calls = calls,
                tokens = fmt_num(in_total + out_total),
                models = models.join(", ")
            )
            .to_string()
        };

        // Whole-line clip cap for long row bodies (rem-aware: UI scaling
        // keeps the cut on a line boundary)
        let cap_px = traj_collapsed_cap_px(f32::from(window.rem_size()));

        v_flex()
            .id("trajectory-panel")
            .size_full()
            .overflow_y_scroll()
            .gap_3()
            .p_3()
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(summary),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("refresh-trajectory")
                            .ghost()
                            .small()
                            .icon(IconName::RotateCw)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.reload_trajectory();
                                cx.notify();
                            })),
                    ),
            )
            .child(if calls == 0 {
                h_flex()
                    .items_center()
                    .gap_2()
                    .py_6()
                    .text_color(cx.theme().muted_foreground)
                    .child(Icon::new(IconName::Inbox).size_5())
                    .child(div().text_sm().child(match &state.error {
                        Some(error) => {
                            rust_i18n::t!("trajectory.read_failed", error = error).to_string()
                        }
                        None => rust_i18n::t!("trajectory.no_records").to_string(),
                    }))
                    .into_any_element()
            } else {
                // Display-layer deltas (aligned with ZCode's timeline): the
                // first record shows the full starting context; later
                // records only show the input added relative to the previous
                // one, with assistant messages filtered out (replies already
                // appear as the previous record's output); when the context
                // shrinks (compact reset), fall back to full display
                let mut cards = Vec::new();
                let mut prev_len = 0usize;
                for (rec_ix, record) in state.records.iter().enumerate() {
                    let full = &record.input;
                    let display: Vec<_> = if rec_ix == 0 || full.len() < prev_len {
                        full.clone()
                    } else {
                        full[prev_len..]
                            .iter()
                            .filter(|m| m.role != "assistant")
                            .cloned()
                            .collect()
                    };
                    prev_len = full.len();
                    cards.push(self.render_call_card(rec_ix, record, &display, cap_px, cx));
                }
                v_flex().w_full().gap_4().children(cards).into_any_element()
            })
            .into_any_element()
    }

    /// Single call card: group header (index/source/finish reason/
    /// IN·OUT·duration·clock time) + "Input" and "Output" section cards +
    /// error block; display_input is the display-layer delta input; cap_px
    /// is the long-body whole-line clip cap
    fn render_call_card(
        &self,
        rec_ix: usize,
        record: &pig_core::model_io::ModelIoRecord,
        display_input: &[pig_core::model_io::ModelIoMessage],
        cap_px: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Materialize theme values into locals first (Hsla is Copy / fonts
        // are cloned); later closures need &mut cx
        let (muted, danger, border, accent, mono) = {
            let theme = cx.theme();
            (
                theme.muted_foreground,
                theme.danger,
                theme.border,
                theme.accent,
                theme.mono_font_family.clone(),
            )
        };
        let (finish_label, finish_color) = match record.finish.as_str() {
            "stop" => (rust_i18n::t!("trajectory.finish_stop"), muted),
            "tool_calls" => (rust_i18n::t!("trajectory.finish_tool_calls"), muted),
            "cancelled" => (rust_i18n::t!("trajectory.finish_cancelled"), muted),
            _ => (rust_i18n::t!("trajectory.finish_failed"), danger),
        };
        // Input message rows
        let mut input_rows: Vec<AnyElement> = Vec::new();
        let mut row_ix = 0usize;
        for msg in display_input {
            let mut full = msg.content.clone().unwrap_or_default();
            if !msg.tool_calls.is_empty() {
                if !full.is_empty() {
                    full.push('\n');
                }
                full.push_str(
                    rust_i18n::t!("trajectory.calls_tools", tools = msg.tool_calls.join(", "))
                        .as_ref(),
                );
            }
            if msg.images > 0 {
                if !full.is_empty() {
                    full.push('\n');
                }
                full.push_str(rust_i18n::t!("trajectory.images", n = msg.images).as_ref());
            }
            if full.is_empty() {
                full = "—".to_string();
            }
            let row_preview = preview(&full);
            input_rows.push(self.render_msg_row(
                RowSpec {
                    rec_ix,
                    row_ix,
                    key: format!("{}:{row_ix}", record.turn),
                    role: input_role(msg),
                    preview: row_preview,
                    full,
                    duration_ms: record.duration_ms,
                    ts_ms: record.ts_ms,
                },
                cap_px,
                cx,
            ));
            row_ix += 1;
        }
        // Output message rows: reasoning → assistant message → tool calls
        // (row numbering continues from the input)
        let mut output_rows: Vec<AnyElement> = Vec::new();
        let mut push_output = |role: VisualRole, full: String, rows: &mut Vec<AnyElement>| {
            let row_preview = preview(&full);
            rows.push(self.render_msg_row(
                RowSpec {
                    rec_ix,
                    row_ix,
                    key: format!("{}:{row_ix}", record.turn),
                    role,
                    preview: row_preview,
                    full,
                    duration_ms: record.duration_ms,
                    ts_ms: record.ts_ms,
                },
                cap_px,
                cx,
            ));
            row_ix += 1;
        };
        if !record.reasoning.is_empty() {
            push_output(
                VisualRole::Reasoning,
                record.reasoning.clone(),
                &mut output_rows,
            );
        }
        if !record.text.is_empty() {
            push_output(VisualRole::Assistant, record.text.clone(), &mut output_rows);
        }
        for call in &record.tool_calls {
            push_output(
                VisualRole::ToolCall,
                format!("{}\n{}", call.name, call.arguments),
                &mut output_rows,
            );
        }

        v_flex()
            .w_full()
            .gap_2()
            .child(
                // Group header (ZCode CallMetadata: IN n · OUT n · duration · time)
                h_flex()
                    .w_full()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .w(px(20.))
                            .flex_shrink_0()
                            .text_xs()
                            .font_family(mono.clone())
                            .text_color(muted)
                            .child(format!("{:02}", rec_ix + 1)),
                    )
                    .child(
                        div()
                            .text_sm()
                            .font_medium()
                            .child(source_label(&record.source).to_string()),
                    )
                    .child(
                        div()
                            .px_2()
                            .rounded_full()
                            .border_1()
                            .border_color(border)
                            .bg(accent.opacity(0.4))
                            .text_xs()
                            .text_color(finish_color)
                            .child(finish_label),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_xs()
                            .font_family(mono)
                            .text_color(muted)
                            .child(format!(
                                "IN {} · OUT {} · {} · {}",
                                fmt_num(record.usage.input + record.usage.cache_read),
                                fmt_num(record.usage.output),
                                fmt_duration(record.duration_ms),
                                fmt_clock(record.ts_ms)
                            )),
                    ),
            )
            .when(!input_rows.is_empty(), |this| {
                this.child(section_card(
                    rust_i18n::t!("trajectory.input").as_ref(),
                    input_rows,
                    cx,
                ))
            })
            .when(!output_rows.is_empty(), |this| {
                this.child(section_card(
                    rust_i18n::t!("trajectory.output").as_ref(),
                    output_rows,
                    cx,
                ))
            })
            .when_some(record.error.clone(), |this, error| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(danger)
                        .child(rust_i18n::t!("trajectory.error", error = error).to_string()),
                )
            })
            // Size-cap reset marker: this record restarted the trace file; the
            // session's earlier records are gone by design
            .when(record.file_reset, |this| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(rust_i18n::t!("trajectory.file_reset").to_string()),
                )
            })
            .into_any_element()
    }

    /// Single message row (shared by the input/output cards): collapsed =
    /// colored role label + single-line preview + chevron; expanded = label +
    /// duration·timestamp + copy button + full content; cap_px is the
    /// long-body whole-line clip cap
    fn render_msg_row(&self, spec: RowSpec, cap_px: f32, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let mono = theme.mono_font_family.clone();
        let zebra_bg = theme.accent.opacity(0.3);
        let hover_bg = theme.accent;
        let role_color = spec.role.color(cx);
        let open = self
            .trajectory
            .as_ref()
            .is_some_and(|s| s.expanded.contains(&spec.key));
        let row_id = spec.rec_ix * 4096 + spec.row_ix;
        let key = spec.key.clone();
        let zebra = spec.row_ix % 2 == 1;

        let chevron = Icon::new(if open {
            IconName::ChevronDown
        } else {
            IconName::ChevronRight
        })
        .size_3()
        .text_color(muted);

        let header = h_flex()
            .id(("traj-row", row_id))
            .w_full()
            .min_h(px(32.))
            .pl_3()
            .pr_2()
            .py_1()
            .items_center()
            .gap_2()
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                if let Some(state) = &mut this.trajectory
                    && !state.expanded.remove(&key)
                {
                    state.expanded.insert(key.clone());
                }
                cx.notify();
            }))
            .child(
                div()
                    .w(px(72.))
                    .flex_shrink_0()
                    .text_xs()
                    .font_family(mono.clone())
                    .text_color(role_color)
                    .child(spec.role.label()),
            );

        if open {
            let copy_text = spec.full.clone();
            let copy_btn = div()
                .id(("traj-copy", row_id))
                .cursor_pointer()
                .p_1()
                .rounded_sm()
                .hover(move |d| d.bg(hover_bg))
                .on_click(cx.listener(move |_, _, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copy_text.clone()));
                    cx.stop_propagation();
                }))
                .child(Icon::new(IconName::Copy).size_3().text_color(muted));
            // Long bodies clip to a whole-line pixel cap with an expand
            // affordance (no "truncated" marker — the full text is one click
            // away): collapsed = max_h clip + bottom fade band + a floating
            // centered pill (the user-message bubble's toggle pattern, with a
            // text label); expanded = the same pill as an in-flow centered
            // row under the full text. The clip is by height — the text
            // itself is never cut
            let is_long = traj_body_is_long(&spec.full);
            let full_key = format!("{}:full", spec.key);
            let full_open = self
                .trajectory
                .as_ref()
                .is_some_and(|s| s.expanded_full.contains(&full_key));
            let body = div()
                .w_full()
                .px_3()
                .pb_2()
                .text_xs()
                // Same mono face as the collapsed preview — expanding a row
                // must not flip the font under the eyes
                .font_family(mono.clone())
                .text_color(cx.theme().foreground)
                .child(spec.full.clone());
            v_flex()
                .w_full()
                .when(zebra, |d| d.bg(zebra_bg))
                .child(
                    header
                        .child(div().flex_1())
                        .child(
                            div()
                                .text_xs()
                                .font_family(mono)
                                .text_color(muted)
                                .child(format!(
                                    "{} · {}",
                                    fmt_duration(spec.duration_ms),
                                    fmt_datetime(spec.ts_ms)
                                )),
                        )
                        .child(copy_btn)
                        .child(chevron),
                )
                .child(if is_long && !full_open {
                    // Bottom fade over the clipped tail: a transparent→solid
                    // gradient in the row's base color (the panel sits on the
                    // root background; the zebra tint composites on top for odd
                    // rows — the same two-layer trick as the sidebar title fade)
                    let base = cx.theme().background;
                    div()
                        .relative()
                        .child(div().max_h(px(cap_px)).overflow_hidden().child(body))
                        .child(
                            div()
                                .absolute()
                                .bottom_0()
                                .left_0()
                                .w_full()
                                .h(px(48.))
                                .flex()
                                .items_end()
                                .justify_center()
                                .bg(linear_gradient(
                                    180.,
                                    linear_color_stop(base.opacity(0.), 0.),
                                    linear_color_stop(base, 1.),
                                ))
                                .when(zebra, |this| {
                                    this.child(div().absolute().inset_0().bg(linear_gradient(
                                        180.,
                                        linear_color_stop(zebra_bg.opacity(0.), 0.),
                                        linear_color_stop(zebra_bg, 1.),
                                    )))
                                })
                                .child(
                                    div()
                                        .pb_1p5()
                                        .child(full_toggle_pill(row_id, false, &full_key, cx)),
                                ),
                        )
                        .into_any_element()
                } else {
                    body.into_any_element()
                })
                // Expanded: the pill moves in-flow as a centered row under the
                // full text (the user-message bubble's placement rule)
                .when(is_long && full_open, |this| {
                    this.child(
                        div()
                            .w_full()
                            .pb_2()
                            .flex()
                            .justify_center()
                            .child(full_toggle_pill(row_id, true, &full_key, cx)),
                    )
                })
                .into_any_element()
        } else {
            header
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .font_family(mono)
                        .text_color(muted)
                        .child(spec.preview),
                )
                .child(chevron)
                .when(zebra, |d| d.bg(zebra_bg))
                .into_any_element()
        }
    }
}

/// Long-body toggle pill (shared by both states): a rounded-full bordered
/// chip with a shadow, carrying a chevron + the 展开/收起 label (the
/// user-message bubble's toggle shape, with a text label). The click toggles
/// `expanded_full` with stop_propagation so the row's own collapse isn't
/// triggered
fn full_toggle_pill(
    row_id: usize,
    full_open: bool,
    full_key: &str,
    cx: &mut Context<AppView>,
) -> Stateful<Div> {
    let muted = cx.theme().muted_foreground;
    let label = if full_open {
        rust_i18n::t!("trajectory.collapse_full")
    } else {
        rust_i18n::t!("trajectory.expand_full")
    };
    let full_key = full_key.to_string();
    div()
        .id(("traj-full", row_id))
        .flex()
        .items_center()
        .gap_1()
        .h_7()
        .px_3()
        .rounded_full()
        .border_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().background)
        .shadow_sm()
        .cursor_pointer()
        .hover(|this| this.bg(cx.theme().secondary))
        .child(
            Icon::new(if full_open {
                IconName::ChevronUp
            } else {
                IconName::ChevronDown
            })
            .size_3p5()
            .text_color(muted),
        )
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().foreground)
                .child(label.to_string()),
        )
        .on_click(cx.listener(move |this, _, _, cx| {
            cx.stop_propagation();
            if let Some(state) = &mut this.trajectory
                && !state.expanded_full.remove(&full_key)
            {
                state.expanded_full.insert(full_key.clone());
            }
            cx.notify();
        }))
}

/// "Input"/"Output" section card: title bar + message row list with thin
/// dividers between rows
fn section_card(title: &str, rows: Vec<AnyElement>, cx: &mut Context<AppView>) -> AnyElement {
    let theme = cx.theme();
    let mut card = v_flex()
        .w_full()
        .rounded_lg()
        .border_1()
        .border_color(theme.border)
        .overflow_hidden()
        .child(
            div()
                .h(px(32.))
                .w_full()
                .px_3()
                .flex()
                .items_center()
                .bg(theme.accent.opacity(0.4))
                .text_xs()
                .font_medium()
                .text_color(theme.foreground)
                .child(title.to_string()),
        );
    for row in rows {
        card = card
            .child(div().h(px(1.)).w_full().bg(theme.border.opacity(0.5)))
            .child(row);
    }
    card.into_any_element()
}
