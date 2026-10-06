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

    /// 每轮改动面板（ZCode 文件更改卡同款）：圆角描边卡片，头部
    /// 「✎ N 个文件已修改」+ 右端「+A -D」与增删比例条（绿=新增占比、红=删除）；
    /// 下方逐文件行（目录暗淡可省略 + 文件名高亮，右端等宽 +N/-N）。
    /// 折叠态只露前 PREVIEW_ROWS 行，底部「还有 N 个文件 ▾」展开全部（再点收起）；
    /// 文件行点击在右侧「文件」tab 打开（ThreadEvent::OpenFile）。
    pub(crate) fn render_turn_changes(
        &self,
        message_ix: usize,
        segment_ix: usize,
        rows: &[TurnFileRow],
        open: bool,
        expand_anim: &ExpandAnim,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        /// 折叠态直出的文件行数，超出收进底部「还有 N 个文件」
        const PREVIEW_ROWS: usize = 3;
        /// 头部增删比例条宽度（绿段占比 = 新增 / 总改动）
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

        // 头部增删比例条
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
                            .child(format!("{} 个文件已修改", rows.len())),
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
                // 开合动画包装（滑开/滑收 + 淡入淡出）
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
                                    "收起".to_string()
                                } else {
                                    format!("还有 {} 个文件", rows.len() - PREVIEW_ROWS)
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

    /// 改动面板的单文件行：目录暗淡可省略 + 文件名高亮，右端等宽 +N/-N；
    /// 点击在右侧「文件」tab 打开（ThreadEvent::OpenFile，按会话 cwd 解析路径），
    /// 悬停时路径加下划线提示可点
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
            .tooltip(move |window, cx| Tooltip::new("在右侧打开文件").build(window, cx))
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.emit(ThreadEvent::OpenFile {
                    path: path.clone(),
                    line: None,
                });
            }))
            // 路径：目录暗淡可省略，文件名高亮收尾
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
