//! Presentation of the Read tool: the summary line attachment (clickable path +
//! line count) and the expanded code card (header = file name + wrap/copy
//! buttons; body = line-number gutter + tree-sitter highlighting, line numbers
//! taken from the Read output's real file line numbers; trailing note lines
//! appended after the code verbatim).
//!
//! Output format (pig-core tool/read.rs): `{line}\t{content}` per line +
//! trailing note lines ([truncated…] / [file info…] / [warning…]). Line-less
//! outputs such as empty files/"file unchanged"/errors → is_read_code_output
//! is false and the generic tool card is the fallback.

use super::*;

/// Render row cap for the card (same measure as the diff card; click the path to
/// see the full content in the right file panel)
const MAX_CARD_ROWS: usize = 600;
/// Card body height cap
const CARD_BODY_MAX_H: f32 = 320.;

/// A first line shaped like `{digits}\t…` counts as numbered file content output
/// (zero allocation, usable per frame)
pub(crate) fn is_read_code_output(output: &str) -> bool {
    output
        .lines()
        .next()
        .and_then(|line| line.split_once('\t'))
        .is_some_and(|(no, _)| !no.is_empty() && no.bytes().all(|b| b.is_ascii_digit()))
}

/// The Read output's first line number (scroll positioning when clicking the
/// path opens the file panel)
pub(crate) fn read_output_first_line(output: &str) -> Option<usize> {
    let (no, _) = output.lines().next()?.split_once('\t')?;
    if no.is_empty() || !no.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    no.parse().ok()
}

/// The summary line's "N lines" count: the number of lines with a numeric
/// prefix (zero-allocation scan; 0 for non-content output)
pub(crate) fn read_output_line_count(output: &str) -> usize {
    output
        .lines()
        .filter(|line| {
            line.split_once('\t')
                .is_some_and(|(no, _)| !no.is_empty() && no.bytes().all(|b| b.is_ascii_digit()))
        })
        .count()
}

/// Full-text parsing: `{line}\t{content}` lines go into lines, blank lines are
/// skipped, and the rest (trailing notes) go into notes. Note a content line
/// itself can start with "digits+tab" (the file content is like that);
/// split_once splitting only at the first tab is naturally correct; an empty
/// content line is `{no}\t` (empty body)
pub(crate) fn parse_read_output(output: &str) -> Option<ParsedReadOutput> {
    let mut lines = Vec::new();
    let mut notes = Vec::new();
    for line in output.lines() {
        match line.split_once('\t') {
            Some((no, body)) if !no.is_empty() && no.bytes().all(|b| b.is_ascii_digit()) => {
                lines.push((no.parse().ok()?, body.to_string()));
            }
            _ if line.trim().is_empty() => {}
            _ => notes.push(line.to_string()),
        }
    }
    (!lines.is_empty()).then_some(ParsedReadOutput { lines, notes })
}

/// Result of parse_read_output
pub(crate) struct ParsedReadOutput {
    /// (file line number, line content)
    pub lines: Vec<(usize, String)>,
    /// Trailing note lines ([truncated…]/[file info…]/[warning…])
    pub notes: Vec<String>,
}

/// Parse output + assemble highlight text + tree-sitter highlight + measure max
/// line width (result cached in ReadCardUi; theme changes are rebuilt by the
/// caller via Arc equality)
fn build_card_content(
    output: &str,
    path: &str,
    theme: &std::sync::Arc<gpui_kit::component::highlighter::HighlightTheme>,
    window: &Window,
    cx: &App,
) -> Option<ReadCardContent> {
    let ParsedReadOutput { lines, notes } = parse_read_output(output)?;
    let code = lines
        .iter()
        .map(|(_, body)| body.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let highlighted = highlight_code(&code, lang_name_for_path(path), theme);
    let max_line_width = measure_max_line_width(&code, &highlighted, window, cx);
    Some(ReadCardContent {
        lines,
        notes,
        code,
        highlighted,
        max_line_width,
    })
}

impl ThreadView {
    /// The Read tool's expanded code card (same as ZCode): rounded bordered card;
    /// header = file name + wrap/copy buttons; body = line-number gutter (real
    /// file line numbers) + tree-sitter highlighted lines. No wrapping by
    /// default (horizontal scroll); click the wrap button to toggle soft wrap.
    /// Height-capped with internal scrolling; truncated with a note beyond
    /// MAX_CARD_ROWS. The four corners are rounded by card-colored patches
    /// (same as the diff card).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_read_card(
        &self,
        message_ix: usize,
        segment_ix: usize,
        path: &str,
        output: &str,
        ui: &ReadCardUi,
        body_scroll: &ScrollHandle,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = message_ix * 1024 + segment_ix;
        let theme = cx.theme().highlight_theme.clone();
        let content = {
            let mut cache = ui.cache.borrow_mut();
            let stale = cache
                .as_ref()
                .is_none_or(|c| !std::sync::Arc::ptr_eq(&c.highlighted.theme, &theme));
            if stale {
                *cache = build_card_content(output, path, &theme, window, cx).map(std::rc::Rc::new);
            }
            cache.clone()
        };
        let Some(content) = content else {
            return div().into_any_element();
        };

        let border = cx.theme().border;
        let card_bg = cx.theme().secondary;
        // Behind the card = page background (the message area itself is
        // transparent, same value as Root's tokens.background)
        let behind = cx.theme().background;
        let subtle = cx.theme().muted_foreground;
        let subtlest = subtle.opacity(0.6);
        let (_, name) = split_path(path);

        // Header: file name + wrap toggle + copy (hover-only buttons are the
        // convention elsewhere, but here they are always visible — ZCode's read
        // card header buttons are always visible too)
        let header = h_flex()
            .w_full()
            .pl_3()
            .pr_2()
            .py_1p5()
            .gap_1()
            .border_b_1()
            .border_color(border)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_sm()
                    .text_color(cx.theme().foreground)
                    .child(name),
            )
            .child(
                Button::new(("read-wrap", key))
                    .ghost()
                    .xsmall()
                    .icon(AssetIconName::TextWrap)
                    .when(ui.wrap, |this| this.text_color(cx.theme().foreground))
                    .tooltip(rust_i18n::t!("thread.wrap"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(Segment::ToolCall {
                            read_ui: Some(ui), ..
                        }) = this
                            .messages
                            .get_mut(message_ix)
                            .and_then(|m| m.segments.get_mut(segment_ix))
                        {
                            ui.wrap = !ui.wrap;
                        }
                        cx.notify();
                    })),
            )
            .child(
                Button::new(("read-copy", key))
                    .ghost()
                    .xsmall()
                    .icon(if ui.copied {
                        IconName::CircleCheck
                    } else {
                        IconName::Copy
                    })
                    .when(ui.copied, |this| this.text_color(cx.theme().success))
                    .tooltip(rust_i18n::t!("common.copy"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(Segment::ToolCall {
                            read_ui: Some(ui), ..
                        }) = this
                            .messages
                            .get_mut(message_ix)
                            .and_then(|m| m.segments.get_mut(segment_ix))
                        {
                            let text = ui
                                .cache
                                .borrow()
                                .as_ref()
                                .map(|content| content.code.clone())
                                .unwrap_or_default();
                            if !text.is_empty() {
                                cx.write_to_clipboard(ClipboardItem::new_string(text));
                                ui.copied = true;
                            }
                        }
                        cx.notify();
                    })),
            );

        // Body rows (line numbers = the real file line numbers from the Read output)
        let total = content.lines.len();
        let shown = total.min(MAX_CARD_ROWS);
        let gutter_w = gutter_width(content.lines.last().map(|(no, _)| *no).unwrap_or(1));
        let mut rows: Vec<AnyElement> = (0..shown)
            .map(|ix| {
                code_line_row(
                    content.lines[ix].0,
                    content.highlighted.line_text(&content.code, ix),
                    content.highlighted.line_styles(ix),
                    gutter_w,
                    subtlest,
                    ui.wrap,
                )
            })
            .collect();
        // Trailing notes ([truncated…]/[file info…]) plus the over-cap omission note
        for note in &content.notes {
            rows.push(
                div()
                    .w_full()
                    .px_3()
                    .py_1()
                    .text_color(subtlest)
                    .child(note.clone())
                    .into_any_element(),
            );
        }
        if total > shown {
            rows.push(
                div()
                    .w_full()
                    .py_1()
                    .text_center()
                    .text_color(subtlest)
                    .child(rust_i18n::t!("thread.read_omitted", n = total - shown).to_string())
                    .into_any_element(),
            );
        }

        let body = div()
            .id(("read-body", key))
            .w_full()
            .max_h(px(CARD_BODY_MAX_H))
            .overflow_y_scroll()
            // Lock the wheel to the gesture axis: by default gpui maps vertical
            // wheel delta onto x-only scrollable containers (y→x) and horizontal
            // delta onto y-only scrollable containers (x→y) — without the lock
            // one wheel move scrolls both axes. Locked: the vertical wheel only
            // scrolls vertically, horizontal (Shift+wheel/trackpad swipe) only
            // horizontally
            .restrict_scroll_to_axis()
            .track_scroll(body_scroll)
            // No wrap: content gets an explicit measured width + horizontal
            // scrolling (without an explicit width layout clamps it into the
            // available space and horizontal scrolling breaks — see
            // code_view::measure_max_line_width); wrapped: content fits the card
            // width
            .child(if ui.wrap {
                v_flex().w_full().children(rows).into_any_element()
            } else {
                // Row = gutter + code cell (pl_3 + text + pr_3); reserve a
                // horizontal scrollbar lane at the bottom
                let content_w = gutter_w + px(24.) + content.max_line_width;
                div()
                    .id(("read-body-x", key))
                    .overflow_x_scroll()
                    .restrict_scroll_to_axis()
                    .track_scroll(&ui.h_scroll)
                    .child(
                        v_flex()
                            .w(content_w)
                            .pb(px(CODE_SCROLLBAR_LANE))
                            .children(rows),
                    )
                    .into_any_element()
            });

        div()
            .relative()
            .w_full()
            .child(
                v_flex()
                    .w_full()
                    .rounded_xl()
                    .border_1()
                    .border_color(border)
                    .bg(card_bg)
                    .text_xs()
                    .line_height(px(CODE_LINE_H))
                    .font_family(cx.theme().mono_font_family.clone())
                    .child(header)
                    .child(
                        // Scrollbar tucked into the body area (not reaching the
                        // header); corners rounded by patches
                        div()
                            .relative()
                            .w_full()
                            .child(body)
                            .child(Scrollbar::vertical(body_scroll))
                            .when(!ui.wrap, |this| {
                                // Horizontal scrollbar always visible (in
                                // Scrolling mode it fades out after scrolling,
                                // leaving mouse users without their only
                                // horizontal entry — the vertical wheel never
                                // maps to horizontal; only the bar/Shift+wheel/
                                // trackpad work)
                                this.child(
                                    Scrollbar::horizontal(&ui.h_scroll).mode(ScrollbarMode::Always),
                                )
                            }),
                    ),
            )
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
            // The patches cover the corner strokes; redraw the rounded border
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
}
