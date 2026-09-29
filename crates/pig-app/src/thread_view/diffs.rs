use super::*;

impl ThreadView {
    /// 编辑工具的展开卡片（ZCode LightweightDiffPreview 同款）：圆角描边代码卡，
    /// 无 padding；行号 gutter（新增绿/删除红/其余最暗）+ 增删行淡底色与左缘色条，
    /// 行号是预览行连续序号（非文件行号），限高内部滚动，超 400 行截断；
    /// 四角用卡片底色补丁收圆（gpui 内容裁剪仅矩形），滚动条内置随补丁收角。
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
                    // 左缘色条（对应 ZCode 的 inset 3px box-shadow）
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
                    .child(format!("… 省略 {omitted} 行 …"))
                    .into_any_element(),
            );
        }

        let card_bg = cx.theme().secondary;
        // 卡片背后 = 页面底色（消息区自身透明，与 Root 的 tokens.background 同值）
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
            // 滚动条收进卡片内部，角上同样被补丁收住
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
            // 补丁盖住了角上的描边，重描一遍圆角边框
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


    /// 每轮改动面板（ZCode turn 头部「文件更改」同款）：一行汇总
    /// 「N 个文件已更改 +A -D」（箭头悬停显示），展开后逐文件行（路径 + +N/-N），
    /// 文件行再展开为内联 diff 卡（复用编辑卡的渲染）。
    pub(crate) fn render_turn_changes(
        &self,
        message_ix: usize,
        segment_ix: usize,
        rows: &[TurnFileRow],
        open: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let subtle = cx.theme().muted_foreground;
        let subtlest = subtle.opacity(0.6);
        let group_id = format!("turn-changes-{message_ix}-{segment_ix}");
        let (adds, dels) = rows.iter().fold((0u32, 0u32), |(a, d), r| {
            (a + r.edit.additions, d + r.edit.deletions)
        });

        let mut file_rows: Vec<AnyElement> = Vec::new();
        for (rix, row) in rows.iter().enumerate() {
            let (dir, name) = split_path(&row.edit.path);
            let row_group = format!("{group_id}-file-{rix}");
            file_rows.push(
                v_flex()
                    .w_full()
                    .child(
                        h_flex()
                            .id(("turn-file", (message_ix * 1024 + segment_ix) * 512 + rix))
                            .group(row_group.clone())
                            .w_full()
                            .gap_2()
                            .py_1()
                            .pl(px(26.))
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(Segment::TurnChanges { rows, .. }) = this
                                    .messages
                                    .get_mut(message_ix)
                                    .and_then(|m| m.segments.get_mut(segment_ix))
                                    && let Some(row) = rows.get_mut(rix)
                                {
                                    row.expanded = !row.expanded;
                                }
                                cx.notify();
                            }))
                            .child(Icon::new(IconName::FileText).size_4().text_color(subtlest))
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_sm()
                                    .text_color(subtle)
                                    .child(name),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .text_sm()
                                    .text_color(subtlest)
                                    .child(dir),
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
                            .child(
                                div()
                                    .invisible()
                                    .group_hover(row_group, |this| this.visible())
                                    .when(row.expanded, |this| this.visible())
                                    .child(
                                        Icon::new(if row.expanded {
                                            IconName::ChevronDown
                                        } else {
                                            IconName::ChevronRight
                                        })
                                        .size_4()
                                        .text_color(subtlest),
                                    ),
                            ),
                    )
                    .when(row.expanded, |this| {
                        this.child(
                            div()
                                .relative()
                                .on_scroll_wheel(consume_scroll(&row.scroll))
                                .child(Self::render_edit_diff(
                                    ("turn-diff", (message_ix * 1024 + segment_ix) * 512 + rix),
                                    &row.edit,
                                    &row.scroll,
                                    cx,
                                )),
                        )
                    })
                    .into_any_element(),
            );
        }

        v_flex()
            .w_full()
            .child(
                h_flex()
                    .id(("turn-changes", message_ix * 1024 + segment_ix))
                    .group(group_id.clone())
                    .w_full()
                    .gap_2()
                    .py_1()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(Segment::TurnChanges { open, .. }) = this
                            .messages
                            .get_mut(message_ix)
                            .and_then(|m| m.segments.get_mut(segment_ix))
                        {
                            *open = !*open;
                        }
                        cx.notify();
                    }))
                    .child(
                        Icon::new(AssetIconName::ListTodo)
                            .size_4()
                            .text_color(subtlest),
                    )
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(subtlest)
                            .child(format!("{} 个文件已更改", rows.len())),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .text_sm()
                            .font_family(cx.theme().mono_font_family.clone())
                            .when(adds > 0, |this| {
                                this.child(
                                    div()
                                        .text_color(cx.theme().success)
                                        .child(format!("+{adds}")),
                                )
                            })
                            .when(dels > 0, |this| {
                                this.child(
                                    div()
                                        .text_color(cx.theme().danger)
                                        .child(format!("-{dels}")),
                                )
                            }),
                    )
                    .child(
                        div()
                            .invisible()
                            .group_hover(group_id, |this| this.visible())
                            .when(open, |this| this.visible())
                            .child(
                                Icon::new(if open {
                                    IconName::ChevronDown
                                } else {
                                    IconName::ChevronRight
                                })
                                .size_4()
                                .text_color(subtlest),
                            ),
                    ),
            )
            .when(open, |this| this.children(file_rows))
            .into_any_element()
    }


}
