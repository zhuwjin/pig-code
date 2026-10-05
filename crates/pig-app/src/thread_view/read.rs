//! Read 工具的展示：摘要行附加件（路径可点击 + 行数）与展开的代码卡
//!（头部 = 文件名 + 自动换行/复制按钮；正文 = 行号 gutter + tree-sitter
//! 高亮，行号取 Read 输出的真实文件行号；尾部标注行原样附在代码之后）。
//!
//! 输出格式（pig-core tool/read.rs）：`{行号}\t{内容}` 逐行 + 尾部标注行
//!（[已截断…] / [文件信息…] / [警告…]）。空文件/「文件未变化」/报错等
//! 无行号输出 → is_read_code_output 为 false，回落通用工具卡。

use super::*;

/// 卡片渲染行数上限（diff 卡同款口径；完整内容点路径在右侧文件面板看）
const MAX_CARD_ROWS: usize = 600;
/// 卡片正文限高
const CARD_BODY_MAX_H: f32 = 320.;

/// 首行形如 `{数字}\t…` 即判定为带行号的文件内容输出（零分配，逐帧可用）
pub(crate) fn is_read_code_output(output: &str) -> bool {
    output
        .lines()
        .next()
        .and_then(|line| line.split_once('\t'))
        .is_some_and(|(no, _)| !no.is_empty() && no.bytes().all(|b| b.is_ascii_digit()))
}

/// Read 输出的首行号（点路径打开文件面板时的滚动定位）
pub(crate) fn read_output_first_line(output: &str) -> Option<usize> {
    let (no, _) = output.lines().next()?.split_once('\t')?;
    if no.is_empty() || !no.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    no.parse().ok()
}

/// 摘要行的「N 行」计数：带行号前缀的行数（零分配扫描；非内容输出为 0）
pub(crate) fn read_output_line_count(output: &str) -> usize {
    output
        .lines()
        .filter(|line| {
            line.split_once('\t')
                .is_some_and(|(no, _)| !no.is_empty() && no.bytes().all(|b| b.is_ascii_digit()))
        })
        .count()
}

/// 全文解析：`{行号}\t{内容}` 行入 lines，空行跳过，其余（尾部标注）入 notes。
/// 注意内容行自身可能以「数字+tab」开头（文件内容如此），split_once 只切第一个
/// tab 天然正确；空内容行是 `{no}\t`（body 为空串）
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

/// parse_read_output 的结果
pub(crate) struct ParsedReadOutput {
    /// (文件行号, 行内容)
    pub lines: Vec<(usize, String)>,
    /// 尾部标注行（[已截断…]/[文件信息…]/[警告…]）
    pub notes: Vec<String>,
}

/// 解析输出 + 拼接高亮文本 + tree-sitter 高亮 + 量最大行宽（结果缓存在
/// ReadCardUi，主题切换由调用方按 Arc 判等重建）
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
    /// Read 工具的展开代码卡（ZCode 同款）：圆角描边卡，头部 = 文件名 +
    /// 自动换行/复制按钮；正文 = 行号 gutter（真实文件行号）+ tree-sitter
    /// 高亮行。默认不折行（横向滚动），点换行钮切自动换行。限高内部滚动；
    /// 超 MAX_CARD_ROWS 截断并提示。四角用卡片底色补丁收圆（diff 卡同款）。
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
        // 卡片背后 = 页面底色（消息区自身透明，与 Root 的 tokens.background 同值）
        let behind = cx.theme().background;
        let subtle = cx.theme().muted_foreground;
        let subtlest = subtle.opacity(0.6);
        let (_, name) = split_path(path);

        // 头部：文件名 + 自动换行开关 + 复制（按钮悬停才显出是惯例，这里常显——
        // ZCode 读取卡头部按钮即常显）
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
                    .tooltip("自动换行")
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
                    .tooltip("复制")
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

        // 正文行（行号 = Read 输出里的真实文件行号）
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
        // 尾部标注（[已截断…]/[文件信息…]）与超上限省略提示
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
                    .child(format!(
                        "… 省略 {} 行（点摘要行路径在右侧查看完整文件）…",
                        total - shown
                    ))
                    .into_any_element(),
            );
        }

        let body = div()
            .id(("read-body", key))
            .w_full()
            .max_h(px(CARD_BODY_MAX_H))
            .overflow_y_scroll()
            // 滚轮锁定手势轴：gpui 默认会把纵向滚轮 delta 映射到仅 x 可滚的
            // 容器（y→x），也会把横向 delta 映射到仅 y 可滚的容器（x→y）——
            // 不锁定时滚轮一动两个轴一起滚。锁定后：纵向滚轮只滚纵向，
            // 横向（Shift+滚轮/触控板横滑）只滚横向
            .restrict_scroll_to_axis()
            .track_scroll(body_scroll)
            // 不折行：内容显式量宽 + 横向滚动（不显式给宽会被布局钳进可用空间，
            // 横向滚动失效——见 code_view::measure_max_line_width）；折行：内容收进卡宽
            .child(if ui.wrap {
                v_flex().w_full().children(rows).into_any_element()
            } else {
                // 行 = gutter + 代码格（pl_3 + 文本 + pr_3）；底部预留横向滚动条车道
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
                        // 滚动条收进正文区域（不到头部）；角上由补丁收圆
                        div()
                            .relative()
                            .w_full()
                            .child(body)
                            .child(Scrollbar::vertical(body_scroll))
                            .when(!ui.wrap, |this| {
                                // 横向滚动条常显（Scrolling 模式滚动完就淡出，
                                // 鼠标用户会失去唯一的横滚入口——滚轮纵走不映射
                                // 横向，只能靠拖条/Shift+滚轮/触控板）
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
