use super::*;

fn tag_for(kind: Popup) -> &'static str {
    match kind {
        Popup::Mention => "mention",
        Popup::Slash => "slash",
        Popup::ExecMode => "exec",
        Popup::Model => "model",
        Popup::Reasoning => "reasoning",
        Popup::Cwd => "cwd",
        Popup::Branch => "branch",
        Popup::Context => "context",
        Popup::Todos => "todos",
        Popup::Tasks => "tasks",
        Popup::AgentTasks => "agent-tasks",
    }
}

impl Composer {
    /// Context capacity panel: title + usage/ratio + progress bar + average cache
    /// hit rate, anchored centered right above the indicator chip (shown on hover).
    pub(crate) fn render_context_popup(&self, cx: &mut Context<Self>) -> AnyElement {
        let (used, total, cache_read_total, input_total) =
            self.context_usage.unwrap_or((0, 1, 0, 0));
        let ratio = (used as f32 / total as f32).clamp(0.0, 1.0);
        let bar_color = if ratio > 0.8 {
            cx.theme().warning
        } else {
            cx.theme().progress_bar
        };
        let cache_total = cache_read_total + input_total;

        let content = v_flex()
            .w_full()
            .gap_2()
            .rounded(cx.theme().radius_lg)
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().popover)
            .p_3()
            .child(
                h_flex()
                    .w_full()
                    .child(
                        div()
                            .text_sm()
                            .font_medium()
                            .child(rust_i18n::t!("composer.context_capacity")),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!(
                                "{} / {} ({:.1}%)",
                                Self::format_tokens_compact(used),
                                Self::format_tokens_compact(total),
                                ratio * 100.0
                            )),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .h(px(6.))
                    .rounded_full()
                    .bg(cx.theme().muted_foreground.opacity(0.15))
                    .when(used > 0, |this| {
                        this.child(
                            div()
                                .h_full()
                                .w(relative(ratio))
                                // Keep a visible sliver even for tiny ratios (0.1% is
                                // only 0.3px and would be rounded away)
                                .min_w(px(3.))
                                .rounded_full()
                                .bg(bar_color),
                        )
                    }),
            )
            .when(cache_total > 0, |this| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(
                            rust_i18n::t!(
                                "composer.cache_hit_rate",
                                rate = cache_read_total as f64 / cache_total as f64 * 100.0 : {:.1},
                                hit = Self::format_tokens_compact(cache_read_total),
                                total = Self::format_tokens_compact(cache_total),
                            )
                            .to_string(),
                        ),
                )
            })
            .into_any_element();
        self.popup_shell(
            "composer-context-popup",
            content,
            PopupAnchor::Center,
            None,
            cx,
        )
    }

    pub(crate) fn popup_query(&self, cx: &App) -> Option<(Popup, usize, String)> {
        let (kind, start) = self.popup?;
        match kind {
            Popup::Mention | Popup::Slash => {
                let value = self.input.read(cx).value();
                let caret = self.input.read(cx).selected_range().start.min(value.len());
                if caret < start + 1 {
                    return None;
                }
                Some((kind, start, value[start + 1..caret].to_string()))
            }
            _ => Some((kind, start, String::new())),
        }
    }

    pub(crate) fn insert_file(
        &mut self,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((Popup::Mention, start)) = self.popup else {
            return;
        };
        let caret = self.input.read(cx).selected_range().start;
        // The document text is the full @path (files are collected from it on send);
        // the display text shows only the file name
        let file_name = path.rsplit('/').next().unwrap_or(path.as_str());
        let token =
            InlineToken::new(path.clone(), format!("@{path}")).with_label(format!("@{file_name}"));
        self.input.update(cx, |input, cx| {
            if input
                .replace_range_with_token(start..caret, token, window, cx)
                .is_ok()
            {
                // The token API adds no separator: after insertion the selection
                // collapses at the token's tail, so append a trailing space
                input.replace(" ", window, cx);
            } else {
                // Fall back to plain-text insertion when token validation fails, etc.
                input.set_selected_range(start..caret, cx);
                input.replace(format!("@{path} "), window, cx);
            }
            input.focus(window, cx);
        });
        self.popup = None;
        cx.notify();
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_list_item(
        &self,
        id: impl Into<ElementId>,
        icon: IconName,
        label: String,
        detail: Option<String>,
        selected: bool,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        h_flex()
            .id(id)
            .w_full()
            .gap_2()
            .px_3()
            .py_1()
            .cursor_pointer()
            .rounded(cx.theme().radius)
            .when(selected, |this| this.bg(cx.theme().accent))
            .hover(|this| this.bg(cx.theme().accent))
            .on_click(on_click)
            .child(
                Icon::new(icon)
                    .size_4()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(div().text_sm().child(label))
            .when_some(detail, |this, detail| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(detail),
                )
            })
            .into_any_element()
    }

    pub(crate) fn render_popup(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (kind, _start, query) = self.popup_query(cx)?;
        let items: Vec<AnyElement> = match kind {
            Popup::Mention => {
                let _ = query;
                self.mention_results
                    .iter()
                    .enumerate()
                    .map(|(ix, path)| {
                        let path = path.clone();
                        self.render_list_item(
                            ("mention", ix),
                            IconName::FileText,
                            path.clone(),
                            None,
                            ix == self.popup_sel,
                            cx.listener(move |this, _, window, cx| {
                                this.insert_file(path.clone(), window, cx);
                            }),
                            cx,
                        )
                    })
                    .collect()
            }
            Popup::Slash => Self::slash_filtered(&query)
                .into_iter()
                .enumerate()
                .map(|(ix, (name, desc))| {
                    self.render_list_item(
                        ("slash", ix),
                        IconName::SquareTerminal,
                        name.to_string(),
                        Some(desc.to_string()),
                        ix == self.popup_sel,
                        cx.listener(move |this, _, window, cx| {
                            this.stage_command(name, window, cx);
                        }),
                        cx,
                    )
                })
                .collect(),
            Popup::ExecMode
            | Popup::Cwd
            | Popup::Branch
            | Popup::Model
            | Popup::Reasoning
            | Popup::Context
            | Popup::Todos
            | Popup::Tasks
            | Popup::AgentTasks => {
                unreachable!(
                    "Cwd/Branch/ExecMode/Model/Reasoning/Context/Todos/Tasks/AgentTasks are rendered by their own dedicated panels"
                )
            }
        };
        if items.is_empty() {
            return None;
        }

        Some(
            div()
                .id("composer-popup")
                .test_support()
                .absolute()
                .bottom_full()
                .left_0()
                .right_0()
                .mb_1()
                .max_h(px(240.))
                .rounded(cx.theme().radius)
                .border_1()
                .border_color(cx.theme().border)
                .bg(cx.theme().popover)
                .py_1()
                .child(
                    div()
                        .relative()
                        .with_animation(
                            format!("popup-enter-{}", tag_for(kind)),
                            Animation::new(std::time::Duration::from_millis(150))
                                .with_easing(ease_out_quint()),
                            |el, delta| el.top(px(6.0 * (1.0 - delta))).opacity(delta),
                        )
                        .child(
                            // Rows are direct children of the scroll container: only
                            // then can the selected item scroll into view via
                            // scroll_to_item during keyboard navigation (same
                            // mechanism as the message list).
                            // overflow_y_scroll/track_scroll are
                            // StatefulInteractiveElement methods, so the scroll
                            // container must have an id; px_1 keeps the selection
                            // highlight off the popup border
                            div()
                                .id("composer-popup-scroll")
                                .max_h(px(228.))
                                .overflow_y_scroll()
                                .track_scroll(&self.popup_scroll)
                                .px_1()
                                .children(items),
                        ),
                )
                .into_any_element(),
        )
    }

    /// Popup shell: anchored right above the trigger chip, closes on outside click,
    /// with an enter animation.
    /// When `anchor` is Center, the popup's horizontal centerline aligns with the
    /// chip's centerline (fixed width 360); other popups size to content (160-360).
    /// `hover_clear`: Command panels are "hover to select"; after the mouse leaves
    /// the panel the selected row's highlight lingers, so passing the corresponding
    /// CommandState clears the selection when the pointer leaves the panel.
    pub(crate) fn popup_shell(
        &self,
        id: &'static str,
        content: AnyElement,
        anchor: PopupAnchor,
        hover_clear: Option<Entity<CommandState>>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id(id)
            .absolute()
            .bottom_full()
            .mb_2()
            .map(|this| match anchor {
                PopupAnchor::Left => this.left_0(),
                PopupAnchor::Right => this.right_0(),
                // Stretch the outer layer to the chip's width, then flex centers the
                // fixed-width panel on the chip's centerline
                PopupAnchor::Center => this.left_0().right_0().flex().flex_row().justify_center(),
            })
            // Scroll-wheel events do not pass through to the session message stream
            // behind the popup; the content's own scrolling (Command virtual list
            // and other deeper scroll areas) consumes events first and is unaffected
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .on_mouse_down_out(cx.listener(|this, event: &MouseDownEvent, _, cx| {
                if let Some((kind, _)) = this.popup {
                    this.outside_closed = Some((kind, event.position));
                }
                this.popup = None;
                cx.notify();
            }))
            .when_some(hover_clear, |this, command| {
                this.on_hover(cx.listener(move |_, hovered: &bool, window, cx| {
                    if !*hovered {
                        command.update(cx, |state, cx| {
                            state.set_selected_index(None, window, cx);
                        });
                    }
                }))
            })
            .child(
                div()
                    .relative()
                    .map(|this| match anchor {
                        // Context capacity panel content is fixed (title row +
                        // progress bar), fixed width centered
                        PopupAnchor::Center => this.w(px(360.)),
                        // Other panels size to content: short lists like reasoning
                        // levels need not fill 360
                        _ => this.min_w(px(160.)).max_w(px(360.)),
                    })
                    .with_animation(
                        format!("{id}-enter"),
                        Animation::new(std::time::Duration::from_millis(150))
                            .with_easing(ease_out_quint()),
                        |el, delta| el.top(px(6.0 * (1.0 - delta))).opacity(delta),
                    )
                    .child(content),
            )
            .into_any_element()
    }

    /// Command popup shell: anchored right above the trigger chip, closes on outside
    /// click, with an enter animation.
    pub(crate) fn command_popup_shell(
        &self,
        id: &'static str,
        state: &Entity<CommandState>,
        command: Command,
        anchor: PopupAnchor,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.popup_shell(
            id,
            command.into_any_element(),
            anchor,
            Some(state.clone()),
            cx,
        )
    }

    /// Common tail for panel confirm/cancel: close the popup and refocus the composer.
    pub(crate) fn close_command_popup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.popup = None;
        self.input.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    /// Chip click toggles the popup: while the popup is open, clicking the chip
    /// first triggers the popup's on_mouse_down_out which closes it (a re-render
    /// sits in between, so the open/closed state captured at render time is
    /// unreliable); here the immediately following click is swallowed by matching
    /// "the down position of the same press", avoiding collapse followed by an
    /// instant reopen.
    pub(crate) fn toggle_popup(
        &mut self,
        kind: Popup,
        click: &ClickEvent,
        command: Option<Entity<CommandState>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let down_pos = match click {
            ClickEvent::Mouse(event) => Some(event.down.position),
            _ => None,
        };
        if let Some((closed, pos)) = self.outside_closed.take()
            && closed == kind
            && Some(pos) == down_pos
        {
            return;
        }
        if matches!(self.popup, Some((k, _)) if k == kind) {
            self.close_command_popup(window, cx);
        } else {
            self.popup = Some((kind, 0));
            if let Some(command) = command {
                command.update(cx, |state, cx| {
                    state.set_query("", window, cx);
                    state.focus(window, cx);
                });
            }
            cx.notify();
        }
    }
}
