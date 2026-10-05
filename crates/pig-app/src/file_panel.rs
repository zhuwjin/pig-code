//! 右侧面板「文件」tab：只读文件查看器（Read 工具卡的路径点击打开）。
//! 行号 gutter + tree-sitter 语法高亮（code_view 共享件）；头部为
//! 路径 + 自动换行开关 + 复制按钮。默认不折行（uniform_list 虚拟化 + 横向
//! 滚动，整文件可开）；换行模式行高不定，退回普通列表（超 MAX_WRAP_LINES
//! 截断并提示）。打开时若 Read 卡带了行号，加载完成后滚到该行。

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::base::{Scrollbar, ScrollbarMode};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::code_view::{
    CODE_LINE_H, CODE_SCROLLBAR_LANE, HighlightedCode, code_line_row, gutter_width, highlight_code,
    measure_max_line_width,
};
use crate::review_panel::shorten_path;

/// 文件读取上限（点击的路径可能是任意大文件；超出拒绝并提示）
const MAX_VIEW_BYTES: u64 = 8 * 1024 * 1024;
/// 换行模式渲染行数上限（普通列表非虚拟化；不折行模式经 uniform_list 全量渲染）
const MAX_WRAP_LINES: usize = 2000;

enum LoadState {
    Loading,
    Ready(Rc<FileContent>),
    Err(String),
}

struct FileContent {
    /// 全文（LF 归一）；行区间见 highlighted.lines
    code: String,
    /// 高亮结果（含行区间；主题切换后在 render 里按 Arc 判等重算）
    highlighted: HighlightedCode,
    /// 语言名（高亮重算用）
    lang: &'static str,
    /// 最大行宽（首帧 render 惰性量宽——text_system 只在 UI 线程可用；
    /// 不显式给宽时横向滚动失效，见 code_view::measure_max_line_width）
    max_width: std::cell::Cell<Option<Pixels>>,
}

pub struct FileViewPanel {
    /// 展示路径（Read 工具摘要原文，多为工作区相对路径；头部截断显示）
    display: String,
    /// 实际读取的绝对路径
    full: PathBuf,
    state: LoadState,
    /// 自动换行开关（默认关：横向滚动）
    wrap: bool,
    /// 复制按钮反馈（换勾，应用惯例不回弹）
    copied: bool,
    /// 不折行模式：uniform_list 虚拟滚动
    list_scroll: UniformListScrollHandle,
    /// 折行模式：普通纵向滚动
    wrap_scroll: ScrollHandle,
}

/// 后台读文件 + 解码（pig-core 同款 UTF-16/GBK 转码）；返回错误文案或全文
fn read_file(full: &Path) -> Result<String, String> {
    let meta = std::fs::metadata(full).map_err(|e| format!("读取失败 {}: {e}", full.display()))?;
    if meta.len() > MAX_VIEW_BYTES {
        return Err(format!(
            "文件过大（{} MB），仅支持查看 {} MB 以内的文件",
            meta.len() / 1024 / 1024,
            MAX_VIEW_BYTES / 1024 / 1024
        ));
    }
    let bytes = std::fs::read(full).map_err(|e| format!("读取失败 {}: {e}", full.display()))?;
    Ok(pig_core::text::decode(&bytes)?.text)
}

impl FileViewPanel {
    pub fn new(display: String, full: PathBuf) -> Self {
        Self {
            display,
            full,
            state: LoadState::Loading,
            wrap: false,
            copied: false,
            list_scroll: UniformListScrollHandle::new(),
            wrap_scroll: ScrollHandle::new(),
        }
    }

    /// 读取并高亮（读盘与高亮都在后台线程；主题在 UI 线程取）。
    /// initial_line：加载完成后滚到该行（Read 卡带过来的首行号）
    pub fn reload(&mut self, initial_line: Option<usize>, cx: &mut Context<Self>) {
        self.state = LoadState::Loading;
        self.copied = false;
        let full = self.full.clone();
        let lang = crate::code_view::lang_name_for_path(&self.display);
        cx.spawn(async move |this, cx| {
            let text = cx
                .background_executor()
                .spawn(async move { read_file(&full) })
                .await;
            let Ok(text) = text else {
                let _ = this.update(cx, |this, cx| {
                    this.state = LoadState::Err(text.err().unwrap_or_default());
                    cx.notify();
                });
                return;
            };
            // 主题对象需在 UI 线程取（高亮颜色在解析时定死）
            let theme = this
                .update(cx, |_, cx| cx.theme().highlight_theme.clone())
                .ok();
            let Some(theme) = theme else { return };
            let code = text.clone();
            let highlighted = cx
                .background_executor()
                .spawn(async move { highlight_code(&code, lang, &theme) })
                .await;
            let _ = this.update(cx, |this, cx| {
                let total = highlighted.lines.len();
                this.state = LoadState::Ready(Rc::new(FileContent {
                    code: text,
                    highlighted,
                    lang,
                    max_width: std::cell::Cell::new(None),
                }));
                if let Some(line) = initial_line {
                    // uniform_list 与折行滚动各记一份（实际生效看当前模式）
                    this.list_scroll
                        .scroll_to_item(line.saturating_sub(1).min(total), ScrollStrategy::Top);
                    this.wrap_scroll
                        .scroll_to_item(line.saturating_sub(1).min(total));
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 自测用：已加载的行数；未加载/失败为 None
    pub fn debug_state(&self) -> Option<usize> {
        match &self.state {
            LoadState::Ready(content) => Some(content.highlighted.lines.len()),
            _ => None,
        }
    }

    /// 主题切换后高亮颜色过期：按 Arc 指针判等重算（render 前调用）
    fn refresh_highlight_if_stale(&mut self, cx: &mut Context<Self>) {
        let LoadState::Ready(content) = &self.state else {
            return;
        };
        let theme = cx.theme().highlight_theme.clone();
        if Arc::ptr_eq(&content.highlighted.theme, &theme) {
            return;
        }
        let highlighted = highlight_code(&content.code, content.lang, &theme);
        self.state = LoadState::Ready(Rc::new(FileContent {
            code: content.code.clone(),
            highlighted,
            lang: content.lang,
            max_width: std::cell::Cell::new(None),
        }));
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let subtle = cx.theme().muted_foreground;
        let wrap = self.wrap;
        let copied = self.copied;
        h_flex()
            .w_full()
            .flex_shrink_0()
            .h(px(36.))
            .px_3()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(Icon::new(IconName::FileText).size_3p5().text_color(subtle))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_xs()
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_color(subtle)
                    // 头部截断保留文件名（review 面板同款）
                    .child(shorten_path(&self.display, 48)),
            )
            .child(
                Button::new("file-wrap-toggle")
                    .ghost()
                    .xsmall()
                    .icon(gpui_kit::assets::IconName::TextWrap)
                    .when(wrap, |this| this.text_color(cx.theme().foreground))
                    .tooltip("自动换行")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.wrap = !this.wrap;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("file-copy")
                    .ghost()
                    .xsmall()
                    .icon(if copied {
                        IconName::CircleCheck
                    } else {
                        IconName::Copy
                    })
                    .when(copied, |this| this.text_color(cx.theme().success))
                    .tooltip("复制文件内容")
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let LoadState::Ready(content) = &this.state {
                            cx.write_to_clipboard(ClipboardItem::new_string(content.code.clone()));
                            this.copied = true;
                            cx.notify();
                        }
                    })),
            )
    }

    /// 不折行：uniform_list 虚拟化（整文件），横向不约束宽度（长行横滚）。
    /// 行宽显式给足（行号列 + padding + 最大行宽）：不给宽时列表以首行
    /// 量宽，超长行会无处可滚（见 code_view::measure_max_line_width）
    fn render_lines_nowrap(
        &self,
        content: &Rc<FileContent>,
        gutter_w: Pixels,
        gutter_color: Hsla,
        row_w: Pixels,
    ) -> AnyElement {
        let content = content.clone();
        uniform_list(
            "file-lines",
            content.highlighted.lines.len(),
            move |range, _, _| {
                range
                    .map(|ix| {
                        div()
                            .w(row_w)
                            .child(code_line_row(
                                ix + 1,
                                content.highlighted.line_text(&content.code, ix),
                                content.highlighted.line_styles(ix),
                                gutter_w,
                                gutter_color,
                                false,
                            ))
                            .into_any_element()
                    })
                    .collect()
            },
        )
        .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
        .track_scroll(&self.list_scroll)
        .size_full()
        // 底部预留常显横向滚动条的车道（padding 收缩内容视口，滚动条恰好落在
        // 车道上而不盖末行；内容高度不含 padding，末行仍可完整滚入视口）
        .pb(px(CODE_SCROLLBAR_LANE))
        .into_any_element()
    }

    /// 折行：普通列表（行高不定），超 MAX_WRAP_LINES 截断并提示
    fn render_lines_wrap(
        &self,
        content: &Rc<FileContent>,
        gutter_w: Pixels,
        gutter_color: Hsla,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let total = content.highlighted.lines.len();
        let shown = total.min(MAX_WRAP_LINES);
        let rows = (0..shown)
            .map(|ix| {
                code_line_row(
                    ix + 1,
                    content.highlighted.line_text(&content.code, ix),
                    content.highlighted.line_styles(ix),
                    gutter_w,
                    gutter_color,
                    true,
                )
            })
            .collect::<Vec<_>>();
        v_flex()
            .id("file-lines-wrap")
            .size_full()
            .overflow_y_scroll()
            // 滚轮锁定手势轴（防横向 delta 被映射成纵向滚动，Read 卡同款）
            .restrict_scroll_to_axis()
            .track_scroll(&self.wrap_scroll)
            .child(v_flex().w_full().children(rows))
            .when(total > shown, |this| {
                this.child(
                    div()
                        .w_full()
                        .py_1()
                        .text_center()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!(
                            "… 自动换行模式仅显示前 {shown} 行（共 {total} 行）；关闭换行可查看完整文件 …"
                        )),
                )
            })
            .into_any_element()
    }
}

impl Render for FileViewPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.refresh_highlight_if_stale(cx);
        let subtle = cx.theme().muted_foreground;
        let body: AnyElement = match &self.state {
            LoadState::Loading => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .child(Spinner::new().small().color(subtle))
                .child(div().text_sm().text_color(subtle).child("加载中…"))
                .into_any_element(),
            LoadState::Err(error) => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .p_4()
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().danger)
                        .child(error.clone()),
                )
                .into_any_element(),
            LoadState::Ready(content) => {
                // 最大行宽惰性量宽（text_system 只在 UI 线程可用）；量一次缓存
                let max_width = match content.max_width.get() {
                    Some(w) => w,
                    None => {
                        let w =
                            measure_max_line_width(&content.code, &content.highlighted, window, cx);
                        content.max_width.set(Some(w));
                        w
                    }
                };
                let gutter_w = gutter_width(content.highlighted.lines.len());
                let gutter_color = subtle.opacity(0.6);
                let lines = if self.wrap {
                    self.render_lines_wrap(content, gutter_w, gutter_color, cx)
                } else {
                    self.render_lines_nowrap(
                        content,
                        gutter_w,
                        gutter_color,
                        gutter_w + px(24.) + max_width,
                    )
                };
                div()
                    .relative()
                    .size_full()
                    .text_xs()
                    .line_height(px(CODE_LINE_H))
                    .font_family(cx.theme().mono_font_family.clone())
                    .child(lines)
                    .child(if self.wrap {
                        Scrollbar::vertical(&self.wrap_scroll)
                    } else {
                        Scrollbar::vertical(&self.list_scroll)
                    })
                    // 不折行时长行可横滚（触控板横滑 / Shift+滚轮 / 拖滚动条），
                    // 横向滚动条常显（默认 Scrolling 模式闲时淡出，鼠标用户会
                    // 失去唯一的横滚入口）
                    .when(!self.wrap, |this| {
                        this.child(
                            Scrollbar::horizontal(&self.list_scroll).mode(ScrollbarMode::Always),
                        )
                    })
                    .into_any_element()
            }
        };
        v_flex()
            .size_full()
            .child(self.render_toolbar(cx))
            .child(div().flex_1().min_h_0().child(body))
            .into_any_element()
    }
}
