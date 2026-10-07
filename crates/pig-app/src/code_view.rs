//! Shared code-view pieces: tree-sitter syntax highlighting with language
//! detection by file extension/name (gpui-kit highlighter, full language
//! pack), plus "line number gutter + highlighted line" rendering.
//! Shared by the Read tool card (thread_view/read.rs) and the right file
//! panel (file_panel.rs).

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use gpui_kit::component::highlighter::{HighlightTheme, Language, SyntaxHighlighter};
use gpui_kit::component::{ActiveTheme as _, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// Code line height (same metric as the diff card: text_xs + 19px line height)
pub(crate) const CODE_LINE_H: f32 = 19.;

/// Track height of the always-visible horizontal scrollbar (gpui-base
/// Scrollbar WIDTH = 4×2+8): in no-wrap mode the scrollbar stays at the bottom,
/// so the content must reserve a lane at the end or it covers the last line
pub(crate) const CODE_SCROLLBAR_LANE: f32 = 16.;

/// Detect the highlighting language from a file path: first by extension
/// (gpui-kit Language's built-in alias table covers common suffixes like
/// rs/py/ts/toml), then by whole file name when there is no extension
/// (Makefile etc.); falls back to "text" when nothing matches (no
/// highlighting and no failure either: SyntaxHighlighter returns a lazy
/// instance for languages without a grammar)
pub(crate) fn lang_name_for_path(path: &str) -> &'static str {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let probe = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => ext,
        _ => name,
    };
    Language::from_str(&probe.to_lowercase()).name()
}

/// Highlight result for a piece of code: per-line byte ranges plus whole-text
/// style ranges (non-overlapping, sorted by start).
/// Colors are fixed at computation time: must be recomputed after a theme
/// switch (callers cache by comparing the theme's Arc pointer)
pub(crate) struct HighlightedCode {
    pub theme: Arc<HighlightTheme>,
    /// Byte range of each line in code (excluding the newline)
    pub lines: Vec<Range<usize>>,
    /// Whole-text style ranges (output of SyntaxHighlighter::styles)
    pub styles: Vec<(Range<usize>, HighlightStyle)>,
}

impl HighlightedCode {
    /// Text of line line_ix (excluding the newline)
    pub fn line_text<'a>(&self, code: &'a str, line_ix: usize) -> &'a str {
        &code[self.lines[line_ix].clone()]
    }

    /// Style ranges hit by this line, shifted to line-relative offsets (for
    /// StyledText::with_highlights)
    pub fn line_styles(&self, line_ix: usize) -> Vec<(Range<usize>, HighlightStyle)> {
        let line = &self.lines[line_ix];
        self.styles
            .iter()
            .filter_map(|(range, style)| {
                let start = range.start.max(line.start);
                let end = range.end.min(line.end);
                (start < end).then(|| (start - line.start..end - line.start, *style))
            })
            .collect()
    }
}

/// Entry point for tree-sitter highlighting. SyntaxHighlighter caches per
/// language at thread level (same approach as gpui-component's
/// component_code_block_highlighter: compare and swap the language before
/// update; repeated parsing is avoided via result-level caching: results are
/// cached by the caller (card/panel state) and only recomputed on theme
/// switch)
pub(crate) fn highlight_code(
    code: &str,
    lang: &str,
    theme: &Arc<HighlightTheme>,
) -> HighlightedCode {
    std::thread_local! {
        static HIGHLIGHTERS: RefCell<HashMap<SharedString, SyntaxHighlighter>> =
            RefCell::new(HashMap::new());
    }
    let styles = HIGHLIGHTERS.with(|cache| {
        let mut cache = cache.borrow_mut();
        let highlighter = cache
            .entry(lang.into())
            .or_insert_with(|| SyntaxHighlighter::new(lang));
        // The registered name in LanguageRegistry may differ from the
        // passed-in name (e.g. aliases): rebuild when it changes
        if highlighter.language() != lang {
            *highlighter = SyntaxHighlighter::new(lang);
        }
        let rope = ropey::Rope::from_str(code);
        highlighter.update(None, &rope, None);
        highlighter.styles(&(0..code.len()), theme.as_ref())
    });
    HighlightedCode {
        theme: theme.clone(),
        lines: line_ranges(code),
        styles,
    }
}

/// Byte range of each line in code (excluding \n; the last line ends at EOF
/// when there is no trailing newline; an empty string still yields one empty
/// line)
fn line_ranges(code: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut start = 0;
    for (ix, byte) in code.bytes().enumerate() {
        if byte == b'\n' {
            ranges.push(start..ix);
            start = ix + 1;
        }
    }
    ranges.push(start..code.len());
    ranges
}

/// One code line: line number gutter + highlighted text (StyledText lazy
/// highlighting, follows the parent container's font_family/text_xs text
/// style).
/// wrap=false: fixed line height and no wrapping (overlong lines are handled
/// by the outer horizontal scrolling, the gutter scrolls horizontally with
/// the content); wrap=true: the code cell wraps automatically (line height
/// grows with wrapping) and the gutter only covers the first line's height.
pub(crate) fn code_line_row(
    line_no: usize,
    text: &str,
    styles: Vec<(Range<usize>, HighlightStyle)>,
    gutter_w: Pixels,
    gutter_color: Hsla,
    wrap: bool,
) -> AnyElement {
    let code_cell = div()
        .pl_3()
        .pr_3()
        .child(StyledText::new(text.to_string()).with_highlights(styles));
    h_flex()
        .w_full()
        .items_start()
        .map(|this| {
            if wrap {
                this.min_h(px(CODE_LINE_H))
            } else {
                this.h(px(CODE_LINE_H))
            }
        })
        .child(
            div()
                .w(gutter_w)
                .flex_shrink_0()
                .pr_2()
                .text_right()
                .text_color(gutter_color)
                .child(line_no.to_string()),
        )
        .child(if wrap {
            code_cell.flex_1().min_w_0().into_any_element()
        } else {
            code_cell
                .flex_shrink_0()
                .whitespace_nowrap()
                .into_any_element()
        })
        .into_any_element()
}

/// Gutter width adapts to the digit count of the max line number (same metric
/// as the review panel diff: 10 + digits×8)
pub(crate) fn gutter_width(max_line_no: usize) -> Pixels {
    let digits = max_line_no.max(1).ilog10() as usize + 1;
    px(10. + digits as f32 * 8.)
}

/// Single code line (no line number, used by the Bash card for
/// command/output): monospace + highlight styles.
/// wrap=false: fixed line height without wrapping (overlong lines are handled
/// by the outer horizontal scrolling); true: wraps automatically
pub(crate) fn code_line(
    text: &str,
    styles: Vec<(Range<usize>, HighlightStyle)>,
    wrap: bool,
) -> AnyElement {
    let cell = div()
        .pl_3()
        .pr_3()
        .child(StyledText::new(text.to_string()).with_highlights(styles));
    div()
        .w_full()
        .map(|this| {
            if wrap {
                this.min_h(px(CODE_LINE_H))
            } else {
                this.h(px(CODE_LINE_H))
            }
        })
        .child(if wrap {
            cell.w_full().into_any_element()
        } else {
            cell.flex_shrink_0().whitespace_nowrap().into_any_element()
        })
        .into_any_element()
}

/// Packaged "highlight + line ranges + max line width" result for a piece of
/// text (the caching unit of code card content; rebuilt on theme switch by
/// comparing the highlighted.theme Arc pointer for equality)
pub(crate) struct PreparedCode {
    pub code: String,
    pub highlighted: HighlightedCode,
    /// Explicit content width in no-wrap mode (drives horizontal scrolling)
    pub max_line_width: Pixels,
}

impl PreparedCode {
    /// Passing "text" as lang means plain text (the lazy highlighter does not
    /// parse; only width measuring cost)
    pub(crate) fn build(
        code: String,
        lang: &str,
        theme: &Arc<HighlightTheme>,
        window: &Window,
        cx: &App,
    ) -> Self {
        let highlighted = highlight_code(&code, lang, theme);
        let max_line_width = measure_max_line_width(&code, &highlighted, window, cx);
        Self {
            code,
            highlighted,
            max_line_width,
        }
    }

    pub(crate) fn line_count(&self) -> usize {
        self.highlighted.lines.len()
    }

    pub(crate) fn line_text(&self, line_ix: usize) -> &str {
        self.highlighted.line_text(&self.code, line_ix)
    }

    pub(crate) fn line_styles(&self, line_ix: usize) -> Vec<(Range<usize>, HighlightStyle)> {
        self.highlighted.line_styles(line_ix)
    }
}

/// Max line width in pixels. Without an explicit width, the content width
/// inside a scroll container gets clamped to the available space by layout
/// (same pitfall as the ticker, verified in both places); horizontal
/// scrolling is driven by this explicit width.
/// Exact per-line measuring is too expensive on large files (shape_line ×
/// line count), so instead: pick the top 3 lines by estimated weight
/// (tab=4, non-ASCII=2, others=1), shape them exactly and take the max. In a
/// monospace font the estimated weight correlates strongly with the real
/// width; weight/glyph shape (highlight styles) shows up in the exact shaping
pub(crate) fn measure_max_line_width(
    code: &str,
    highlighted: &HighlightedCode,
    window: &Window,
    cx: &App,
) -> Pixels {
    let font_size = rems(0.75).to_pixels(window.rem_size());
    let font = Font {
        family: cx.theme().mono_font_family.clone(),
        ..Font::default()
    };
    let text_system = window.text_system();
    // Line indexes of the top 3 by estimated weight
    let mut top: Vec<(usize, usize)> = Vec::with_capacity(4);
    for (ix, range) in highlighted.lines.iter().enumerate() {
        let weight = code[range.clone()]
            .chars()
            .map(|ch| {
                if ch == '\t' {
                    4
                } else if ch.is_ascii() {
                    1
                } else {
                    2
                }
            })
            .sum::<usize>();
        top.push((weight, ix));
        top.sort_unstable_by(|a, b| b.cmp(a));
        top.truncate(3);
    }
    let mut max = px(0.);
    for (_, ix) in top {
        let text = highlighted.line_text(code, ix);
        if text.is_empty() {
            continue;
        }
        // Same run sequence as StyledText::with_default_highlights: default
        // font as the base, highlight ranges overlay weight/glyph shape (color
        // does not affect width; use placeholder black)
        let mut runs: Vec<TextRun> = Vec::new();
        let mut cursor = 0;
        for (range, style) in highlighted.line_styles(ix) {
            if cursor < range.start {
                runs.push(text_run(&font, range.start - cursor));
            }
            let mut styled = font.clone();
            if let Some(weight) = style.font_weight {
                styled.weight = weight;
            }
            if let Some(font_style) = style.font_style {
                styled.style = font_style;
            }
            runs.push(text_run(&styled, range.end - range.start));
            cursor = range.end;
        }
        if cursor < text.len() {
            runs.push(text_run(&font, text.len() - cursor));
        }
        let width = text_system
            .shape_line(SharedString::from(text.to_string()), font_size, &runs, None)
            .width;
        if width > max {
            max = width;
        }
    }
    // +2px guards against glyph width rounding error (same as measure_ticker_width)
    max + px(2.)
}

fn text_run(font: &Font, len: usize) -> TextRun {
    TextRun {
        len,
        font: font.clone(),
        color: black(),
        background_color: None,
        underline: None,
        strikethrough: None,
    }
}

#[cfg(test)]
mod tests {
    // Explicit imports instead of use super::*: super's `use gpui_kit::*`
    // pulls gpui's test macro into the submodule, shadowing the built-in
    // #[test] and recursing infinitely on expansion (pitfall recorded in
    // PLAN.md)
    use super::{HighlightedCode, highlight_code, lang_name_for_path, line_ranges};
    use gpui_kit::HighlightStyle;
    use gpui_kit::component::highlighter::HighlightTheme;

    #[test]
    fn lang_name_by_extension() {
        assert_eq!(lang_name_for_path("src/main.rs"), "rust");
        assert_eq!(lang_name_for_path("a/b/App.TSX"), "tsx");
        assert_eq!(lang_name_for_path("config.toml"), "toml");
        assert_eq!(lang_name_for_path("README.md"), "markdown");
        assert_eq!(lang_name_for_path("data.json"), "json");
        // No extension: probe by whole file name; fall back to text when unknown
        assert_eq!(lang_name_for_path("Makefile"), "make");
        assert_eq!(lang_name_for_path("Dockerfile"), "text");
        assert_eq!(lang_name_for_path("foo.unknownext"), "text");
        // Hidden file (.gitignore): stem is empty, probe by whole file name
        assert_eq!(lang_name_for_path(".gitignore"), "text");
    }

    #[test]
    fn line_ranges_split() {
        let code = "ab\n\ncd\n";
        let ranges = line_ranges(code);
        assert_eq!(ranges, vec![0..2, 3..3, 4..6, 7..7]);
        assert_eq!(line_ranges(""), vec![0..0]);
    }

    #[test]
    fn line_styles_shift_to_line_relative() {
        let theme = HighlightTheme::default_dark();
        let highlighted = HighlightedCode {
            theme,
            lines: vec![0..3, 4..7],
            styles: vec![
                (
                    1..5,
                    HighlightStyle {
                        ..Default::default()
                    },
                ),
                (6..7, HighlightStyle::default()),
            ],
        };
        // Line 0 (0..3) intersects style 1..5 → in-line 1..3
        let styles = highlighted.line_styles(0);
        assert_eq!(styles.len(), 1);
        assert_eq!(styles[0].0, 1..3);
        // Line 1 (4..7) hits two ranges: 1..5 → 0..1; 6..7 → 2..3
        let styles = highlighted.line_styles(1);
        assert_eq!(styles.len(), 2);
        assert_eq!(styles[0].0, 0..1);
        assert_eq!(styles[1].0, 2..3);
    }

    #[test]
    fn highlight_rust_produces_styles() {
        let theme = HighlightTheme::default_dark();
        let highlighted = highlight_code("fn main() {}", "rust", &theme);
        assert_eq!(highlighted.lines, vec![0..12]);
        assert!(
            highlighted.styles.len() > 1,
            "rust syntax should produce multiple style ranges: {:?}",
            highlighted.styles
        );
    }

    #[test]
    fn highlight_unknown_lang_falls_back_plain() {
        let theme = HighlightTheme::default_dark();
        let highlighted = highlight_code("hello\nworld", "text", &theme);
        assert_eq!(highlighted.lines.len(), 2);
        // The lazy highlighter returns a single default range
        assert_eq!(highlighted.styles.len(), 1);
    }
}
