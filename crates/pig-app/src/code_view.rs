//! 代码视图共享件：按文件后缀/文件名探测语言的 tree-sitter 语法高亮
//!（gpui-kit highlighter，全语言包），以及「行号 gutter + 高亮行」渲染。
//! Read 工具卡（thread_view/read.rs）与右侧文件面板（file_panel.rs）共用。

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use gpui_kit::component::highlighter::{HighlightTheme, Language, SyntaxHighlighter};
use gpui_kit::component::{ActiveTheme as _, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// 代码行高（与 diff 卡同口径：text_xs + 19px 行高）
pub(crate) const CODE_LINE_H: f32 = 19.;

/// 常显横向滚动条的轨道高（gpui-base Scrollbar WIDTH = 4×2+8）：不折行模式下
/// 滚动条常驻底部，内容末尾必须预留一条车道，否则盖住末行
pub(crate) const CODE_SCROLLBAR_LANE: f32 = 16.;

/// 按文件路径探测高亮语言：先按扩展名（gpui-kit Language 内置别名表覆盖
/// rs/py/ts/toml 等常见后缀），无扩展名按整文件名（Makefile 等）；
/// 都不认识回落 "text"（不高亮、也不会失败——SyntaxHighlighter 对无语法
/// 语言返回惰性实例）
pub(crate) fn lang_name_for_path(path: &str) -> &'static str {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let probe = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => ext,
        _ => name,
    };
    Language::from_str(&probe.to_lowercase()).name()
}

/// 一段代码的高亮结果：行字节区间 + 全文样式区间（互不重叠、按起点排序）。
/// 颜色已在计算时定死——主题切换后必须重算（调用方缓存时比对 theme 的 Arc 指针）
pub(crate) struct HighlightedCode {
    pub theme: Arc<HighlightTheme>,
    /// code 中每行的字节区间（不含换行符）
    pub lines: Vec<Range<usize>>,
    /// 全文样式区间（SyntaxHighlighter::styles 输出）
    pub styles: Vec<(Range<usize>, HighlightStyle)>,
}

impl HighlightedCode {
    /// 第 line_ix 行的文本（不含换行符）
    pub fn line_text<'a>(&self, code: &'a str, line_ix: usize) -> &'a str {
        &code[self.lines[line_ix].clone()]
    }

    /// 该行命中的样式区间，平移为行内相对偏移（StyledText::with_highlights 用）
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

/// tree-sitter 高亮入口。SyntaxHighlighter 按语言做线程级缓存（gpui-component
/// component_code_block_highlighter 同款手法：update 前比价换语言，重复解析
/// 靠结果级缓存避免——结果缓存在调用方（卡/面板状态）里，主题切换才重算）
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
        // LanguageRegistry 里的注册名可能与传入名不同（如别名）：换名即重建
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

/// code 中每行的字节区间（不含 \n；末尾无换行时最后一行到 EOF；空串也有一行空行）
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

/// 一行代码：行号 gutter + 高亮文本（StyledText 延迟高亮，跟随父容器
/// font_family/text_xs 文本样式）。
/// wrap=false：固定行高 + 不折行（超长由外层横向滚动承担，gutter 随内容横滚）；
/// wrap=true：代码格自动换行（行高随折行增长），gutter 只盖首行高。
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

/// gutter 宽度按最大行号位数自适应（review 面板 diff 同款口径：10 + 位数×8）
pub(crate) fn gutter_width(max_line_no: usize) -> Pixels {
    let digits = max_line_no.max(1).ilog10() as usize + 1;
    px(10. + digits as f32 * 8.)
}

/// 单行代码（无行号，Bash 卡的命令/输出用）：等宽 + 高亮样式。
/// wrap=false：固定行高不折行（超长由外层横向滚动承担）；true：自动换行
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

/// 一段文本的「高亮 + 行区间 + 最大行宽」打包结果（代码卡内容缓存单元；
/// 主题切换后按 highlighted.theme 的 Arc 指针判等重建）
pub(crate) struct PreparedCode {
    pub code: String,
    pub highlighted: HighlightedCode,
    /// 不折行模式的内容显式宽度（横向滚动驱动）
    pub max_line_width: Pixels,
}

impl PreparedCode {
    /// lang 传 "text" = 纯文本（惰性高亮器不 parse，只有量宽开销）
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

/// 最大行宽（像素）。不显式给宽时，滚动容器内的内容宽度会被布局钳进可用空间
///（ticker 同款坑，两处实测），横向滚动靠这个显式宽度驱动。
/// 逐行精确量宽在大文件上太贵（shape_line × 行数），改为：按估计权重
///（tab=4、非 ASCII=2、其余=1）取前 3 行精确 shape 取最大。等宽字体下
/// 估计权重与真实宽度强相关；字重/字形（高亮样式）在精确 shape 里体现
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
    // 估计权重前 3 的行下标
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
        // 与 StyledText::with_default_highlights 同款的 run 序列：默认字体打底，
        // 高亮区间叠加字重/字形（颜色不影响宽度，取占位黑）
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
    // +2px 防字宽取整误差（measure_ticker_width 同款）
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
    // 显式导入而非 use super::*：super 的 `use gpui_kit::*` 会把 gpui 的
    // test 宏带进子模块遮蔽内置 #[test]，展开无限递归（PLAN.md 记载的坑）
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
        // 无扩展名按整文件名；不认识回落 text
        assert_eq!(lang_name_for_path("Makefile"), "make");
        assert_eq!(lang_name_for_path("Dockerfile"), "text");
        assert_eq!(lang_name_for_path("foo.unknownext"), "text");
        // 隐藏文件（.gitignore）：stem 为空按整文件名探
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
        // 第 0 行（0..3）与样式 1..5 相交 → 行内 1..3
        let styles = highlighted.line_styles(0);
        assert_eq!(styles.len(), 1);
        assert_eq!(styles[0].0, 1..3);
        // 第 1 行（4..7）命中两条：1..5 → 0..1；6..7 → 2..3
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
            "rust 语法应产生多个样式区间: {:?}",
            highlighted.styles
        );
    }

    #[test]
    fn highlight_unknown_lang_falls_back_plain() {
        let theme = HighlightTheme::default_dark();
        let highlighted = highlight_code("hello\nworld", "text", &theme);
        assert_eq!(highlighted.lines.len(), 2);
        // 惰性高亮器返回单个默认区间
        assert_eq!(highlighted.styles.len(), 1);
    }
}
