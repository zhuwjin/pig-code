use super::*;

impl Sidebar {
    /// Hover an overflowing title: start the marquee, slowly scrolling the text to
    /// the end and back
    pub(crate) fn begin_title_marquee(&mut self, session_id: &str, cx: &mut Context<Self>) {
        let Some(handle) = self.title_scrolls.borrow().get(session_id).cloned() else {
            return;
        };
        // Not overflowing (no horizontal overflow) means no scrolling needed
        if f32::from(handle.max_offset().x) <= 1.0 {
            return;
        }
        if self
            .marquee
            .as_ref()
            .is_some_and(|m| m.session_id == session_id)
        {
            return;
        }
        self.marquee = Some(TitleMarquee {
            session_id: session_id.to_string(),
            position: 0.0,
            forward: true,
            hold_ticks: MARQUEE_START_TICKS,
        });
        let session_id = session_id.to_string();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(16))
                    .await;
                match this.update(cx, |this, cx| this.tick_title_marquee(&session_id, cx)) {
                    Ok(true) => {}
                    _ => break,
                }
            }
        })
        .detach();
    }

    pub(crate) fn end_title_marquee(&mut self, session_id: &str, cx: &mut Context<Self>) {
        if self
            .marquee
            .as_ref()
            .is_some_and(|m| m.session_id == session_id)
        {
            self.marquee = None;
        }
        if let Some(handle) = self.title_scrolls.borrow().get(session_id)
            && handle.offset().x != px(0.)
        {
            handle.set_offset(point(px(0.), px(0.)));
        }
        cx.notify();
    }

    /// Advance the marquee one step; returning false means the loop should stop
    pub(crate) fn tick_title_marquee(&mut self, session_id: &str, cx: &mut Context<Self>) -> bool {
        if self
            .marquee
            .as_ref()
            .is_none_or(|m| m.session_id != session_id)
        {
            return false;
        }
        let Some(handle) = self.title_scrolls.borrow().get(session_id).cloned() else {
            self.marquee = None;
            return false;
        };
        let max = f32::from(handle.max_offset().x);
        if max <= 1.0 {
            handle.set_offset(point(px(0.), px(0.)));
            self.marquee = None;
            cx.notify();
            return false;
        }
        let marquee = self.marquee.as_mut().expect("checked above");
        if marquee.hold_ticks > 0 {
            marquee.hold_ticks -= 1;
            return true;
        }
        if marquee.forward {
            marquee.position += 1.5;
            if marquee.position >= max {
                marquee.position = max;
                marquee.forward = false;
                marquee.hold_ticks = MARQUEE_HOLD_TICKS;
            }
        } else {
            marquee.position -= 3.0;
            if marquee.position <= 0.0 {
                marquee.position = 0.0;
                marquee.forward = true;
                marquee.hold_ticks = MARQUEE_HOLD_TICKS;
            }
        }
        handle.set_offset(point(px(-marquee.position), px(0.)));
        cx.notify();
        true
    }

    /// Measure the title's single-line render width with the text system.
    ///
    /// gpui's text measurement clamps the width into the available space, leaving
    /// ScrollHandle unable to sense the overflow (measured: max_offset stays 0); the
    /// scrolling mechanism works only after the content is given an explicit true
    /// width.
    pub(crate) fn measure_title_width(title: &str, window: &Window, cx: &App) -> Pixels {
        let font_size = rems(0.875).to_pixels(window.rem_size());
        let font = Font {
            family: cx.theme().font_family.clone(),
            ..Font::default()
        };
        window
            .text_system()
            .shape_line(
                SharedString::from(title.to_string()),
                font_size,
                &[TextRun {
                    len: title.len(),
                    font,
                    color: black(),
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                }],
                None,
            )
            .width
    }

    /// Fade strip at the ends of a session title: base is the row background solid
    /// color (plain sidebar / accent when selected), tint is the hover overlay
    /// (accent 60%); after compositing it matches the row background with no color
    /// shift at the ends
    pub(crate) fn title_fade(leading: bool, base: Hsla, tint: Option<Hsla>) -> Div {
        let (from, to) = if leading {
            (base, base.opacity(0.))
        } else {
            (base.opacity(0.), base)
        };
        let fade = div()
            .absolute()
            .top_0()
            .bottom_0()
            .when(leading, |this| this.left_0())
            .when(!leading, |this| this.right_0())
            .w(px(20.))
            .bg(linear_gradient(
                90.,
                linear_color_stop(from, 0.),
                linear_color_stop(to, 1.),
            ));
        match tint {
            Some(tint) => {
                let (from, to) = if leading {
                    (tint, tint.opacity(0.))
                } else {
                    (tint.opacity(0.), tint)
                };
                fade.child(div().size_full().bg(linear_gradient(
                    90.,
                    linear_color_stop(from, 0.),
                    linear_color_stop(to, 1.),
                )))
            }
            None => fade,
        }
    }

    /// Session title area: horizontal scrolling (hover marquee) + fades at both
    /// ends. key is the title_scrolls key: single-line session rows use the session
    /// id, two-line detail rows use "pinned-{id}" (the same session's scroll state
    /// in the two row kinds does not interfere)
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_title_scroll(
        &self,
        key: &str,
        element_id: impl Into<ElementId>,
        title: &str,
        fade_base: Hsla,
        fade_tint: Option<Hsla>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Div {
        // Flatten control characters: `measure_title_width` shapes the title
        // with gpui's `shape_line`, which debug_assert-panics on embedded
        // newlines. Pre-fix title seeds (and any legacy store rows) can carry
        // raw `\n`/`\t` from multi-line first messages — the 2026-10-08 crash
        // was exactly this firing when a workspace block expanded.
        let title = title.replace(['\n', '\r', '\t'], " ");
        let title_handle = self
            .title_scrolls
            .borrow_mut()
            .entry(key.to_string())
            .or_default()
            .clone();
        let hover_key = key.to_string();
        // Explicit true width (+2px slack against font width rounding), pushing the
        // overflow to the ScrollHandle
        let title_width = Self::measure_title_width(&title, window, cx) + px(2.);
        // Fade visibility follows scroll position (same as thread_view's scrolling
        // thinking rows): the right end fades while unexposed text remains; after
        // the marquee scrolls past the start, the left end fades too
        let max = title_handle.max_offset().x;
        let offset = title_handle.offset().x;
        let hides_leading = max > px(1.) && offset < px(-1.);
        let hides_trailing = max > px(1.) && offset > px(1.) - max;
        div()
            .relative()
            .flex_1()
            .min_w_0()
            .child(
                div()
                    .id(element_id)
                    .text_sm()
                    .w_full()
                    // Two-axis scrolling instead of overflow_x_scroll: for
                    // single-axis scroll containers gpui folds the other axis's
                    // wheel delta in; with two axes the vertical wheel bubbles to
                    // the session list untouched, neither affecting the other
                    .overflow_scroll()
                    .whitespace_nowrap()
                    .track_scroll(&title_handle)
                    .on_hover(cx.listener(move |this, hovered, _, cx| {
                        if *hovered {
                            this.begin_title_marquee(&hover_key, cx);
                        } else {
                            this.end_title_marquee(&hover_key, cx);
                        }
                    }))
                    .child(div().w(title_width).child(title)),
            )
            .when(hides_leading, |this| {
                this.child(Self::title_fade(true, fade_base, fade_tint))
            })
            .when(hides_trailing, |this| {
                this.child(Self::title_fade(false, fade_base, fade_tint))
            })
    }
}
