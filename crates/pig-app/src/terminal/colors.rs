//! ANSI 16/256 色 → gpui 颜色的解析。
//!
//! 暗色 16 色全抄 tty7 `src/terminal/palette.rs` 的 DARK_ANSI16；
//! 亮色 16 色取 tty7 `src/ui/presets.rs` 内建 "light" 主题的 ansi16；
//! 216 色立方 + 24 级灰阶的排布与 tty7 build() 相同（xterm 标准）。

use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor, Rgb};
use gpui_kit::component::{ActiveTheme as _, Theme};
use gpui_kit::{App, Hsla, Rgba};

/// tty7 palette.rs 的暗色 ANSI 16 色
const DARK_ANSI16: [(u8, u8, u8); 16] = [
    (0x2c, 0x2a, 0x26),
    (0xec, 0x6a, 0x78),
    (0x8f, 0xbf, 0x6e),
    (0xe0, 0xb0, 0x72),
    (0x6f, 0xa8, 0xe6),
    (0xc0, 0x8a, 0xdf),
    (0x5f, 0xc2, 0xc9),
    (0xd2, 0xcf, 0xc8),
    (0x6b, 0x66, 0x5d),
    (0xf5, 0x86, 0x8f),
    (0xa8, 0xd9, 0x8a),
    (0xef, 0xc7, 0x8a),
    (0x8f, 0xc0, 0xf5),
    (0xd2, 0xa6, 0xec),
    (0x84, 0xd6, 0xdc),
    (0xf6, 0xf3, 0xec),
];

/// tty7 presets.rs 内建 "light" 主题的亮色 ANSI 16 色
const LIGHT_ANSI16: [(u8, u8, u8); 16] = [
    (0x24, 0x29, 0x2e),
    (0xd1, 0x24, 0x2f),
    (0x1a, 0x7f, 0x37),
    (0x9a, 0x67, 0x00),
    (0x09, 0x69, 0xda),
    (0x82, 0x50, 0xdf),
    (0x1b, 0x7c, 0x83),
    (0x6e, 0x77, 0x81),
    (0x57, 0x60, 0x6a),
    (0xcf, 0x22, 0x2e),
    (0x1f, 0x88, 0x3d),
    (0xbf, 0x87, 0x00),
    (0x21, 0x8b, 0xff),
    (0xa4, 0x75, 0xf9),
    (0x31, 0x92, 0xaa),
    (0x8c, 0x95, 0x9f),
];

/// 完整 256 色表：16 主题色 + 6x6x6 立方 + 24 灰阶
pub(crate) fn build(dark: bool) -> [Rgb; 256] {
    let mut p = [Rgb { r: 0, g: 0, b: 0 }; 256];
    let ansi16 = if dark { DARK_ANSI16 } else { LIGHT_ANSI16 };
    for (i, (r, g, b)) in ansi16.iter().enumerate() {
        p[i] = Rgb {
            r: *r,
            g: *g,
            b: *b,
        };
    }
    let steps = [0u8, 95, 135, 175, 215, 255];
    let mut idx = 16;
    for r in 0..6 {
        for g in 0..6 {
            for b in 0..6 {
                p[idx] = Rgb {
                    r: steps[r],
                    g: steps[g],
                    b: steps[b],
                };
                idx += 1;
            }
        }
    }
    for i in 0..24 {
        let v = 8 + i as u8 * 10;
        p[232 + i] = Rgb { r: v, g: v, b: v };
    }
    p
}

/// 参考 tty7 palette.rs:12（Hsla → alacritty Rgb）
pub(crate) fn hsla_to_rgb(c: Hsla) -> Rgb {
    let rgba = Rgba::from(c);
    Rgb {
        r: (rgba.r * 255.0).round().clamp(0.0, 255.0) as u8,
        g: (rgba.g * 255.0).round().clamp(0.0, 255.0) as u8,
        b: (rgba.b * 255.0).round().clamp(0.0, 255.0) as u8,
    }
}

pub(crate) fn rgb_to_hsla(c: Rgb) -> Hsla {
    Rgba {
        r: c.r as f32 / 255.,
        g: c.g as f32 / 255.,
        b: c.b as f32 / 255.,
        a: 1.,
    }
    .into()
}

/// 一帧渲染用的全部颜色（参考 tty7 element.rs 的 PaintColors::resolve，
/// 砍掉搜索/link 高亮色）。默认前景/背景跟随 pig-app 主题。
pub(crate) struct TermColors {
    pub default_fg: Hsla,
    pub default_bg: Hsla,
    pub caret: Hsla,
    pub selection_bg: Hsla,
    pub fg_rgb: Rgb,
    pub bg_rgb: Rgb,
    pub palette: [Rgb; 256],
}

impl TermColors {
    pub(crate) fn resolve(cx: &App) -> Self {
        let theme: &Theme = cx.theme();
        let default_fg = theme.foreground;
        let default_bg = theme.background;
        // 选区色 = 前景 24% 透明（tty7 同款比例）
        let selection_bg = default_fg.opacity(0.24);
        Self {
            default_fg,
            default_bg,
            caret: theme.caret,
            selection_bg,
            fg_rgb: hsla_to_rgb(default_fg),
            bg_rgb: hsla_to_rgb(default_bg),
            palette: build(theme.is_dark()),
        }
    }
}

/// alacritty 颜色 → (RGB, 是否默认色)。默认前景/背景解析到主题色，
/// 其余进 256 色表（参考 tty7 element.rs:125 的 resolve）。
pub(crate) fn resolve(
    color: AnsiColor,
    palette: &[Rgb; 256],
    default_fg: Rgb,
    default_bg: Rgb,
) -> (Rgb, bool) {
    match color {
        AnsiColor::Spec(rgb) => (rgb, false),
        AnsiColor::Indexed(i) => (palette[i as usize], false),
        AnsiColor::Named(named) => match named {
            NamedColor::Foreground => (default_fg, true),
            NamedColor::Background => (default_bg, true),
            other => {
                let idx = other as usize;
                if idx < 256 {
                    (palette[idx], false)
                } else {
                    (default_fg, true)
                }
            }
        },
    }
}
