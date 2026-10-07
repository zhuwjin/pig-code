use super::*;

impl ThreadView {
    /// The edit tool's expanded card (same as ZCode LightweightDiffPreview):
    /// rounded bordered code card, no padding; line-number gutter (added green/
    /// removed red/rest dimmest) + faint row backgrounds and a left color bar
    /// for add/delete rows; line numbers are consecutive preview row numbers
    /// (not file line numbers); height-capped with internal scrolling,
    /// truncated past 400 rows; the four corners are rounded by card-colored
    /// patches (gpui content clipping is rectangular only), scrollbars built in
    /// and rounded with the patches.
    pub(crate) fn render_edit_diff(
        id: impl Into<ElementId>,
        edit: &EditDiff,
        body_scroll: &ScrollHandle,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        const MAX_ROWS: usize = 400;
        enum Kind {
            Hunk,
            Add,
            Del,
            Context,
        }
        let border = cx.theme().border;
        let code_color = cx.theme().foreground;
        let gutter_muted = cx.theme().muted_foreground.opacity(0.6);
        let added = cx.theme().success;
        let removed = cx.theme().danger;
        let transparent = cx.theme().transparent;

        let mut rows: Vec<AnyElement> = Vec::new();
        let mut line_no = 0u32;
        let mut omitted = 0usize;
        for line in edit.unified_diff.lines() {
            if rows.len() >= MAX_ROWS {
                omitted += 1;
                continue;
            }
            let (kind, text) = if line.starts_with("+++") || line.starts_with("---") {
                continue;
            } else if line.starts_with("@@") {
                (Kind::Hunk, line)
            } else if let Some(rest) = line.strip_prefix('+') {
                (Kind::Add, rest)
            } else if let Some(rest) = line.strip_prefix('-') {
                (Kind::Del, rest)
            } else if line.starts_with('\\') {
                // "\ No newline at end of file"
                continue;
            } else {
                (Kind::Context, line.strip_prefix(' ').unwrap_or(line))
            };
            if matches!(kind, Kind::Hunk) {
                rows.push(
                    h_flex()
                        .w_full()
                        .items_stretch()
                        .child(div().w(px(3.)))
                        .child(
                            div()
                                .w(px(45.))
                                .flex_shrink_0()
                                .border_r_1()
                                .border_color(border),
                        )
                        .child(
                            div()
                                .px_3()
                                .whitespace_nowrap()
                                .text_color(gutter_muted)
                                .child(text.to_string()),
                        )
                        .into_any_element(),
                );
                continue;
            }
            line_no += 1;
            let (bar, number_color, bg) = match kind {
                Kind::Add => (added, added, Some(added.opacity(0.14))),
                Kind::Del => (removed, removed, Some(removed.opacity(0.14))),
                Kind::Context => (transparent, gutter_muted, None),
                Kind::Hunk => unreachable!(),
            };
            rows.push(
                h_flex()
                    .w_full()
                    .items_stretch()
                    .when_some(bg, |this, bg| this.bg(bg))
                    // Left color bar (ZCode's inset 3px box-shadow equivalent)
                    .child(div().w(px(3.)).flex_shrink_0().bg(bar))
                    .child(
                        div()
                            .w(px(45.))
                            .pr_2()
                            .text_right()
                            .flex_shrink_0()
                            .border_r_1()
                            .border_color(border)
                            .text_color(number_color)
                            .child(line_no.to_string()),
                    )
                    .child(
                        div()
                            .px_3()
                            .whitespace_nowrap()
                            .text_color(code_color)
                            .child(text.to_string()),
                    )
                    .into_any_element(),
            );
        }
        if omitted > 0 {
            rows.push(
                div()
                    .w_full()
                    .py_1()
                    .text_center()
                    .text_color(gutter_muted)
                    .child(rust_i18n::t!("thread.omitted_lines", n = omitted).to_string())
                    .into_any_element(),
            );
        }

        let card_bg = cx.theme().secondary;
        // Behind the card = page background (the message area itself is
        // transparent, same value as Root's tokens.background)
        let behind = cx.theme().background;
        div()
            .relative()
            .w_full()
            .child(
                div()
                    .id(id)
                    .w_full()
                    .rounded_xl()
                    .border_1()
                    .border_color(border)
                    .bg(card_bg)
                    .max_h(px(240.))
                    .overflow_y_scroll()
                    .track_scroll(body_scroll)
                    .text_xs()
                    .line_height(px(19.))
                    .font_family(cx.theme().mono_font_family.clone())
                    .children(rows),
            )
            // Scrollbar tucked inside the card; corners likewise held by the patches
            .child(Scrollbar::vertical(body_scroll))
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

    /// Per-turn changes panel (same as ZCode's file changes card): rounded
    /// bordered card; header "✎ N files modified" + "+A -D" on the right end
    /// with an add/delete ratio bar (green = added share, red = deleted); one
    /// row per file below (dimmed elidable directory + highlighted file name,
    /// monospace +N/-N on the right end). The collapsed state shows only the
    /// first PREVIEW_ROWS rows; the bottom "N more files ▾" expands all (click
    /// again to collapse); clicking a file row opens it in the right "Files"
    /// tab (ThreadEvent::OpenFile).
    pub(crate) fn render_turn_changes(
        &self,
        message_ix: usize,
        segment_ix: usize,
        rows: &[TurnFileRow],
        open: bool,
        expand_anim: &ExpandAnim,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        /// File rows shown directly in the collapsed state; the rest folds into
        /// the bottom "N more files"
        const PREVIEW_ROWS: usize = 3;
        /// Header add/delete ratio bar width (green share = additions / total changes)
        const STAT_BAR_W: f32 = 48.;

        let subtle = cx.theme().muted_foreground;
        let subtlest = subtle.opacity(0.6);
        let border = cx.theme().border;
        let success = cx.theme().success;
        let danger = cx.theme().danger;
        let group_id = format!("turn-changes-{message_ix}-{segment_ix}");
        let (adds, dels) = rows.iter().fold((0u32, 0u32), |(a, d), r| {
            (a + r.edit.additions, d + r.edit.deletions)
        });

        // Header add/delete ratio bar
        let stat_bar = (adds + dels > 0).then(|| {
            let green_w = (STAT_BAR_W * adds as f32 / (adds + dels) as f32).round();
            h_flex()
                .flex_shrink_0()
                .w(px(STAT_BAR_W))
                .h(px(4.))
                .rounded_full()
                .overflow_hidden()
                .when(adds > 0, |this| {
                    this.child(div().h_full().w(px(green_w)).bg(success))
                })
                .when(dels > 0, |this| {
                    this.child(div().h_full().flex_1().bg(danger))
                })
        });

        let has_extra = rows.len() > PREVIEW_ROWS;
        let show_extra = has_extra && (open || expand_anim.collapsing);
        let mut preview_rows: Vec<AnyElement> = Vec::new();
        let mut extra_rows: Vec<AnyElement> = Vec::new();
        for (rix, row) in rows.iter().enumerate() {
            if rix >= PREVIEW_ROWS && !show_extra {
                break;
            }
            let el = Self::render_turn_file_row(message_ix, segment_ix, rix, row, &group_id, cx);
            if rix < PREVIEW_ROWS {
                preview_rows.push(el);
            } else {
                extra_rows.push(el);
            }
        }

        v_flex()
            .w_full()
            .rounded_xl()
            .border_1()
            .border_color(border)
            .bg(cx.theme().secondary)
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py(px(10.))
                    .child(
                        Icon::new(AssetIconName::SquarePen)
                            .size_4()
                            .text_color(subtlest),
                    )
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(subtle)
                            .child(
                                rust_i18n::t!("thread.files_modified", n = rows.len()).to_string(),
                            ),
                    )
                    .child(div().flex_1())
                    .child(
                        h_flex()
                            .gap_1()
                            .text_sm()
                            .font_family(cx.theme().mono_font_family.clone())
                            .when(adds > 0, |this| {
                                this.child(div().text_color(success).child(format!("+{adds}")))
                            })
                            .when(dels > 0, |this| {
                                this.child(div().text_color(danger).child(format!("-{dels}")))
                            }),
                    )
                    .when_some(stat_bar, |this, bar| this.child(bar)),
            )
            .child(
                v_flex()
                    .w_full()
                    .border_t_1()
                    .border_color(border)
                    .py_1()
                    .children(preview_rows),
            )
            .when(show_extra, |this| {
                // Expand/collapse animation wrapper (slide open/closed + fade
                // in/out)
                this.child(self.expand_anim_wrap(
                    format!(
                        "turn-changes-expand-{message_ix}-{segment_ix}-{}",
                        expand_anim.generation
                    ),
                    expand_anim,
                    v_flex().w_full().children(extra_rows).into_any_element(),
                ))
            })
            .when(has_extra, |this| {
                this.child(
                    h_flex()
                        .id(("turn-changes", message_ix * 1024 + segment_ix))
                        .w_full()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py(px(8.))
                        .border_t_1()
                        .border_color(border)
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let mut open_now = false;
                            if let Some(Segment::TurnChanges { open, .. }) = this
                                .messages
                                .get_mut(message_ix)
                                .and_then(|m| m.segments.get_mut(segment_ix))
                            {
                                *open = !*open;
                                open_now = *open;
                            }
                            this.drive_expand_anim(message_ix, segment_ix, open_now, cx);
                            cx.notify();
                        }))
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(subtle)
                                .child(if open {
                                    rust_i18n::t!("thread.collapse").to_string()
                                } else {
                                    rust_i18n::t!(
                                        "thread.more_files",
                                        n = rows.len() - PREVIEW_ROWS
                                    )
                                    .to_string()
                                }),
                        )
                        .child(
                            Icon::new(if open {
                                AssetIconName::ChevronUp
                            } else {
                                AssetIconName::ChevronDown
                            })
                            .size_4()
                            .text_color(subtlest),
                        ),
                )
            })
            .into_any_element()
    }

    /// A single file row of the changes panel: dimmed elidable directory +
    /// highlighted file name, monospace +N/-N on the right end; clicking opens
    /// it in the right "Files" tab (ThreadEvent::OpenFile, path resolved
    /// against the session cwd); hovering underlines the path to hint
    /// clickability
    fn render_turn_file_row(
        message_ix: usize,
        segment_ix: usize,
        rix: usize,
        row: &TurnFileRow,
        group_id: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let subtlest = cx.theme().muted_foreground.opacity(0.6);
        let path_hover = cx.theme().foreground;
        let (dir, name) = split_path(&row.edit.path);
        let row_group = format!("{group_id}-file-{rix}");
        let path = row.edit.path.clone();
        h_flex()
            .id(("turn-file", (message_ix * 1024 + segment_ix) * 512 + rix))
            .group(row_group.clone())
            .w_full()
            .items_center()
            .gap_2()
            .px_3()
            .py(px(6.))
            .cursor_pointer()
            .tooltip(move |window, cx| {
                Tooltip::new(rust_i18n::t!("thread.open_file_right").to_string()).build(window, cx)
            })
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.emit(ThreadEvent::OpenFile {
                    path: path.clone(),
                    line: None,
                });
            }))
            // Path: dimmed elidable directory, highlighted file name at the end
            .child(
                h_flex()
                    .min_w_0()
                    .flex_1()
                    .text_sm()
                    .group_hover(row_group, |this| this.text_color(path_hover).underline())
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_color(subtlest)
                            .child(dir),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .whitespace_nowrap()
                            .text_color(cx.theme().foreground)
                            .child(name),
                    ),
            )
            .when(row.edit.additions > 0, |this| {
                this.child(
                    div()
                        .text_sm()
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_color(cx.theme().success)
                        .child(format!("+{}", row.edit.additions)),
                )
            })
            .when(row.edit.deletions > 0, |this| {
                this.child(
                    div()
                        .text_sm()
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_color(cx.theme().danger)
                        .child(format!("-{}", row.edit.deletions)),
                )
            })
            .into_any_element()
    }
}
