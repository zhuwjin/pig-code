//! "File" tab of the right panel: read-only file viewer (opened by clicking
//! a path on a Read tool card).
//! Line number gutter + tree-sitter syntax highlighting (shared code_view
//! pieces); the header is path + wrap toggle + copy button. Defaults to
//! no-wrap (uniform_list virtualization + horizontal scrolling, so any file
//! size opens); in wrap mode the line height varies, falling back to a
//! plain list (truncated with a hint beyond MAX_WRAP_LINES). If the Read
//! card carried a line number, scrolls to that line after loading
//! completes.

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

/// File read limit (a clicked path may be an arbitrarily large file; refuse
/// and hint beyond it)
const MAX_VIEW_BYTES: u64 = 8 * 1024 * 1024;
/// Rendered line limit in wrap mode (the plain list is not virtualized;
/// no-wrap mode renders everything via uniform_list)
const MAX_WRAP_LINES: usize = 2000;

enum LoadState {
    Loading,
    Ready(Rc<FileContent>),
    Err(String),
}

struct FileContent {
    /// Full text (LF-normalized); see highlighted.lines for line ranges
    code: String,
    /// Highlight result (with line ranges; recomputed in render on theme
    /// switch by comparing the Arc)
    highlighted: HighlightedCode,
    /// Language name (for highlight recomputation)
    lang: &'static str,
    /// Max line width (measured lazily on the first render frame:
    /// text_system is only available on the UI thread; horizontal scrolling
    /// breaks without an explicit width, see
    /// code_view::measure_max_line_width)
    max_width: std::cell::Cell<Option<Pixels>>,
}

pub struct FileViewPanel {
    /// Display path (verbatim from the Read tool summary, usually a
    /// workspace-relative path; truncated in the header)
    display: String,
    /// Absolute path actually read
    full: PathBuf,
    state: LoadState,
    /// Wrap toggle (off by default: horizontal scrolling)
    wrap: bool,
    /// Copy button feedback (swaps to a checkmark; app convention is no
    /// revert)
    copied: bool,
    /// No-wrap mode: uniform_list virtual scrolling
    list_scroll: UniformListScrollHandle,
    /// Wrap mode: plain vertical scrolling
    wrap_scroll: ScrollHandle,
}

/// Read + decode the file in the background (same UTF-16/GBK transcoding as
/// pig-core); returns an error message or the full text
fn read_file(full: &Path) -> Result<String, String> {
    let meta = std::fs::metadata(full).map_err(|e| {
        rust_i18n::t!(
            "file_panel.read_failed",
            path = full.display().to_string(),
            error = e.to_string()
        )
        .to_string()
    })?;
    if meta.len() > MAX_VIEW_BYTES {
        return Err(rust_i18n::t!(
            "file_panel.too_large",
            size = meta.len() / 1024 / 1024,
            max = MAX_VIEW_BYTES / 1024 / 1024
        )
        .to_string());
    }
    let bytes = std::fs::read(full).map_err(|e| {
        rust_i18n::t!(
            "file_panel.read_failed",
            path = full.display().to_string(),
            error = e.to_string()
        )
        .to_string()
    })?;
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

    /// Read and highlight (disk read and highlighting both on a background
    /// thread; the theme is taken on the UI thread).
    /// initial_line: scroll to this line after loading (first line number
    /// carried over from the Read card)
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
            // The theme object must be taken on the UI thread (highlight
            // colors are fixed at parse time)
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
                    // Record one for uniform_list and one for wrap scrolling
                    // (whichever matches the current mode takes effect)
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

    /// For self-tests: loaded line count; None when not loaded or failed
    pub fn debug_state(&self) -> Option<usize> {
        match &self.state {
            LoadState::Ready(content) => Some(content.highlighted.lines.len()),
            _ => None,
        }
    }

    /// Highlight colors go stale after a theme switch: recompute by Arc
    /// pointer equality (called before render)
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
                    // Header truncation keeps the file name (same as the
                    // review panel)
                    .child(shorten_path(&self.display, 48)),
            )
            .child(
                Button::new("file-wrap-toggle")
                    .ghost()
                    .xsmall()
                    .icon(gpui_kit::assets::IconName::TextWrap)
                    .when(wrap, |this| this.text_color(cx.theme().foreground))
                    .tooltip(rust_i18n::t!("file_panel.wrap"))
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
                    .tooltip(rust_i18n::t!("file_panel.copy_content"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let LoadState::Ready(content) = &this.state {
                            cx.write_to_clipboard(ClipboardItem::new_string(content.code.clone()));
                            this.copied = true;
                            cx.notify();
                        }
                    })),
            )
    }

    /// No-wrap: uniform_list virtualization (whole file), width unconstrained
    /// horizontally (long lines scroll sideways).
    /// The row width is given explicitly and fully (gutter column, padding
    /// and max line width): without an explicit width the list measures by
    /// the first row and overlong lines have nowhere to scroll (see
    /// code_view::measure_max_line_width)
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
        // Reserve a lane at the bottom for the always-visible horizontal
        // scrollbar (padding shrinks the content viewport so the scrollbar
        // lands exactly on the lane instead of covering the last line;
        // content height excludes padding, so the last line can still scroll
        // fully into view)
        .pb(px(CODE_SCROLLBAR_LANE))
        .into_any_element()
    }

    /// Wrap: plain list (variable line height), truncated with a hint beyond
    /// MAX_WRAP_LINES
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
            // Lock the scroll wheel to the gesture axis (prevents horizontal
            // delta from being mapped to vertical scrolling, same as the
            // Read card)
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
                        .child(
                            rust_i18n::t!(
                                "file_panel.wrap_truncated",
                                shown = shown,
                                total = total
                            )
                            .to_string(),
                        ),
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
                .child(
                    div()
                        .text_sm()
                        .text_color(subtle)
                        .child(rust_i18n::t!("common.loading")),
                )
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
                // Measure the max line width lazily (text_system is only
                // available on the UI thread); measure once and cache
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
                    // In no-wrap mode long lines scroll horizontally
                    // (trackpad swipe / Shift+wheel / dragging the
                    // scrollbar), and the horizontal scrollbar is always
                    // visible (the default Scrolling mode fades out when
                    // idle, leaving mouse users without their only
                    // horizontal scroll entry)
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
