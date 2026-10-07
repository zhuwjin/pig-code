//! Bash tool card (same as ZCode): expanded it is two stacked code cards — the
//! command card (header "Bash" + wrap/copy buttons; the command gets bash
//! syntax highlighting) and the output card (header "Output" + wrap/copy
//! buttons; the output is plain text without highlighting, red on failure).
//! No wrapping by default (horizontal scrolling + always-visible horizontal
//! scrollbar); the two cards' toggles are independent. While running or
//! awaiting approval the generic tool card is used (live output); the code
//! cards take over once done (including failure).

use super::*;

/// Command card body height cap
const CMD_MAX_H: f32 = 160.;
/// Output card body height cap
const OUT_MAX_H: f32 = 320.;
/// Per-card render row cap (same measure as the Read card)
const MAX_CARD_ROWS: usize = 600;

/// Command card / output card (listeners route to the matching state field by it)
#[derive(Clone, Copy)]
enum SubCard {
    Cmd,
    Out,
}

impl ThreadView {
    /// The Bash tool's expanded area: command card + output card stacked
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_bash_card(
        &self,
        message_ix: usize,
        segment_ix: usize,
        command: &str,
        output: &str,
        is_error: bool,
        ui: &BashCardUi,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Content cache: theme changes recompute via Arc equality (the output
        // is "text" plain text, only width measuring costs)
        let theme = cx.theme().highlight_theme.clone();
        let content = {
            let mut cache = ui.cache.borrow_mut();
            let stale = cache
                .as_ref()
                .is_none_or(|c| !std::sync::Arc::ptr_eq(&c.cmd.highlighted.theme, &theme));
            if stale {
                *cache = Some(std::rc::Rc::new(BashCardContent {
                    cmd: PreparedCode::build(command.to_string(), "bash", &theme, window, cx),
                    out: PreparedCode::build(output.to_string(), "text", &theme, window, cx),
                }));
            }
            cache.clone().expect("cache was just populated")
        };

        v_flex()
            .w_full()
            .gap_2()
            .child(self.render_bash_subcard(
                ("bash-cmd", message_ix * 1024 + segment_ix),
                "Bash",
                &content.cmd,
                false,
                CMD_MAX_H,
                ui.cmd_wrap,
                ui.cmd_copied,
                &ui.cmd_scroll,
                &ui.cmd_h_scroll,
                SubCard::Cmd,
                message_ix,
                segment_ix,
                cx,
            ))
            .child(self.render_bash_subcard(
                ("bash-out", message_ix * 1024 + segment_ix),
                rust_i18n::t!("thread.bash_output").as_ref(),
                &content.out,
                is_error,
                OUT_MAX_H,
                ui.out_wrap,
                ui.out_copied,
                &ui.out_scroll,
                &ui.out_h_scroll,
                SubCard::Out,
                message_ix,
                segment_ix,
                cx,
            ))
            .into_any_element()
    }

    /// One subcard: header (title + wrap/copy) + body (monospace rows, no
    /// line-number gutter; without wrapping it gets an explicit measured width
    /// plus horizontal scrolling, scrollbars built in and rounded with the
    /// corner patches)
    #[allow(clippy::too_many_arguments)]
    fn render_bash_subcard(
        &self,
        key: (&str, usize),
        title: &str,
        content: &PreparedCode,
        is_error: bool,
        max_h: f32,
        wrap: bool,
        copied: bool,
        v_scroll: &ScrollHandle,
        h_scroll: &ScrollHandle,
        which: SubCard,
        message_ix: usize,
        segment_ix: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let border = cx.theme().border;
        let card_bg = cx.theme().secondary;
        // Behind the card = page background (the message area itself is
        // transparent, same value as Root's tokens.background)
        let behind = cx.theme().background;
        let subtle = cx.theme().muted_foreground;
        let subtlest = subtle.opacity(0.6);

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
                    // Title uses the UI font (the body is the monospace one)
                    .font_family(cx.theme().font_family.clone())
                    .text_sm()
                    .text_color(cx.theme().foreground)
                    .child(title.to_string()),
            )
            .child(
                Button::new(format!("{}-wrap-{}", key.0, key.1))
                    .ghost()
                    .xsmall()
                    .icon(AssetIconName::TextWrap)
                    .when(wrap, |this| this.text_color(cx.theme().foreground))
                    .tooltip(rust_i18n::t!("thread.wrap"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(Segment::ToolCall {
                            bash_ui: Some(ui), ..
                        }) = this
                            .messages
                            .get_mut(message_ix)
                            .and_then(|m| m.segments.get_mut(segment_ix))
                        {
                            match which {
                                SubCard::Cmd => ui.cmd_wrap = !ui.cmd_wrap,
                                SubCard::Out => ui.out_wrap = !ui.out_wrap,
                            }
                        }
                        cx.notify();
                    })),
            )
            .child(
                Button::new(format!("{}-copy-{}", key.0, key.1))
                    .ghost()
                    .xsmall()
                    .icon(if copied {
                        IconName::CircleCheck
                    } else {
                        IconName::Copy
                    })
                    .when(copied, |this| this.text_color(cx.theme().success))
                    .tooltip(rust_i18n::t!("common.copy"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(Segment::ToolCall {
                            bash_ui: Some(ui), ..
                        }) = this
                            .messages
                            .get_mut(message_ix)
                            .and_then(|m| m.segments.get_mut(segment_ix))
                        {
                            let text = ui
                                .cache
                                .borrow()
                                .as_ref()
                                .map(|c| match which {
                                    SubCard::Cmd => c.cmd.code.clone(),
                                    SubCard::Out => c.out.code.clone(),
                                })
                                .unwrap_or_default();
                            if !text.is_empty() {
                                cx.write_to_clipboard(ClipboardItem::new_string(text));
                                match which {
                                    SubCard::Cmd => ui.cmd_copied = true,
                                    SubCard::Out => ui.out_copied = true,
                                }
                            }
                        }
                        cx.notify();
                    })),
            );

        // Body rows (no line numbers)
        let total = content.line_count();
        let shown = total.min(MAX_CARD_ROWS);
        let text_color = if is_error {
            cx.theme().danger
        } else {
            cx.theme().foreground
        };
        let mut rows: Vec<AnyElement> = (0..shown)
            .map(|ix| code_line(content.line_text(ix), content.line_styles(ix), wrap))
            .collect();
        if total == 0 || (total == 1 && content.line_text(0).is_empty()) {
            rows = vec![
                div()
                    .w_full()
                    .px_3()
                    .py_1()
                    .text_color(subtlest)
                    .child(rust_i18n::t!("thread.no_output").to_string())
                    .into_any_element(),
            ];
        }
        if total > shown {
            rows.push(
                div()
                    .w_full()
                    .py_1()
                    .text_center()
                    .text_color(subtlest)
                    .child(rust_i18n::t!("thread.omitted_lines", n = total - shown).to_string())
                    .into_any_element(),
            );
        }

        let body = div()
            .id(format!("{}-body-{}", key.0, key.1))
            .w_full()
            .max_h(px(max_h))
            .overflow_y_scroll()
            // Lock the wheel to the gesture axis (same as the Read card): the
            // vertical wheel only scrolls vertically, horizontal only horizontally
            .restrict_scroll_to_axis()
            .track_scroll(v_scroll)
            .text_color(text_color)
            .child(if wrap {
                v_flex().w_full().children(rows).into_any_element()
            } else {
                // No gutter: content width = code cell padding (24) + max line
                // width; reserve a horizontal scrollbar lane at the bottom
                let content_w = px(24.) + content.max_line_width;
                div()
                    .id(format!("{}-body-x-{}", key.0, key.1))
                    .overflow_x_scroll()
                    .restrict_scroll_to_axis()
                    .track_scroll(h_scroll)
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
            // Scroll chaining: swallow the wheel when this card's content can
            // scroll (the Bash card does not use the shared body_scroll
            // fallback in cards.rs — subcard handles are independent); chain
            // through to the outer message list when it cannot, consistent with
            // other tool cards
            .on_scroll_wheel(consume_scroll(v_scroll))
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
                            .child(Scrollbar::vertical(v_scroll))
                            .when(!wrap, |this| {
                                // Always visible: idle fade-out would leave
                                // mouse users without their only horizontal entry
                                this.child(
                                    Scrollbar::horizontal(h_scroll).mode(ScrollbarMode::Always),
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
