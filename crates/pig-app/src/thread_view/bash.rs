//! Bash 工具卡（ZCode 同款）：展开后是两张堆叠的代码卡——
//! 命令卡（头部「Bash」+ 换行/复制按钮；命令带 bash 语法高亮）与
//! 输出卡（头部「输出」+ 换行/复制按钮；输出纯文本不高亮，失败时红色）。
//! 默认不折行（横向滚动 + 常显横向滚动条），两卡开关独立。
//! 运行中/等审批仍走通用工具卡（实时输出），完成（含失败）后才切代码卡。

use super::*;

/// 命令卡正文限高
const CMD_MAX_H: f32 = 160.;
/// 输出卡正文限高
const OUT_MAX_H: f32 = 320.;
/// 单卡渲染行数上限（Read 卡同款口径）
const MAX_CARD_ROWS: usize = 600;

/// 命令卡 / 输出卡（listeners 按它路由到对应的状态字段）
#[derive(Clone, Copy)]
enum SubCard {
    Cmd,
    Out,
}

impl ThreadView {
    /// Bash 工具的展开区：命令卡 + 输出卡堆叠
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
        // 内容缓存：主题切换经 Arc 判等重算（输出是 "text" 纯文本，只有量宽开销）
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
            cache.clone().expect("刚填充")
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
                "输出",
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

    /// 单张子卡：头部（标题 + 换行/复制）+ 正文（等宽行，无行号 gutter；
    /// 不折行时显式量宽 + 横向滚动，滚动条内置随圆角补丁收角）
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
        // 卡片背后 = 页面底色（消息区自身透明，与 Root 的 tokens.background 同值）
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
                    // 标题用界面字体（正文才是等宽）
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
                    .tooltip("自动换行")
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
                    .tooltip("复制")
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

        // 正文行（无行号）
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
                    .child("没有输出。".to_string())
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
                    .child(format!("… 省略 {} 行 …", total - shown))
                    .into_any_element(),
            );
        }

        let body = div()
            .id(format!("{}-body-{}", key.0, key.1))
            .w_full()
            .max_h(px(max_h))
            .overflow_y_scroll()
            // 滚轮锁定手势轴（Read 卡同款）：纵向滚轮只滚纵向，横向只滚横向
            .restrict_scroll_to_axis()
            .track_scroll(v_scroll)
            .text_color(text_color)
            .child(if wrap {
                v_flex().w_full().children(rows).into_any_element()
            } else {
                // 无 gutter：内容宽 = 代码格 padding（24）+ 最大行宽；
                // 底部预留横向滚动条车道
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
            // 滚动链：本卡内容能滚时吞掉滚轮（Bash 卡不走 cards.rs 共享的
            // body_scroll 兜底——子卡句柄各自独立），不能滚时穿透给外层消息列表，
            // 与其他工具卡行为一致
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
                        // 滚动条收进正文区域（不到头部）；角上由补丁收圆
                        div()
                            .relative()
                            .w_full()
                            .child(body)
                            .child(Scrollbar::vertical(v_scroll))
                            .when(!wrap, |this| {
                                // 常显：闲时淡出会让鼠标用户失去唯一的横滚入口
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
}
