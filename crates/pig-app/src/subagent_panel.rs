//! "Subagent" tab of the right panel: a read-only view of a
//! background/foreground subagent's full conversation (since A3b).
//! Data flow: notification card / agent card click → Op::LoadSubagent → core
//! reads {sessions_dir}/{session_id}.agents/{agent_id}.jsonl →
//! Event::SubagentHistory → set_history fills the content. Before the load
//! arrives, "Loading…" is shown.
//! Live updates since A3d: at each step core projects new messages into
//! SubagentActivity events pushed one by one, and the panel appends them
//! (while running, a "running" indicator sits at the bottom and disappears
//! after finished); the narrow race between incremental and full loads is
//! closed by a full reload after finished (initiated on the AppView side).

use gpui_kit::assets::IconName as AssetsIconName;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::text::{TextView, TextViewState};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use pig_protocol::SubagentItem;

/// Single display row; tool rows carry the expanded state and an output
/// viewport scroll handle, assistant rows carry markdown state
struct SubagentRow {
    item: SubagentItem,
    /// Markdown render state of assistant rows (None for other rows)
    markdown: Option<Entity<TextViewState>>,
    /// Expanded state of tool rows
    expanded: bool,
    /// Scroll handle for a tool row's expanded output (track_scroll keeps the
    /// scroll position)
    output_scroll: ScrollHandle,
}

pub struct SubagentPanel {
    session_id: String,
    /// Tab/header title (notification card description; superseded by meta
    /// once SubagentHistory arrives)
    title: String,
    /// "{provider} · {model}" (filled after loading)
    subtitle: String,
    /// None = loading
    rows: Option<Vec<SubagentRow>>,
    /// Whether the subagent is still running (initialized from the registry
    /// snapshot in SubagentHistory, maintained via SubagentActivity)
    running: bool,
    /// Activity items received before the full load arrives (out-of-order
    /// buffer; appended to the tail in set_history)
    pending: Vec<SubagentItem>,
    /// Total count of received activity items (for self-test assertions)
    activity_items: usize,
    /// Follow mode (same as thread_view): auto follow bottom on append/load;
    /// scrolling up by the user pauses it (a "latest message" button pops
    /// up), and returning to the bottom (by any means) or clicking the
    /// button resumes it
    following: bool,
    scroll: ScrollHandle,
}

/// Display item → row: assistant rows build markdown render state (one-shot
/// set_text, see the Markdown section usage in thread_view). Shared by the
/// initial full load and live appends.
fn build_row(item: SubagentItem, cx: &mut Context<SubagentPanel>) -> SubagentRow {
    let markdown = (item.role == "assistant").then(|| {
        let state = cx.new(|cx| TextViewState::markdown("", cx));
        let text = item.text.clone();
        state.update(cx, |state, cx| state.set_text(&text, cx));
        state
    });
    SubagentRow {
        item,
        markdown,
        expanded: false,
        output_scroll: ScrollHandle::new(),
    }
}

impl SubagentPanel {
    pub fn new(session_id: String, title: String) -> Self {
        Self {
            session_id,
            title,
            subtitle: String::new(),
            rows: None,
            running: false,
            pending: vec![],
            activity_items: 0,
            following: true,
            scroll: ScrollHandle::new(),
        }
    }

    /// At-bottom check (same as thread_view): gpui scroll offsets are
    /// negative (top 0 → bottom -max_offset), so at_bottom ⟺ offset +
    /// max_offset ≈ 0
    fn at_bottom(&self) -> bool {
        self.scroll.offset().y + self.scroll.max_offset().y <= px(2.)
    }

    /// Session ownership check for event routing (late events from other
    /// sessions are ignored)
    pub fn matches_session(&self, session_id: &str) -> bool {
        self.session_id == session_id
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    /// SubagentHistory arrives: fill with the full history plus the earlier
    /// activity buffer
    pub fn set_history(
        &mut self,
        title: String,
        subtitle: String,
        items: Vec<SubagentItem>,
        running: bool,
        cx: &mut Context<Self>,
    ) {
        self.title = title;
        self.subtitle = subtitle;
        self.running = running;
        let mut all = items;
        all.append(&mut self.pending);
        self.rows = Some(all.into_iter().map(|item| build_row(item, cx)).collect());
        // Follow bottom while following (the initial load always has
        // following=true: opening means reading the latest);
        // scroll_to_bottom is a deferred marker that only reaches the real
        // bottom after the next frame's layout
        if self.following {
            self.scroll.scroll_to_bottom();
        }
        cx.notify();
    }

    /// Append a SubagentActivity item; follow bottom when following (do not
    /// disturb after the user scrolls up)
    pub fn push_item(&mut self, item: SubagentItem, cx: &mut Context<Self>) {
        self.activity_items += 1;
        match &mut self.rows {
            // Full history not arrived yet: buffer (set_history appends to
            // the tail); closed by a full reload after finished
            None => self.pending.push(item),
            Some(rows) => {
                rows.push(build_row(item, cx));
                if self.following {
                    self.scroll.scroll_to_bottom();
                }
            }
        }
        cx.notify();
    }

    /// Subagent finished (including cancelled/killed): turn off the
    /// "running" indicator
    pub fn set_finished(&mut self, cx: &mut Context<Self>) {
        self.running = false;
        cx.notify();
    }

    /// For self-tests: (title, loaded item count); None when not loaded
    pub fn debug_state(&self) -> Option<(String, usize)> {
        self.rows
            .as_ref()
            .map(|rows| (self.title.clone(), rows.len()))
    }

    /// For self-tests: (running, loaded row count (buffer included), total
    /// activity item count)
    pub fn debug_live(&self) -> (bool, usize, usize) {
        (
            self.running,
            self.rows.as_ref().map(|r| r.len()).unwrap_or(0) + self.pending.len(),
            self.activity_items,
        )
    }

    /// For self-tests: (following, at_bottom) — follow state and at-bottom check
    pub fn debug_scroll(&self) -> (bool, bool) {
        (self.following, self.at_bottom())
    }

    /// Tool row: icon + tool name + summary (single line with ellipsis);
    /// click toggles the output card
    fn render_tool_row(&self, ix: usize, row: &SubagentRow, cx: &mut Context<Self>) -> AnyElement {
        let subtle = cx.theme().muted_foreground;
        let subtlest = subtle.opacity(0.6);
        let summary = row
            .item
            .text
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        v_flex()
            .w_full()
            .child(
                h_flex()
                    .id(("subagent-tool", ix))
                    .w_full()
                    .gap_2()
                    .py_1()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(rows) = &mut this.rows
                            && let Some(row) = rows.get_mut(ix)
                        {
                            row.expanded = !row.expanded;
                        }
                        cx.notify();
                    }))
                    .child(
                        Icon::new(AssetsIconName::Wrench)
                            .size_3p5()
                            .text_color(subtlest),
                    )
                    .child(
                        div().flex_shrink_0().text_sm().text_color(subtlest).child(
                            row.item.tool.clone().unwrap_or_else(|| {
                                rust_i18n::t!("panel.tool_fallback").to_string()
                            }),
                        ),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_sm()
                            .text_color(subtle)
                            .child(summary),
                    )
                    .child(
                        Icon::new(if row.expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .size_3p5()
                        .text_color(subtlest),
                    ),
            )
            // Expanded output: monospace height-capped card (a simplified
            // version of the thread_view tool card's expanded section)
            .when(row.expanded, |this| {
                this.when_some(row.item.output.clone(), |this, output| {
                    this.child(
                        div()
                            .id(("subagent-tool-body", ix))
                            .w_full()
                            .max_h(px(120.))
                            .overflow_y_scroll()
                            .track_scroll(&row.output_scroll)
                            .rounded_lg()
                            .border_1()
                            .border_color(cx.theme().border)
                            .bg(cx.theme().group_box)
                            .px_3()
                            .py_2()
                            .text_xs()
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_color(subtle)
                            .child(output),
                    )
                })
            })
            .into_any_element()
    }
}

impl Render for SubagentPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let subtle = cx.theme().muted_foreground;
        // Returning to the bottom (scroll wheel or dragging the scrollbar,
        // any means) auto-resumes following (same as thread_view)
        if !self.following && self.at_bottom() {
            self.following = true;
        }
        let body: AnyElement = match &self.rows {
            None => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .text_sm()
                        .text_color(subtle)
                        .child(rust_i18n::t!("common.loading")),
                )
                .into_any_element(),
            Some(rows) => v_flex()
                .w_full()
                .gap_3()
                .p_3()
                // Header: title + model subtitle
                .child(
                    v_flex()
                        .w_full()
                        .gap_0p5()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .child(self.title.clone()),
                        )
                        .when(!self.subtitle.is_empty(), |this| {
                            this.child(
                                div()
                                    .text_xs()
                                    .text_color(subtle)
                                    .child(self.subtitle.clone()),
                            )
                        }),
                )
                .children(rows.iter().enumerate().map(|(ix, row)| {
                    match row.item.role.as_str() {
                        // user row: small "Task" label + body text
                        "user" => v_flex()
                            .w_full()
                            .gap_1()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(subtle)
                                    .child(rust_i18n::t!("panel.task_label")),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().foreground)
                                    .child(row.item.text.clone()),
                            )
                            .into_any_element(),
                        // tool row: expandable
                        "tool" => self.render_tool_row(ix, row, cx),
                        // assistant row: markdown rendering
                        _ => match &row.markdown {
                            Some(state) => TextView::new(state)
                                .selectable(true)
                                .text_sm()
                                .into_any_element(),
                            None => div()
                                .text_sm()
                                .text_color(cx.theme().foreground)
                                .child(row.item.text.clone())
                                .into_any_element(),
                        },
                    }
                }))
                // Bottom "running" indicator (disappears on finished when
                // the subagent ends)
                .when(self.running, |this| {
                    this.child(
                        h_flex()
                            .gap_2()
                            .child(Spinner::new().small().color(subtle))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(subtle)
                                    .child(rust_i18n::t!("panel.subagent_running")),
                            ),
                    )
                })
                .into_any_element(),
        };
        div()
            .relative()
            .size_full()
            .child(
                div()
                    .id("subagent-panel-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, window, cx| {
                        let delta = event.delta.pixel_delta(window.line_height());
                        // User scrolls up: pause following and pop up the
                        // "latest message" button (this event is not
                        // swallowed, the list still scrolls). When the
                        // content is not overflowing (not scrollable),
                        // scrolling up does not pause: otherwise a small
                        // scroll in a short panel would also pop up the
                        // button
                        if delta.y > px(0.) && this.following && this.scroll.max_offset().y > px(0.)
                        {
                            this.following = false;
                            cx.notify();
                        }
                        // Swallow the scroll wheel when scrollable so it does
                        // not pass through to the three-pane layout or the
                        // main message stream outside the panel
                        if this.scroll.max_offset().y > px(0.) {
                            cx.stop_propagation();
                        }
                    }))
                    .child(body),
            )
            // When not following, pop up the "latest message" button:
            // clicking returns to the bottom and resumes following
            .when(!self.following, |this| {
                this.child(
                    div().absolute().bottom_3().right_3().child(
                        h_flex()
                            .id("subagent-latest-fab")
                            .items_center()
                            .gap_1()
                            .px_3()
                            .py_1p5()
                            .rounded_full()
                            .bg(cx.theme().popover)
                            .border_1()
                            .border_color(cx.theme().border)
                            .shadow_md()
                            .cursor_pointer()
                            // The default hitbox does not block the layer
                            // below: clicks would pass through to the panel
                            // content — block the pass-through while the
                            // scroll wheel still reaches the list
                            .block_mouse_except_scroll()
                            .child(
                                Icon::new(AssetsIconName::ArrowDown)
                                    .size_3p5()
                                    .text_color(cx.theme().foreground),
                            )
                            .child(div().text_xs().child(rust_i18n::t!("panel.latest")))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.following = true;
                                this.scroll.scroll_to_bottom();
                                cx.notify();
                            })),
                    ),
                )
            })
            .into_any_element()
    }
}
