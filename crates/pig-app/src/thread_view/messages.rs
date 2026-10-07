use super::*;

/// Spring parameters for nav bar width/opacity: near-critical damping (ζ≈0.93),
/// smooth settling with no trailing and no visible overshoot
const NAV_BAR_SPRING: SpringConfig = SpringConfig::new(260., 30., 1.);

/// Enter/exit animation duration of the nav preview card (the stable-120ms-hover opening rule is unchanged; the animation only handles fade in/out)
const NAV_CARD_ANIM_DUR: std::time::Duration = std::time::Duration::from_millis(160);

/// Compact divider: line - content - line (the "Compacting context" in-progress state and the
/// "Context compacted" done state share this skeleton, modeled on ZCode's context compaction divider row)
pub(crate) fn render_compact_divider(content: AnyElement, cx: &App) -> AnyElement {
    let line = || div().flex_grow(1.).h(px(1.)).bg(cx.theme().border);
    h_flex()
        .w_full()
        .items_center()
        .gap_3()
        .py_2()
        .child(line())
        .child(content)
        .child(line())
        .into_any_element()
}

/// @-mention segmentation: every @path occurring in the text (a files hit) is cut into a Mention, the rest stays Text.
/// Longer paths first (prevents prefixes from eating each other); the character after a hit must be a path terminator (prevents @a.rs2 from matching @a.rs)
#[derive(Debug, PartialEq)]
pub(crate) enum MentionSegment {
    Text(String),
    Mention(String),
}

pub(crate) fn split_mention_segments(text: &str, files: &[String]) -> Vec<MentionSegment> {
    let mut segments = vec![MentionSegment::Text(text.to_string())];
    let mut sorted: Vec<&String> = files.iter().collect();
    sorted.sort_by_key(|f| std::cmp::Reverse(f.len()));
    for file in sorted {
        let needle = format!("@{file}");
        let mut next = Vec::with_capacity(segments.len() + 1);
        for segment in segments {
            let MentionSegment::Text(text) = segment else {
                next.push(segment);
                continue;
            };
            let mut rest = text.as_str();
            loop {
                let Some(pos) = rest.find(&needle) else {
                    next.push(MentionSegment::Text(rest.to_string()));
                    break;
                };
                let boundary = rest.as_bytes().get(pos + needle.len()).is_none_or(|b| {
                    !matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'/' | b'.' | b'-')
                });
                if !boundary {
                    let skip = pos + 1;
                    next.push(MentionSegment::Text(rest[..skip].to_string()));
                    rest = &rest[skip..];
                    continue;
                }
                if pos > 0 {
                    next.push(MentionSegment::Text(rest[..pos].to_string()));
                }
                next.push(MentionSegment::Mention(file.clone()));
                rest = &rest[pos + needle.len()..];
            }
        }
        segments = next;
    }
    // Merge adjacent Text segments (the boundary guard skips failed hits, leaving adjacent text segments behind)
    let mut merged: Vec<MentionSegment> = Vec::with_capacity(segments.len());
    for segment in segments {
        match (merged.last_mut(), &segment) {
            (Some(MentionSegment::Text(prev)), MentionSegment::Text(text)) => prev.push_str(text),
            _ => merged.push(segment),
        }
    }
    merged.retain(|s| !matches!(s, MentionSegment::Text(t) if t.is_empty()));
    merged
}

/// Nav preview card body (shared by the open card and the exit snapshot): title 2 lines + assistant summary 3 lines
fn nav_card_body(data: &NavCardData, cx: &App) -> Div {
    let (_, _, user_preview, assistant_preview, is_text) = data;
    v_flex()
        .w(px(320.))
        .p_3()
        .gap_2()
        .bg(cx.theme().popover)
        .border_1()
        .border_color(cx.theme().border)
        .rounded_lg()
        .shadow_lg()
        .child(
            div()
                .text_sm()
                .font_medium()
                .line_clamp(2)
                .child(user_preview.clone()),
        )
        .child(
            div()
                .text_sm()
                // Text replies at 80% brightness, placeholder labels at the darkest tier
                // (aligned with ZCode's popover-foreground/80 and foreground-subtle tiers)
                .text_color(if *is_text {
                    cx.theme().foreground.opacity(0.8)
                } else {
                    cx.theme().muted_foreground
                })
                .line_clamp(3)
                .child(assistant_preview.clone()),
        )
}

impl ThreadView {
    pub(crate) fn render_user_message(
        &self,
        ix: usize,
        message: &ChatMessage,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Synthetic message of a background subagent finishing/failing: rendered as a notification card instead of a user bubble
        // (tag stripping happens only in the display layer; message.text keeps the original, and live and replay share this path)
        if let Some(note) = as_task_notification(&message.text) {
            return self.render_task_notification(ix, &note, message, cx);
        }
        // Display text and @-mention segmentation (inline chips); files that never appear
        // in the text (programmatic attachments, etc.) fall back to the traditional chip row above the bubble
        // (pre-2026-10 records may carry a legacy "引用文件: ..." suffix in the text; shown as-is)
        let segments = split_mention_segments(&message.text, &message.files);
        let unmatched: Vec<String> = {
            let inline: std::collections::HashSet<&str> = segments
                .iter()
                .filter_map(|s| match s {
                    MentionSegment::Mention(path) => Some(path.as_str()),
                    _ => None,
                })
                .collect();
            message
                .files
                .iter()
                .filter(|f| !inline.contains(f.as_str()))
                .cloned()
                .collect()
        };
        v_flex()
            .w_full()
            .items_end()
            .gap_1()
            .when(!unmatched.is_empty(), |this| {
                this.child(h_flex().gap_1().children(unmatched.iter().map(|file| {
                    h_flex()
                        .gap_1()
                        .px_2()
                        .py_0p5()
                        .rounded(cx.theme().radius)
                        .bg(cx.theme().accent)
                        .child(
                            Icon::new(IconName::FileText)
                                .size_3()
                                .text_color(cx.theme().muted_foreground),
                        )
                        .child(div().text_xs().child(file.clone()))
                })))
            })
            .child(
                v_flex()
                    .max_w(relative(0.8))
                    .px_3()
                    .py_2()
                    .gap_2()
                    .rounded_2xl()
                    .bg(cx.theme().accent)
                    .text_sm()
                    .with_animation(
                        "user-msg-enter",
                        Animation::new(std::time::Duration::from_millis(150))
                            .with_easing(ease_out_quint()),
                        |el, delta| el.top(px(4.0 * (1.0 - delta))).opacity(delta),
                    )
                    // Image attachments: thumbnails in a wrapping row, above the text (first row of the bubble content);
                    // missing/corrupt bytes → degrade to a text chip
                    .when(!message.images.is_empty(), |this| {
                        this.child(h_flex().gap_2().flex_wrap().children(
                            message.images.iter().enumerate().map(|(image_ix, image)| {
                                self.render_user_image(ix, image_ix, image, cx)
                            }),
                        ))
                    })
                    // @-mention inline rendering: text segments keep window-level selection (shared handle + reading order),
                    // chip = file icon + underlined file name; click opens the file in the right-side panel
                    .when(!message.text.is_empty(), |this| {
                        let handle = message
                            .selection
                            .as_ref()
                            .expect("selection handle lazily created during render")
                            .0
                            .clone();
                        if segments
                            .iter()
                            .any(|s| matches!(s, MentionSegment::Mention(_)))
                        {
                            this.child(h_flex().flex_wrap().items_center().children(
                                segments.iter().enumerate().map(|(six, segment)| {
                                    match segment {
                                        MentionSegment::Text(text) => SelectableText::with_handle(
                                            ("user-msg-text", ix * 1024 + six),
                                            handle.clone(),
                                            text.clone(),
                                        )
                                        .document_order(six as u64)
                                        .into_any_element(),
                                        MentionSegment::Mention(path) => {
                                            let file_name =
                                                path.rsplit('/').next().unwrap_or(path).to_string();
                                            let open_path = path.clone();
                                            h_flex()
                                                .id(("user-mention", ix * 1024 + six))
                                                .test_support()
                                                .gap_1()
                                                .cursor_pointer()
                                                .rounded(cx.theme().radius)
                                                .hover(|this| {
                                                    this.bg(cx.theme().accent.opacity(0.6))
                                                })
                                                .on_click(cx.listener(move |_, _, _, cx| {
                                                    cx.emit(ThreadEvent::OpenFile {
                                                        path: open_path.clone(),
                                                        line: None,
                                                    });
                                                }))
                                                .child(
                                                    Icon::new(IconName::FileText)
                                                        .size_3p5()
                                                        .text_color(cx.theme().muted_foreground),
                                                )
                                                .child(div().text_sm().underline().child(file_name))
                                                .into_any_element()
                                        }
                                    }
                                }),
                            ))
                        } else {
                            this.child(
                                SelectableText::with_handle(
                                    ("user-msg-text", ix),
                                    handle,
                                    message.text.clone(),
                                )
                                .document_order(ix as u64),
                            )
                        }
                    }),
            )
            .into_any_element()
    }

    /// The background subagent's synthetic notification block (A3d, same as kimi-code):
    /// a right-aligned "✓ Sent by background (Agent)" mini label + a width-capped bubble on the same background as user bubbles
    /// (title / completed · duration / result file row / raw payload collapsed by default).
    /// Clicking the bubble opens the right-side subagent conversation tab (the copy-path button's and the payload collapse row's hit areas
    /// stop_propagation and do not bubble; no click handler when the agent_id attribute is missing; core output always has it,
    /// and a missing value only occurs in the fault-tolerant path for hand-built text).
    pub(crate) fn render_task_notification(
        &self,
        message_ix: usize,
        note: &TaskNotification,
        message: &ChatMessage,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let failed = note.status.as_deref() == Some("failed");
        let subtle = cx.theme().muted_foreground;
        let subtlest = subtle.opacity(0.6);
        let title = note
            .description
            .clone()
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| rust_i18n::t!("thread.bg_subagent").to_string());
        // Status line: completed/failed (N steps) · duration … (whichever attribute is missing is omitted)
        let status_word = if failed {
            rust_i18n::t!("thread.failed")
        } else {
            rust_i18n::t!("thread.completed")
        };
        let steps = note.turns.as_deref().map(|t| {
            // turns 以字符串形态存（通知属性），"1" 时取单数键
            if t == "1" {
                rust_i18n::t!("thread.steps_one", n = t).to_string()
            } else {
                rust_i18n::t!("thread.steps", n = t).to_string()
            }
        });
        let cost = note.duration_ms.map(|ms| {
            rust_i18n::t!("thread.duration", d = format_notification_duration(ms)).to_string()
        });
        let status_line = match (steps, cost) {
            (Some(steps), Some(cost)) => format!("{status_word} {steps} · {cost}"),
            (Some(steps), None) => format!("{status_word} {steps}"),
            (None, Some(cost)) => format!("{status_word} · {cost}"),
            (None, None) => status_word.to_string(),
        };
        // The UI state was lazily created in the prepare loop before render; guard against None (theoretically unreachable)
        let ui = message.notification_ui.as_ref();
        let payload_open = ui.is_some_and(|u| u.payload_open);
        let copied = ui.is_some_and(|u| u.copied);
        let payload_scroll = ui.map(|u| u.payload_scroll.clone());
        let record_size = ui.and_then(|u| u.record_size);
        let agent_id_click = note
            .agent_id
            .clone()
            .map(|agent_id| (agent_id, title.clone()));
        v_flex()
            .w_full()
            .items_end()
            .gap_1()
            // Top right-aligned mini label: ✓/✗ Sent by background (Agent)
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Icon::new(if failed {
                            IconName::Close
                        } else {
                            IconName::CircleCheck
                        })
                        .size_3()
                        .text_color(if failed {
                            cx.theme().danger
                        } else {
                            cx.theme().success
                        }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(if failed { cx.theme().danger } else { subtle })
                            .child(rust_i18n::t!("thread.sent_by_background")),
                    ),
            )
            // Bubble (A3d: same background/radius/padding as user message bubbles, width follows content capped at 520px;
            // the failed variant keeps the same background; only the status word and label go danger)
            .child(
                v_flex()
                    .id(("task-notification", message_ix))
                    .max_w(px(520.))
                    .gap_2()
                    .px_3()
                    .py_2()
                    .rounded_2xl()
                    .bg(cx.theme().accent)
                    .when_some(agent_id_click, |this, (agent_id, title)| {
                        this.cursor_pointer()
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(ThreadEvent::OpenSubagent {
                                    agent_id: agent_id.clone(),
                                    title: title.clone(),
                                });
                            }))
                    })
                    // Row 1: title (description; falls back to "background subagent")
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(cx.theme().foreground)
                            .child(title),
                    )
                    // Row 2: status (completed/failed · duration)
                    .child(
                        div()
                            .text_xs()
                            .text_color(if failed { cx.theme().danger } else { subtle })
                            .child(status_line),
                    )
                    // Row 3: result file (doc icon + middle-elided path + size + copy-path button);
                    // points at result.md (core output always carries the result attribute; the whole row is omitted without it)
                    .when_some(note.result.clone(), |this, result_path| {
                        let size_text = match record_size {
                            Some(Some(bytes)) => format_file_size(bytes),
                            Some(None) => rust_i18n::t!("thread.record_deleted").to_string(),
                            None => String::new(),
                        };
                        this.child(
                            h_flex()
                                .w_full()
                                .gap_1()
                                .child(
                                    Icon::new(IconName::FileText)
                                        .size_3p5()
                                        .text_color(subtlest),
                                )
                                .child(
                                    div()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .text_xs()
                                        .text_color(subtle)
                                        .child(elide_record_path(&result_path)),
                                )
                                .when(!size_text.is_empty(), |this| {
                                    this.child(
                                        div()
                                            .flex_shrink_0()
                                            .text_xs()
                                            .text_color(subtlest)
                                            .child(size_text),
                                    )
                                })
                                .child(div().flex_1())
                                .child(
                                    Button::new(("task-notification-copy", message_ix))
                                        .xsmall()
                                        .outline()
                                        .label(if copied {
                                            rust_i18n::t!("thread.copied")
                                        } else {
                                            rust_i18n::t!("thread.copy_path")
                                        })
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            // Copy path does not bubble to the card body (does not open the subagent tab)
                                            cx.stop_propagation();
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                result_path.clone(),
                                            ));
                                            if let Some(ui) = this
                                                .messages
                                                .get_mut(message_ix)
                                                .and_then(|m| m.notification_ui.as_mut())
                                            {
                                                ui.copied = true;
                                            }
                                            cx.notify();
                                        })),
                                ),
                        )
                    })
                    // Row 4: "raw payload" collapse row (collapsed by default; its hit area does not bubble to the card body)
                    .child(
                        h_flex()
                            .id(("task-notification-payload-toggle", message_ix))
                            .gap_1()
                            .py_0p5()
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                if let Some(ui) = this
                                    .messages
                                    .get_mut(message_ix)
                                    .and_then(|m| m.notification_ui.as_mut())
                                {
                                    ui.payload_open = !ui.payload_open;
                                }
                                cx.notify();
                            }))
                            .child(
                                Icon::new(if payload_open {
                                    IconName::ChevronDown
                                } else {
                                    IconName::ChevronRight
                                })
                                .size_3()
                                .text_color(subtlest),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(subtle)
                                    .child(rust_i18n::t!("thread.raw_payload")),
                            ),
                    )
                    // Expanded area: raw payload text (full text with tags), monospace + darker background + 240-capped internal scroll
                    .when(payload_open, |this| {
                        let block = div()
                            .id(("task-notification-payload", message_ix))
                            .w_full()
                            .max_h(px(240.))
                            .overflow_y_scroll()
                            .rounded(cx.theme().radius)
                            .bg(cx.theme().muted)
                            .p_2()
                            .text_xs()
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_color(subtle)
                            .child(message.text.clone());
                        match payload_scroll {
                            // A wheel over the payload area does not pass through to the outer message list
                            Some(handle) => this.child(
                                block
                                    .track_scroll(&handle)
                                    .on_scroll_wheel(consume_scroll(&handle)),
                            ),
                            None => this.child(block),
                        }
                    })
                    // The same enter animation as user bubbles (at the chain tail: AnimationElement no longer supports interaction methods)
                    .with_animation(
                        "user-msg-enter",
                        Animation::new(std::time::Duration::from_millis(150))
                            .with_easing(ease_out_quint()),
                        |el, delta| el.top(px(4.0 * (1.0 - delta))).opacity(delta),
                    ),
            )
            .into_any_element()
    }

    /// One image attachment of a user message: a thumbnail (longest edge 72px, aspect-preserving, no upscaling, rounded),
    /// click to open the lightbox for the large image; missing file/decode failure → an "[Image N (expired)]" text chip (not clickable)
    pub(crate) fn render_user_image(
        &self,
        message_ix: usize,
        image_ix: usize,
        image: &UserImage,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match &image.thumb {
            Some(thumb) => div()
                .id(("user-image", message_ix * 256 + image_ix))
                .relative()
                .w(px(72.))
                .h(px(72.))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_lightbox(message_ix, image_ix, window, cx);
                }))
                .child(
                    gpui_kit::img(thumb.clone())
                        .w(px(72.))
                        .h(px(72.))
                        .object_fit(ObjectFit::Cover)
                        .rounded_md(),
                )
                .child(
                    div()
                        .absolute()
                        .bottom_1()
                        .right_1()
                        .px_1()
                        .py_0p5()
                        .rounded_full()
                        .bg(gpui_kit::black().opacity(0.72))
                        .text_xs()
                        .text_color(gpui_kit::white())
                        .child(message_image_number(image_ix).to_string()),
                )
                .into_any_element(),
            None => div()
                .px_2()
                .py_1()
                .rounded(cx.theme().radius)
                .bg(cx.theme().muted)
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(
                    rust_i18n::t!("thread.image_expired", n = message_image_number(image_ix))
                        .to_string(),
                )
                .into_any_element(),
        }
    }

    /// Turn work row (aligned with ZCode AssistantHistoryStatus): after the turn ends, that turn's
    /// thinking blocks/tool cards collapse into this row; click the whole row to expand/collapse. The chevron is always visible (as in the screenshots),
    /// and there is no expand-height animation: work segments and body segments interleave within the message instead of forming one contiguous block
    fn render_work_row(
        &self,
        ix: usize,
        message: &ChatMessage,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let label = match message.work_state {
            Some(WorkState::Completed { duration: Some(d) }) => {
                // Milliseconds → seconds rounded to nearest, at least 1 second (aligned with ZCode workDuration)
                let secs = (d.as_millis() as f64 / 1000.).round() as u64;
                fmt_work_duration(secs.max(1), rust_i18n::t!("thread.worked").as_ref())
            }
            // History turns without TurnStats in replay: no real duration
            Some(WorkState::Completed { duration: None }) => {
                rust_i18n::t!("thread.processed").to_string()
            }
            Some(WorkState::Stopped) => rust_i18n::t!("thread.stopped").to_string(),
            None => return div().into_any_element(),
        };
        let open = message.work_open;
        h_flex()
            .id(("work-row", ix))
            .test_support()
            .w_full()
            .items_center()
            .gap_1()
            .cursor_pointer()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(label)
            .child(
                Icon::new(if open {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .size_3p5()
                .text_color(cx.theme().muted_foreground),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                if let Some(message) = this.messages.get_mut(ix) {
                    message.work_open = !message.work_open;
                }
                cx.notify();
            }))
            .into_any_element()
    }

    pub(crate) fn render_message(
        &self,
        ix: usize,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let message = &self.messages[ix];
        match message.role {
            Role::User => self.render_user_message(ix, message, cx),
            Role::System => match message.system_kind {
                SystemNoteKind::Plain => div()
                    .w_full()
                    .text_center()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(message.text.clone())
                    .into_any_element(),
                SystemNoteKind::Compacted => div()
                    .id(("compact-note", ix))
                    .test_support()
                    .w_full()
                    .child(render_compact_divider(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .child(
                                Icon::new(AssetIconName::Archive)
                                    .size_3p5()
                                    .text_color(cx.theme().muted_foreground),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(rust_i18n::t!("thread.context_compacted")),
                            )
                            .into_any_element(),
                        cx,
                    ))
                    .into_any_element(),
            },
            Role::Assistant => {
                let has_work = message
                    .segments
                    .iter()
                    .any(|s| matches!(s, Segment::Thinking { .. } | Segment::ToolCall { .. }));
                // Turn ended and not manually expanded: thinking blocks/tool cards fold into the work row, leaving only the body
                let collapse_work = message.work_state.is_some() && !message.work_open;
                // Each row carries a stable key (segment index/fixed name), so the work row appearing or the collapse toggling
                // does not shift and replay the other rows' seg-enter animation keys
                let mut segments: Vec<(String, AnyElement)> =
                    Vec::with_capacity(message.segments.len() + 2);
                if message.work_state.is_some() && has_work {
                    segments.push(("work".to_string(), self.render_work_row(ix, message, cx)));
                }
                for (six, segment) in message.segments.iter().enumerate() {
                    // Approvals take no separate row: the awaiting-approval state shows on the corresponding tool call row
                    // (ApprovalRequested is emitted right after that tool's ToolCallBegin)
                    if matches!(segment, Segment::Approval { .. }) {
                        continue;
                    }
                    if collapse_work
                        && matches!(segment, Segment::Thinking { .. } | Segment::ToolCall { .. })
                    {
                        continue;
                    }
                    let key = format!("seg-{six}");
                    segments.push((
                        key,
                        match segment {
                            Segment::Thinking {
                                text,
                                open,
                                duration,
                                ticker,
                                body_scroll,
                                ticker_scroll,
                                expand_anim,
                                ..
                            } => self.render_thinking(
                                ix,
                                six,
                                text,
                                *open,
                                *duration,
                                ticker,
                                body_scroll,
                                ticker_scroll,
                                expand_anim,
                                window,
                                cx,
                            ),
                            Segment::Markdown { state, .. } => {
                                // Tables align with ZCode (w-max min-w-full, PR #2): column widths are
                                // distributed by measured content and hug it (wrap tables split columns by character-count ratio, so short-text
                                // columns like "first four slots" get squeezed into wrapping); when the frame is too narrow, columns shrink and
                                // wrap first, then the whole table scrolls horizontally after hitting the column floor (upstream has no scrollbar, so an extremely
                                // narrow window can scroll horizontally with no visual cue).
                                // Do not touch table_cell's padding: column width measurement includes CELL_PAD_PX(16);
                                // increasing it makes every column's content box narrower than measured and short columns wrap instead (verified in practice).
                                // End-of-line glyph swallowing (#3293, inline flow under-measuring fullwidth punctuation)
                                // was fixed for good in 0.7.1: re-layout tightens by the shaped draw width.
                                let mut table = StyleRefinement::default();
                                table.overflow.x = Some(Overflow::Scroll);
                                TextView::new(state)
                                    .selectable(true)
                                    .stream_fade(self.streaming)
                                    .text_sm()
                                    .style(TextViewStyle::default().table(table))
                                    // Reveal fallback for search jumps: the outer message list is
                                    // a v_flex().overflow_y_scroll() div scroll container,
                                    // not a gpui::list, so reveal_range will not auto-scroll it
                                    // (upstream reports Hidden when the row is invisible; see the TextView::on_reveal
                                    // docs). Here the target row is manually scrolled into the
                                    // visible area by its line bounds (window coordinates); when the row is already visible, upstream reports Shown and this is not called
                                    .on_reveal({
                                        let scroll_handle = self.scroll_handle.clone();
                                        move |line, _window, _cx| {
                                            let view = scroll_handle.bounds();
                                            let mut offset = scroll_handle.offset();
                                            if line.top() < view.top() {
                                                offset.y += view.top() - line.top();
                                            } else if line.bottom() > view.bottom() {
                                                offset.y -= line.bottom() - view.bottom();
                                            } else {
                                                return;
                                            }
                                            scroll_handle.set_offset(offset);
                                        }
                                    })
                                    .into_any_element()
                            }
                            Segment::ToolCall {
                                tool,
                                summary,
                                output,
                                is_error,
                                done,
                                expanded,
                                edit,
                                live_note,
                                agent_cards,
                                read_ui,
                                bash_ui,
                                expand_anim,
                                body_scroll,
                            } => {
                                let approval_pending = matches!(
                                    message.segments.get(six + 1),
                                    Some(Segment::Approval { decision: None, .. })
                                );
                                self.render_tool_card(
                                    ix,
                                    six,
                                    tool,
                                    summary,
                                    live_note.as_deref(),
                                    output,
                                    *is_error,
                                    *done,
                                    *expanded,
                                    approval_pending,
                                    edit.as_ref(),
                                    agent_cards,
                                    read_ui.as_ref(),
                                    bash_ui.as_ref(),
                                    expand_anim,
                                    body_scroll,
                                    window,
                                    cx,
                                )
                            }
                            Segment::Approval { .. } => unreachable!(),
                            Segment::Plan {
                                state,
                                done,
                                approved,
                                is_error: _,
                                open,
                                expand_anim,
                                body_scroll,
                            } => self.render_plan_row(
                                ix,
                                six,
                                state,
                                *done,
                                *approved,
                                *open,
                                expand_anim,
                                body_scroll,
                                cx,
                            ),
                            Segment::TurnChanges {
                                rows,
                                open,
                                expand_anim,
                            } => self.render_turn_changes(ix, six, rows, *open, expand_anim, cx),
                        },
                    ));
                }
                if let Some(footer) = &message.footer {
                    segments.push((
                        "footer".to_string(),
                        h_flex()
                            .gap_2()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(footer.clone())
                            .into_any_element(),
                    ));
                }
                // Action row (same as ZCode ConversationAssistantTextActions): appears on hover,
                // copy the whole turn's Markdown source + fork the session; the row never appears on a turn still in flight
                let turn_in_flight = self.streaming && ix == self.messages.len() - 1;
                let has_markdown = message
                    .segments
                    .iter()
                    .any(|s| matches!(s, Segment::Markdown { .. }));
                if !turn_in_flight {
                    let group_id = format!("assistant-msg-{ix}");
                    segments.push((
                        "actions".to_string(),
                        h_flex()
                            .id(("msg-actions", ix))
                            .test_support()
                            .gap_1()
                            // Transparent rather than invisible: the hit area survives (same as ZCode opacity-0),
                            // surfacing when the message is hovered
                            .opacity(0.)
                            .group_hover(group_id, |this| this.opacity(1.))
                            .when(has_markdown, |this| {
                                this.child(
                                    Button::new(("msg-copy", ix))
                                        .ghost()
                                        .xsmall()
                                        .icon(if message.copied {
                                            IconName::Check
                                        } else {
                                            IconName::Copy
                                        })
                                        .when(message.copied, |this| {
                                            this.text_color(cx.theme().success)
                                        })
                                        .tooltip(rust_i18n::t!("common.copy"))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            if let Some(message) = this.messages.get_mut(ix) {
                                                let text = message
                                                    .segments
                                                    .iter()
                                                    .filter_map(|s| match s {
                                                        Segment::Markdown { text, .. } => {
                                                            Some(text.as_str())
                                                        }
                                                        _ => None,
                                                    })
                                                    .collect::<Vec<_>>()
                                                    .join("\n\n");
                                                cx.write_to_clipboard(ClipboardItem::new_string(
                                                    text,
                                                ));
                                                message.copied = true;
                                                message.copied_gen += 1;
                                                let generation = message.copied_gen;
                                                // Revert the check after 1.2s (same as ZCode's 1200ms);
                                                // rapid clicks invalidate the old timer by generation
                                                cx.spawn(
                                                    async move |this: WeakEntity<ThreadView>, cx| {
                                                        cx.background_executor()
                                                            .timer(std::time::Duration::from_millis(1200))
                                                            .await;
                                                        let _ = this.update(cx, |this, cx| {
                                                            if let Some(message) =
                                                                this.messages.get_mut(ix)
                                                                && message.copied_gen == generation
                                                            {
                                                                message.copied = false;
                                                                cx.notify();
                                                            }
                                                        });
                                                    },
                                                )
                                                .detach();
                                            }
                                            cx.notify();
                                        })),
                                )
                            })
                            .child(
                                Button::new(("msg-fork", ix))
                                    .ghost()
                                    .xsmall()
                                    .icon(AssetIconName::GitFork)
                                    .tooltip(rust_i18n::t!("thread.fork"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        // User roles map 1:1 to core's User records
                                        // (including background subagent task-notification synthetic messages)
                                        let turns = this.messages[..=ix]
                                            .iter()
                                            .filter(|m| m.role == Role::User)
                                            .count();
                                        cx.emit(ThreadEvent::Fork { turns });
                                        cx.notify();
                                    })),
                            )
                            .into_any_element(),
                    ));
                }
                v_flex()
                    .group(format!("assistant-msg-{ix}"))
                    .w_full()
                    .gap_3()
                    .children(segments.into_iter().map(|(key, segment)| {
                        div()
                            .with_animation(
                                format!("seg-enter-{ix}-{key}"),
                                Animation::new(std::time::Duration::from_millis(150))
                                    .with_easing(ease_out_quint()),
                                |el, delta| el.opacity(delta),
                            )
                            .child(segment)
                    }))
                    .into_any_element()
            }
        }
    }

    /// Turn nav (same as ZCode ConversationTurnNavigator): small vertical bars
    /// on the left edge of the message stream, one per user message. On hover, the target and neighboring bars widen mountain-style; after a stable
    /// 120ms hover, a turn preview card pops out to the bar's right (first 2 lines of the user message + first 3 lines of the assistant reply,
    /// closing 80ms after leaving); clicking jumps to the corresponding message.
    /// Without a hover, the turn owning the viewport top is highlighted; the last bar keeps the lowest brightness while streaming.
    pub(crate) fn render_turn_nav(
        &mut self,
        user_ixs: &[usize],
        active: Option<usize>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let foreground = cx.theme().foreground;
        let subtlest = cx.theme().muted_foreground.opacity(0.6);
        // Only the last turn can be streaming (aligned with ZCode: within one running turn, only the
        // last query shows the running emphasis)
        let running_ix = if self.streaming {
            user_ixs.last().copied()
        } else {
            None
        };
        let focus_pos = self
            .nav_hover
            .and_then(|hover| user_ixs.iter().position(|&ix| ix == hover));
        // Rail height cap: aligned with ZCode's max-h calc(100% - 6rem)
        let rail_max_h = self.scroll_handle.bounds().size.height - px(96.);
        let rail_max_h = if rail_max_h < px(0.) {
            px(0.)
        } else {
            rail_max_h
        };
        // Drop the bar bounds of messages that no longer exist
        self.nav_bar_bounds
            .borrow_mut()
            .retain(|&ix, _| ix < self.messages.len());
        // Preview card content is computed only for the currently open bar (no need to generate preview text for every bar each frame).
        // freshly_opened = no card last frame (newly opened from closed): only then does the enter fade play;
        // switching between bars does not replay it
        let freshly_opened = self.nav_card.is_some() && self.nav_card_last.is_none();
        let card: Option<NavCardData> = self.nav_card.and_then(|ix| {
            let bounds = self
                .nav_bar_bounds
                .borrow()
                .get(&ix)
                .map(|cell| cell.get())?;
            if bounds.size.width <= px(0.) {
                return None; // no bounds yet before the first prepaint
            }
            let user_preview = nav_preview_text(
                &[self.messages[ix].text.as_str()],
                rust_i18n::t!("thread.no_text").as_ref(),
            );
            let (assistant_preview, assistant_is_text) = self.nav_assistant_preview(ix, user_ixs);
            Some((
                ix,
                bounds,
                user_preview,
                assistant_preview,
                assistant_is_text,
            ))
        });
        self.nav_card_last = card.clone();
        let exit_card = self.nav_card_exit.clone();

        div()
            .absolute()
            .top_0()
            .bottom_0()
            .left(px(8.))
            .flex()
            .flex_col()
            .justify_center()
            .child(
                v_flex()
                    .id("turn-nav-rail")
                    .w(px(32.))
                    .max_h(rail_max_h)
                    .overflow_y_scroll()
                    .track_scroll(&self.nav_rail_scroll)
                    // A wheel over the nav scrolls only the rail, not the message list
                    .on_scroll_wheel(consume_scroll(&self.nav_rail_scroll))
                    .children(user_ixs.iter().enumerate().map(|(pos, &ix)| {
                        // Mountain-style widening: hovered item 2.6x, neighbors 1.7x / 1.25x (aligned with ZCode's tiers);
                        // at most 31.2px, never exceeding the rail's 32px width
                        let (scale, mut opacity, focus_color): (f32, f32, bool) =
                            match focus_pos.map(|focus| pos.abs_diff(focus)) {
                                Some(0) => (2.6_f32, 1.0, true),
                                Some(1) => (1.7, 0.86, false),
                                Some(2) => (1.25, 0.72, false),
                                _ => (1.0, 0.58, false),
                            };
                        // Scroll-position-driven active-item emphasis when nothing is hovered
                        let show_active = focus_pos.is_none() && active == Some(ix);
                        if show_active {
                            opacity = 0.9;
                        }
                        if running_ix == Some(ix) {
                            opacity = opacity.max(0.72);
                        }
                        let color = if focus_color || show_active {
                            foreground
                        } else {
                            subtlest
                        };
                        let bounds_cell = self
                            .nav_bar_bounds
                            .borrow_mut()
                            .entry(ix)
                            .or_insert_with(|| Rc::new(Cell::new(Bounds::default())))
                            .clone();
                        div()
                            .id(("turn-nav-bar", ix))
                            .w_full()
                            .h(px(10.))
                            .flex()
                            .items_center()
                            .cursor_pointer()
                            .on_prepaint(move |bounds, _, _| bounds_cell.set(bounds))
                            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                                if *hovered {
                                    this.nav_hover = Some(ix);
                                    // The card opens only after a stable 120ms hover (aligned with ZCode openDelay);
                                    // a quick swipe does not flash a card
                                    cx.spawn(async move |this, cx| {
                                        cx.background_executor()
                                            .timer(std::time::Duration::from_millis(120))
                                            .await;
                                        this.update(cx, |this, cx| {
                                            if this.nav_hover == Some(ix) {
                                                this.nav_card = Some(ix);
                                                cx.notify();
                                            }
                                        })
                                        .ok();
                                    })
                                    .detach();
                                } else {
                                    // Clear only this bar's hover: gpui resolves hover
                                    // element by element in paint order; swiping upward (bar2→bar1) fires the new
                                    // bar's enter first and the old bar's leave later, and an unconditional
                                    // clear would wipe the just-set enter
                                    if this.nav_hover == Some(ix) {
                                        this.nav_hover = None;
                                    }
                                    // Closes only 80ms after leaving (aligned with ZCode closeDelay);
                                    // the close condition = the open bar is no longer hovered; leaving the nav
                                    // and "moving to another bar" both close (ZCode gives each bar its
                                    // own HoverCard: moving closes and reopens, so a fast sweep pops nothing)
                                    cx.spawn(async move |this, cx| {
                                        cx.background_executor()
                                            .timer(std::time::Duration::from_millis(80))
                                            .await;
                                        this.update(cx, |this, cx| {
                                            if this
                                                .nav_card
                                                .is_some_and(|open| this.nav_hover != Some(open))
                                            {
                                                this.close_nav_card(cx);
                                            }
                                        })
                                        .ok();
                                    })
                                    .detach();
                                }
                                cx.notify();
                            }))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                // Pause following after jumping away; if the target is near the bottom, the
                                // at_bottom check in render resumes following
                                this.follow_bottom = false;
                                this.nav_jump = true;
                                this.scroll_handle.scroll_to_top_of_item(ix);
                                cx.notify();
                            }))
                            .child(
                                // Opacity spring (inner: active/hover/running emphasis) + width
                                // spring (outer: mountain widening). The element id keeps the spring state,
                                // handing over smoothly when the target changes; color is two discrete tiers and still switches instantly
                                div()
                                    .h(px(2.))
                                    .rounded_full()
                                    .bg(color)
                                    .with_spring(
                                        ("turn-nav-bar-o", ix),
                                        SpringAnimation::new(NAV_BAR_SPRING).to(opacity),
                                        |el, o| el.opacity(o),
                                    )
                                    .with_spring(
                                        ("turn-nav-bar-w", ix),
                                        SpringAnimation::new(NAV_BAR_SPRING).to(scale),
                                        |el, s| el.map_element(|d| d.w(px(12. * s))),
                                    ),
                            )
                            .into_any_element()
                    })),
            )
            // Preview card: deferred to the window layer for painting (escapes the rail's scroll clipping), anchored to the bar's right
            // (ZCode uses a HoverCard with side=right align=start sideOffset=8; gpui-kit's
            // HoverCard only anchors to corners and cannot pop to the trigger's right, so it is hand-drawn with a Positioner).
            // The enter fade plays only when "newly opened from closed" (freshly_opened); switching between bars does not replay it
            .when_some(card, |this, data @ (ix, bounds, _, _, _)| {
                let body: AnyElement = if freshly_opened {
                    nav_card_body(&data, cx)
                        .with_animation(
                            ("nav-card-enter", ix),
                            Animation::new(NAV_CARD_ANIM_DUR).with_easing(ease_out_quint()),
                            |el, d| el.opacity(d),
                        )
                        .into_any_element()
                } else {
                    nav_card_body(&data, cx).into_any_element()
                };
                this.child(
                    deferred(
                        Positioner::side(bounds)
                            .placement(Placement::Right)
                            .align(Align::Start)
                            .offset(px(8.))
                            .margin(px(8.))
                            .occlude()
                            .child(body),
                    )
                    .with_priority(1),
                )
            })
            // Exit card: the snapshot at the moment of closing plays a 160ms fade-out; the cleanup timer is invalidated by generation
            .when_some(exit_card, |this, data @ (_, bounds, _, _, _)| {
                let generation = self.nav_card_exit_gen;
                this.child(
                    deferred(
                        Positioner::side(bounds)
                            .placement(Placement::Right)
                            .align(Align::Start)
                            .offset(px(8.))
                            .margin(px(8.))
                            .occlude()
                            .child(nav_card_body(&data, cx).with_animation(
                                ("nav-card-exit", generation),
                                Animation::new(NAV_CARD_ANIM_DUR),
                                |el, d| el.opacity(1.0 - d),
                            )),
                    )
                    .with_priority(1),
                )
            })
            .into_any_element()
    }

    /// Close the nav preview card: the render snapshot moves into the exit slot to play a 160ms fade-out (the generation goes into the animation id),
    /// and the cleanup timer is invalidated by generation. Leaving the nav and "moving to another bar" share this path
    pub(crate) fn close_nav_card(&mut self, cx: &mut Context<Self>) {
        if let Some(data) = self.nav_card_last.take() {
            self.nav_card_exit_gen += 1;
            let generation = self.nav_card_exit_gen;
            self.nav_card_exit = Some(data);
            cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(NAV_CARD_ANIM_DUR + std::time::Duration::from_millis(40))
                    .await;
                this.update(cx, |this, cx| {
                    if this.nav_card_exit_gen == generation {
                        this.nav_card_exit = None;
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        }
        self.nav_card = None;
        cx.notify();
    }

    /// Assistant summary of the nav preview card: the Markdown texts of the first assistant message after this user message, joined
    /// (aligned with ZCode: assistantTextRows merged, at most 2 paragraphs of 220 characters).
    /// With no text, a placeholder label is given based on the streaming state; the returned bool tells whether it is real reply text.
    pub(crate) fn nav_assistant_preview(&self, ix: usize, user_ixs: &[usize]) -> (String, bool) {
        let running = self.streaming && user_ixs.last() == Some(&ix);
        let texts: Vec<&str> = self.messages[ix + 1..]
            .iter()
            .find(|message| message.role == Role::Assistant)
            .map(|message| {
                message
                    .segments
                    .iter()
                    .filter_map(|segment| match segment {
                        Segment::Markdown { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        if texts.is_empty() {
            return (
                if running {
                    rust_i18n::t!("thread.generating")
                } else {
                    rust_i18n::t!("thread.no_text_reply")
                }
                .to_string(),
                false,
            );
        }
        (
            nav_preview_text(&texts, rust_i18n::t!("thread.no_text_reply").as_ref()),
            true,
        )
    }
}

/// Splits into (directory part including the trailing separator, file name); the directory is empty when there is no separator
pub(crate) fn split_path(path: &str) -> (String, String) {
    match path.rfind(['/', '\\']) {
        Some(ix) => (path[..=ix].to_string(), path[ix + 1..].to_string()),
        None => (String::new(), path.to_string()),
    }
}

/// Token count auto units: <1k as-is; k/M shows an integer when evenly divisible, otherwise one decimal
pub(crate) fn fmt_tokens(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        let k = n as f64 / 1_000.0;
        if k.fract().abs() < 0.05 {
            format!("{}k", k.round() as u64)
        } else {
            format!("{k:.1}k")
        }
    } else {
        let m = n as f64 / 1_000_000.0;
        if m.fract().abs() < 0.05 {
            format!("{}M", m.round() as u64)
        } else {
            format!("{m:.1}M")
        }
    }
}

/// Token stats section of the turn footer: uncached input · cache hits (hit rate) · output ·
/// time to first token · decode speed (excluding TTFT; degenerate data with api_ms 0 falls back to wall clock)
pub(crate) fn format_turn_stats(stats: &pig_protocol::TurnUsageStats) -> String {
    let total_input = stats.input + stats.cache_read;
    let hit_rate = if total_input > 0 {
        format!(
            " ({:.1}%)",
            stats.cache_read as f64 / total_input as f64 * 100.0
        )
    } else {
        String::new()
    };
    // Speed is computed on pure decode time (total API time - the TTFT wait; tool execution/approval waits excluded);
    // when api_ms is 0 (degenerate data like mock sub-millisecond turns), fall back to wall-clock time
    let api_ms = if stats.api_ms > 0 {
        stats.api_ms
    } else {
        stats.duration_ms
    };
    // Average TTFT = total TTFT wait / request count (a multi-step turn averages its many TTFTs;
    // when api_steps is 0, count as one step to avoid dividing by zero)
    let steps = stats.api_steps.max(1);
    let ttft = if stats.ttft_ms > 0 {
        rust_i18n::t!(
            "thread.ttft",
            n = stats.ttft_ms as f64 / steps as f64 / 1000.0 : {:.1}
        )
        .to_string()
    } else {
        String::new()
    };
    let decode_ms = api_ms.saturating_sub(stats.ttft_ms);
    let speed = if decode_ms > 0 {
        format!(
            " · {:.1} tok/s",
            stats.output as f64 / (decode_ms as f64 / 1000.0)
        )
    } else {
        String::new()
    };
    rust_i18n::t!(
        "thread.turn_stats",
        input = fmt_tokens(stats.input),
        hit = fmt_tokens(stats.cache_read),
        hit_rate = hit_rate,
        output = fmt_tokens(stats.output),
        ttft = ttft,
        speed = speed,
    )
    .to_string()
}

/// Nav preview card text: split on blank lines, fold runs of whitespace within a paragraph into spaces, take the first 2 paragraphs joined by newlines,
/// truncate past 220 characters and append "..." (aligned with ZCode conversationTurnNavigatorHelpers
/// buildPreviewText: maxPreviewChars 220 / maxPreviewParagraphs 2)
pub(crate) fn nav_preview_text(parts: &[&str], fallback: &str) -> String {
    let joined = parts.join("\n\n");
    let mut paragraphs: Vec<String> = Vec::new();
    let mut current = String::new();
    for line in joined.trim().lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        if words.is_empty() {
            if !current.is_empty() {
                paragraphs.push(std::mem::take(&mut current));
                if paragraphs.len() == 2 {
                    break;
                }
            }
        } else {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(&words.join(" "));
        }
    }
    if paragraphs.len() < 2 && !current.is_empty() {
        paragraphs.push(current);
    }
    if paragraphs.is_empty() {
        return fallback.to_string();
    }
    let text = paragraphs.join("\n");
    const MAX_CHARS: usize = 220;
    if text.chars().count() <= MAX_CHARS {
        text
    } else {
        let truncated: String = text.chars().take(MAX_CHARS - 3).collect();
        format!("{}...", truncated.trim_end())
    }
}
