//! "Task output" tab of the right panel: a terminal-style view of a background
//! Bash task's output. Data flow: task list row click → AppView
//! `open_task_tab` → this panel reads the spill file
//! `{cwd}/.pigcode/tool-results/{id}.log` (core persists the task's FULL output
//! there; session.jsonl only carries the backgrounding receipt, and the task
//! registry itself is not persisted). Refreshes: on open, on every
//! TaskListChanged (status transitions), and a 2s poll while Running
//! (TaskListChanged does not stream output growth). Fallback while the spill is
//! unreadable: the `output_tail` excerpt from the latest TaskSummary.

use gpui_kit::assets::IconName as AssetsIconName;
use gpui_kit::base::{Scrollbar, SelectableText};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use pig_protocol::{TaskStatus, TaskSummary};

use crate::code_view::CODE_LINE_H;

/// The spill keeps the FULL output (up to core's 10MB cap); rendering all of it
/// in one text block is pointlessly heavy, so the panel shows the last 128KB
/// (terminal tail semantics) with an omission note.
const SPILL_TAIL_BYTES: usize = 128 * 1024;

/// Poll cadence while the task is Running (the watcher only notifies on exit,
/// so output growth between transitions is picked up here).
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

pub struct TaskOutputPanel {
    session_id: String,
    task_id: String,
    command: String,
    status: TaskStatus,
    started_at: u64,
    ended_at: Option<u64>,
    /// `{cwd}/.pigcode/tool-results/{id}.log`
    spill: std::path::PathBuf,
    /// Latest output_tail excerpt (fallback while the spill is unreadable)
    tail: String,
    state: LoadState,
    /// Output card scroll (tails to the bottom while Running)
    scroll: ScrollHandle,
    /// Command card scroll (its own height cap + internal scroll)
    cmd_scroll: ScrollHandle,
    /// Per-card copy-button flash state
    cmd_copy: CopyFlash,
    out_copy: CopyFlash,
    /// 2s poll task while Running (dropped when the panel/entity drops)
    poll: Option<Task<()>>,
}

/// Which card a copy button belongs to (routes the click handler to the
/// matching CopyFlash)
#[derive(Clone, Copy)]
enum CopyWhich {
    Cmd,
    Out,
}

/// A copy button's flash state: the icon swaps to a check + success color on
/// click, reverting after 1.2s (same as the message action row's Copy); the
/// generation guards rapid clicks (a mismatched generation at expiry is
/// discarded)
#[derive(Default)]
struct CopyFlash {
    copied: bool,
    generation: u64,
}

enum LoadState {
    Loading,
    Ok {
        text: String,
        /// Whole lines dropped by the tail cap (shown as an omission note)
        omitted: usize,
    },
    /// Spill unreadable (missing/unreadable): the panel shows the tail excerpt
    Fallback,
}

/// Terminal tail slice: cut the byte buffer to its last `cap` bytes, rounded
/// up to a whole line so the first shown line is not a fragment; returns the
/// text plus the number of dropped lines (a head without any newline counts as
/// one dropped partial line).
pub(crate) fn spill_tail(bytes: &[u8], cap: usize) -> (String, usize) {
    if bytes.len() <= cap {
        return (String::from_utf8_lossy(bytes).into_owned(), 0);
    }
    let head = &bytes[..bytes.len() - cap];
    let start = match head.iter().rposition(|&b| b == b'\n') {
        Some(i) => i + 1,
        None => head.len(),
    };
    let omitted = head[..start].iter().filter(|&&b| b == b'\n').count().max(1);
    (
        String::from_utf8_lossy(&bytes[start..]).into_owned(),
        omitted,
    )
}

impl TaskOutputPanel {
    pub fn new(session_id: String, task: &TaskSummary, spill: std::path::PathBuf) -> Self {
        Self {
            session_id,
            task_id: task.id.clone(),
            command: task.command.clone(),
            status: task.status,
            started_at: task.started_at,
            ended_at: task.ended_at,
            spill,
            tail: task.output_tail.clone(),
            state: LoadState::Loading,
            scroll: ScrollHandle::new(),
            cmd_scroll: ScrollHandle::new(),
            cmd_copy: CopyFlash::default(),
            out_copy: CopyFlash::default(),
            poll: None,
        }
    }

    /// For self-tests / event-side session matching (task ids are per-session
    /// sequences like "b1" and collide across sessions).
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Create + arm the poll (open_task_tab's create branch): a tab opened
    /// while the task is Running gets no TaskListChanged until the task
    /// exits, so without the poll the output card would never refresh.
    pub fn new_armed(
        session_id: String,
        task: &TaskSummary,
        spill: std::path::PathBuf,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut panel = Self::new(session_id, task, spill);
        panel.arm_poll(cx);
        panel
    }

    /// For self-tests.
    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    /// Tab/header title (the raw command; truncated by the tab bar).
    pub fn title(&self) -> &str {
        &self.command
    }

    /// Fresh metadata from a TaskListChanged snapshot; a status change also
    /// refreshes the spill (exit appends the final output). The poll is (re)
    /// armed only while Running.
    pub fn update_meta(&mut self, task: &TaskSummary, cx: &mut Context<Self>) {
        let status_changed = self.status != task.status;
        self.command = task.command.clone();
        self.status = task.status;
        self.started_at = task.started_at;
        self.ended_at = task.ended_at;
        self.tail = task.output_tail.clone();
        if status_changed {
            self.reload(cx);
        }
        self.arm_poll(cx);
    }

    /// (Re)read the spill file in the background; the panel tails to the
    /// bottom while the task is Running.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.state = LoadState::Loading;
        let spill = self.spill.clone();
        cx.spawn(async move |this, cx| {
            let bytes = cx
                .background_executor()
                .spawn(async move { std::fs::read(&spill).ok() })
                .await;
            let _ = this.update(cx, |panel, cx| {
                panel.state = match bytes {
                    Some(bytes) => {
                        let (text, omitted) = spill_tail(&bytes, SPILL_TAIL_BYTES);
                        LoadState::Ok { text, omitted }
                    }
                    None => LoadState::Fallback,
                };
                if matches!(panel.status, TaskStatus::Running) {
                    panel.scroll.scroll_to_bottom();
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// 2s poll while Running (TaskListChanged fires only on status
    /// transitions, not on output growth). A new task replaces the old loop;
    /// the loop exits once the task is no longer Running or the panel drops.
    ///
    /// Callers: new_armed (a tab opened while the task is Running must poll
    /// too — no TaskListChanged fires until a transition) and update_meta.
    fn arm_poll(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.status, TaskStatus::Running) {
            self.poll = None;
            return;
        }
        if self.poll.is_some() {
            return;
        }
        self.poll = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL_INTERVAL).await;
                let Ok(still_running) = this.update(cx, |panel, cx| {
                    if matches!(panel.status, TaskStatus::Running) {
                        panel.reload(cx);
                    }
                    matches!(panel.status, TaskStatus::Running)
                }) else {
                    break;
                };
                if !still_running {
                    break;
                }
            }
        }));
    }

    fn flash(&self, which: CopyWhich) -> &CopyFlash {
        match which {
            CopyWhich::Cmd => &self.cmd_copy,
            CopyWhich::Out => &self.out_copy,
        }
    }

    fn flash_mut(&mut self, which: CopyWhich) -> &mut CopyFlash {
        match which {
            CopyWhich::Cmd => &mut self.cmd_copy,
            CopyWhich::Out => &mut self.out_copy,
        }
    }

    /// A card's copy button (revealed on card hover by card_shell): writes the
    /// payload to the clipboard and flashes a success check for 1.2s; an empty
    /// payload is a no-op (same as the Bash tool card's copy)
    fn copy_button(
        &self,
        key: &'static str,
        which: CopyWhich,
        payload: String,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        Button::new(key)
            .ghost()
            .xsmall()
            .icon(if self.flash(which).copied {
                IconName::CircleCheck
            } else {
                IconName::Copy
            })
            .when(self.flash(which).copied, |this| {
                this.text_color(cx.theme().success)
            })
            .tooltip(rust_i18n::t!("common.copy"))
            .on_click(cx.listener(move |this, _, _, cx| {
                if payload.is_empty() {
                    return;
                }
                cx.write_to_clipboard(ClipboardItem::new_string(payload.clone()));
                {
                    let flash = this.flash_mut(which);
                    flash.copied = true;
                    flash.generation += 1;
                }
                let generation = this.flash(which).generation;
                cx.spawn(async move |this, cx| {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(1200))
                        .await;
                    let _ = this.update(cx, |this, cx| {
                        let flash = this.flash_mut(which);
                        if flash.generation == generation {
                            flash.copied = false;
                            cx.notify();
                        }
                    });
                })
                .detach();
                cx.notify();
            }))
            .into_any_element()
    }
}

/// Command card height cap (long commands scroll inside their card, same
/// cap as the Bash tool card's command half)
const CMD_CARD_MAX_H: f32 = 160.;

/// Rounded card shell in the house style (same construction as the Bash tool
/// card): border + secondary fill + a vertical scrollbar overlay, corners
/// gathered by patches because gpui's ContentMask only clips rectangles
/// (content and the scrollbar would bleed past the rounded stroke); the
/// rounded border is re-stroked on top afterwards. fill=true makes the card
/// take the remaining panel height (the output card). `corner` is an optional
/// floating top-right button (the copy button), hidden until the card is
/// hovered (group/group_hover, the same reveal as the tool cards' fold
/// arrows); it carries its own fill + border so scrolling text does not show
/// through, and sits clear of the 16px scrollbar track at the right edge.
fn card_shell(
    body: AnyElement,
    scroll: &ScrollHandle,
    fill: bool,
    group: &str,
    corner: Option<AnyElement>,
    cx: &App,
) -> AnyElement {
    let border = cx.theme().border;
    // Behind the card = page background (the right dock paints no fill of its
    // own, same value as Root's tokens.background)
    let behind = cx.theme().background;
    let card_bg = cx.theme().secondary;
    div()
        .relative()
        .w_full()
        .group(group.to_string())
        .when(fill, |this| this.flex_1().min_h_0())
        .child(
            div()
                .w_full()
                .when(fill, |this| this.h_full())
                .rounded_xl()
                .border_1()
                .border_color(border)
                .bg(card_bg)
                .text_xs()
                .line_height(px(CODE_LINE_H))
                .font_family(cx.theme().mono_font_family.clone())
                .child(body),
        )
        .child(Scrollbar::vertical(scroll))
        .child(
            canvas(
                |bounds, window, _| (bounds, rems(0.75).to_pixels(window.rem_size())),
                move |bounds, (_, radius), window, _| {
                    crate::thread_view::ThreadView::paint_rounded_corner_patches(
                        bounds, radius, behind, window,
                    );
                },
            )
            .absolute()
            .inset_0(),
        )
        // The patches cover the corner strokes; redraw the rounded border
        .child(
            div()
                .absolute()
                .inset_0()
                .rounded_xl()
                .border_1()
                .border_color(border),
        )
        .when_some(corner, |this, corner| {
            this.child(
                div()
                    .absolute()
                    .top_2()
                    .right_5()
                    .invisible()
                    .group_hover(group.to_string(), |this| this.visible())
                    .child(
                        div()
                            .rounded_md()
                            .border_1()
                            .border_color(border)
                            .bg(card_bg)
                            .shadow_sm()
                            .child(corner),
                    ),
            )
        })
        .into_any_element()
}

impl Render for TaskOutputPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let duration =
            crate::composer::format_task_duration(self.started_at, self.ended_at.unwrap_or(now));
        let icon = match self.status {
            TaskStatus::Running => Spinner::new()
                .icon(AssetsIconName::LoaderCircle)
                .color(cx.theme().progress_bar)
                .into_any_element(),
            TaskStatus::Exited(0) => Icon::new(AssetsIconName::CircleCheck)
                .size_4()
                .text_color(cx.theme().success)
                .into_any_element(),
            TaskStatus::Exited(_) => Icon::new(AssetsIconName::TriangleAlert)
                .size_4()
                .text_color(cx.theme().warning)
                .into_any_element(),
            TaskStatus::Killed => Icon::new(AssetsIconName::TriangleAlert)
                .size_4()
                .text_color(cx.theme().muted_foreground)
                .into_any_element(),
        };
        let subtle = cx.theme().muted_foreground;
        // The currently shown output text (the copy button's payload)
        let output_text = match &self.state {
            LoadState::Ok { text, .. } => text.clone(),
            LoadState::Fallback => self.tail.clone(),
            LoadState::Loading => String::new(),
        };
        // Output card text: spill tail (plus omission note), the tail excerpt
        // while the spill is unreadable, placeholders while loading/empty —
        // real output in the bright foreground and drag-selectable
        // (SelectableText, window-level selection; placeholders stay plain),
        // placeholders muted
        let body =
            match &self.state {
                LoadState::Loading => div()
                    .w_full()
                    .text_color(subtle)
                    .child(rust_i18n::t!("panel.task_output_loading").to_string())
                    .into_any_element(),
                LoadState::Fallback if self.tail.is_empty() => div()
                    .w_full()
                    .text_color(subtle)
                    .child(rust_i18n::t!("composer.no_output_yet").to_string())
                    .into_any_element(),
                LoadState::Fallback => div()
                    .w_full()
                    .cursor_text()
                    .text_color(cx.theme().foreground)
                    .child(SelectableText::new("task-output-text", self.tail.clone()))
                    .into_any_element(),
                LoadState::Ok { text, omitted } if text.is_empty() => div()
                    .w_full()
                    .text_color(subtle)
                    .child(rust_i18n::t!("composer.no_output_yet").to_string())
                    .into_any_element(),
                LoadState::Ok { text, omitted } => v_flex()
                    .w_full()
                    .gap_2()
                    .when(*omitted > 0, |this| {
                        this.child(div().w_full().text_color(subtle).child(
                            rust_i18n::t!("panel.task_output_omitted", n = omitted).to_string(),
                        ))
                    })
                    .child(
                        div()
                            .w_full()
                            .cursor_text()
                            .text_color(cx.theme().foreground)
                            .child(SelectableText::new("task-output-text", text.clone())),
                    )
                    .into_any_element(),
            };
        // ZCode-style terminal blocks: the command echoes as the first card
        // ("$" prompt muted + the full command, its own height cap and
        // scroll), the output fills the remaining height in a second card
        let command_card = card_shell(
            div()
                .id("task-output-cmd")
                .w_full()
                .max_h(px(CMD_CARD_MAX_H))
                .overflow_y_scroll()
                .restrict_scroll_to_axis()
                .track_scroll(&self.cmd_scroll)
                .px_3()
                .py_2()
                .child(
                    h_flex()
                        .w_full()
                        .items_start()
                        .gap_2()
                        .child(div().flex_shrink_0().text_color(subtle).child("$"))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .cursor_text()
                                .text_color(cx.theme().foreground)
                                .child(SelectableText::new(
                                    "task-output-cmd-text",
                                    self.command.clone(),
                                )),
                        ),
                )
                .into_any_element(),
            &self.cmd_scroll,
            false,
            "task-output-cmd-card",
            Some(self.copy_button(
                "task-output-cmd-copy",
                CopyWhich::Cmd,
                self.command.clone(),
                cx,
            )),
            cx,
        );
        let output_card = card_shell(
            div()
                .id("task-output-body")
                .size_full()
                .overflow_y_scroll()
                .restrict_scroll_to_axis()
                .track_scroll(&self.scroll)
                .px_3()
                .py_2()
                .child(body)
                .into_any_element(),
            &self.scroll,
            true,
            "task-output-out-card",
            Some(self.copy_button("task-output-out-copy", CopyWhich::Out, output_text, cx)),
            cx,
        );
        v_flex()
            .id("task-output-panel")
            .size_full()
            .p_3()
            .gap_2()
            // Slim status row: status icon + run duration (the tab bar already
            // carries the command as its title; copying lives on the cards'
            // hover-revealed corner buttons)
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap_2()
                    .px_1()
                    .child(icon)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .text_color(subtle)
                            .child(duration),
                    ),
            )
            .child(command_card)
            .child(output_card)
    }
}

#[cfg(test)]
mod tests {
    use super::spill_tail;

    /// Tail cap: whole-line cut, dropped-line counting, small-buffer passthrough
    #[test]
    fn spill_tail_cuts_on_line_boundaries() {
        // Small buffer passes through untouched
        let small = b"one\ntwo\nthree\n";
        assert_eq!(
            spill_tail(small, 1024),
            ("one\ntwo\nthree\n".to_string(), 0)
        );
        // Cap lands inside line 4: the head's last newline ends line 3, so the
        // tail starts at line 4 and 3 lines are dropped
        let text = "l1\nl2\nl3\nl4xxxxxxxxxx\nl5\n";
        let (tail, omitted) = spill_tail(text.as_bytes(), 10);
        assert_eq!(omitted, 3);
        assert_eq!(tail, "l4xxxxxxxxxx\nl5\n");
        // Head with no newline at all: one dropped partial line
        let (tail, omitted) = spill_tail(b"abcdefghijKLMNOP", 6);
        assert_eq!(omitted, 1);
        assert_eq!(tail, "KLMNOP");
    }

    /// Regression: a tab opened while the task is Running must arm the refresh
    /// poll at creation — no TaskListChanged fires until a status transition,
    /// so without it the output card never refreshed (explicit imports: a glob
    /// would pull gpui's test macro over the built-in #[test], see the trap
    /// noted in composer/tests.rs)
    #[gpui_kit::test]
    fn poll_armed_at_open_only_while_running(cx: &mut gpui_kit::TestAppContext) {
        use super::{TaskOutputPanel, TaskSummary};
        use gpui_kit::AppContext as _;
        use pig_protocol::TaskStatus;

        let task = |status: TaskStatus| TaskSummary {
            id: "b1".to_string(),
            command: "sleep 9".to_string(),
            status,
            started_at: 0,
            ended_at: None,
            output_tail: String::new(),
            agent_id: None,
        };
        cx.update(|cx| {
            let running = cx.new(|cx| {
                TaskOutputPanel::new_armed(
                    "s1".to_string(),
                    &task(TaskStatus::Running),
                    "x".into(),
                    cx,
                )
            });
            assert!(
                running.read(cx).poll.is_some(),
                "a Running task must arm the poll at open"
            );
            let exited = cx.new(|cx| {
                TaskOutputPanel::new_armed(
                    "s1".to_string(),
                    &task(TaskStatus::Exited(0)),
                    "x".into(),
                    cx,
                )
            });
            assert!(
                exited.read(cx).poll.is_none(),
                "a finished task must not poll"
            );
        });
    }
}
