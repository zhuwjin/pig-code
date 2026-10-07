//! Terminal rendering core: a self-drawn Element.
//!
//! A trimmed port of tty7 `src/terminal/element.rs` (4883 lines), keeping:
//! - RenderCell / build_grid snapshots (draw the previous frame when try_lock
//!   fails; force a blocking lock on the first frame and after resize)
//! - cell size measurement (shape_line("M") for the width)
//! - merged background runs via paint_quad, per-row segmented shape_line
//!   (force_width for monospace alignment)
//! - inverted block cursor, selection highlight, and IME preedit underline
//!
//! Dropped: powerline glyphs, kitty images, self-drawn boxdraw, search/link
//! highlights, dim/sliver, ink measurement and icon scaling, ligature drift
//! correction (ligatures are already disabled via calt/liga/clig=0, so ASCII
//! runs of a monospace font cannot drift; out-of-bounds parts are clipped by
//! the content mask).

use std::cell::RefCell;
use std::collections::HashMap;

use alacritty_terminal::index::Point as AlacPoint;
use alacritty_terminal::selection::SelectionRange;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::vte::ansi::CursorShape;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{
    App, BorderStyle, Bounds, ContentMask, CursorStyle, Element, ElementId, Font, FontStyle,
    FontWeight, GlobalElementId, Hitbox, HitboxBehavior, HitboxId, Hsla, InspectorElementId,
    IntoElement, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels,
    Point, SharedString, StrikethroughStyle, Style, TextAlign, TextRun, Window, fill, font,
    outline, point, px, relative, size,
};

use super::colors::{TermColors, resolve as resolve_color, rgb_to_hsla};
use super::input::TerminalInputHandler;
use super::view::TerminalView;

/// Opacity for SGR 2 dim (tty7 DIM_OPACITY)
const DIM_OPACITY: f32 = 0.66;

#[derive(Clone, Copy, PartialEq, Default, Debug)]
enum UnderlineKind {
    #[default]
    None,
    Single,
    Curly,
}

#[derive(Clone)]
pub(crate) struct RenderCell {
    c: char,
    /// Zero-width combining chars (already clustered onto the base cell by alacritty)
    marks: Option<Box<[char]>>,
    fg: Hsla,
    bg: Hsla,
    draw_bg: bool,
    bold: bool,
    italic: bool,
    strikeout: bool,
    underline: UnderlineKind,
    underline_color: Option<Hsla>,
    /// Right-half placeholder cell of a wide char
    spacer: bool,
    selected: bool,
}

impl Default for RenderCell {
    fn default() -> Self {
        Self {
            c: ' ',
            marks: None,
            fg: Hsla::default(),
            bg: Hsla::default(),
            draw_bg: false,
            bold: false,
            italic: false,
            strikeout: false,
            underline: UnderlineKind::None,
            underline_color: None,
            spacer: false,
            selected: false,
        }
    }
}

/// alacritty cell → render snapshot (see snapshot_cell at tty7
/// element.rs:195; osc8/link/match dropped; DOUBLE/DOTTED/DASHED underlines
/// approximated as a single line)
fn snapshot_cell(
    cell: &Cell,
    point: AlacPoint,
    colors: &TermColors,
    selection: Option<&SelectionRange>,
) -> RenderCell {
    let flags = cell.flags;
    if flags.contains(Flags::WIDE_CHAR_SPACER) || flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
        return RenderCell {
            spacer: true,
            ..RenderCell::default()
        };
    }

    let inverse = flags.contains(Flags::INVERSE);
    let (mut fgc, _) = resolve_color(cell.fg, &colors.palette, colors.fg_rgb, colors.bg_rgb);
    let (bgc, bg_default) = resolve_color(cell.bg, &colors.palette, colors.fg_rgb, colors.bg_rgb);
    let (fgc, bgc, draw_bg) = if inverse {
        (bgc, fgc, true)
    } else {
        if flags.contains(Flags::HIDDEN) {
            fgc = bgc;
        }
        (fgc, bgc, !bg_default)
    };

    let mut rc = RenderCell {
        c: cell.c,
        marks: cell
            .zerowidth()
            .filter(|marks| !marks.is_empty())
            .map(Box::from),
        fg: rgb_to_hsla(fgc),
        bg: rgb_to_hsla(bgc),
        draw_bg,
        bold: flags.contains(Flags::BOLD) || flags.contains(Flags::BOLD_ITALIC),
        italic: flags.contains(Flags::ITALIC) || flags.contains(Flags::BOLD_ITALIC),
        strikeout: flags.contains(Flags::STRIKEOUT),
        underline: if flags.contains(Flags::UNDERCURL) {
            UnderlineKind::Curly
        } else if flags.intersects(
            Flags::UNDERLINE
                | Flags::DOUBLE_UNDERLINE
                | Flags::DOTTED_UNDERLINE
                | Flags::DASHED_UNDERLINE,
        ) {
            // No native gpui style for double/dotted/dashed lines; approximate with a single line (tty7 self-draws special underlines, dropped here)
            UnderlineKind::Single
        } else {
            UnderlineKind::None
        },
        underline_color: cell.underline_color().map(|c| {
            rgb_to_hsla(resolve_color(c, &colors.palette, colors.fg_rgb, colors.bg_rgb).0)
        }),
        ..RenderCell::default()
    };

    if selection.is_some_and(|s| s.contains(point)) {
        rc.selected = true;
    }
    if flags.contains(Flags::DIM) {
        rc.fg.a *= DIM_OPACITY;
    }
    rc
}

/// Cursor shape (a three-state simplification of alacritty CursorShape)
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum CursorKind {
    Block,
    Bar,
    Underline,
}

impl CursorKind {
    fn from_shape(shape: CursorShape) -> Self {
        match shape {
            CursorShape::Beam => Self::Bar,
            CursorShape::Underline => Self::Underline,
            _ => Self::Block,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct GridCursor {
    pub row: usize,
    pub col: usize,
    hidden: bool,
    kind: CursorKind,
}

#[derive(Clone)]
pub(crate) struct GridSnapshot {
    cursor: Option<GridCursor>,
    any_selected: bool,
}

/// Pixel geometry of the grid (see CellGeom in tty7 element.rs)
#[derive(Clone, Copy)]
struct CellGeom {
    origin: Point<Pixels>,
    cell_width: Pixels,
    line_height: Pixels,
    cols: usize,
    rows: usize,
}

impl CellGeom {
    fn cell_rect(&self, row: usize, col: usize, span: usize) -> Bounds<Pixels> {
        let x = self.origin.x + self.cell_width * (col as f32);
        let y = self.origin.y + self.line_height * (row as f32);
        Bounds::new(
            point(x, y),
            size(self.cell_width * (span as f32), self.line_height),
        )
    }

    /// Pixel position → (col, row, whether left half). Out-of-range positions
    /// are clamped into the grid so drag selection can still paint the boundary
    fn pos_to_cell(&self, pos: Point<Pixels>) -> (usize, usize, bool) {
        let lx = (pos.x - self.origin.x).as_f32().max(0.);
        let ly = (pos.y - self.origin.y).as_f32().max(0.);
        let colf = lx / self.cell_width.as_f32();
        let col = (colf.floor() as usize).min(self.cols.saturating_sub(1));
        let row =
            ((ly / self.line_height.as_f32()).floor() as usize).min(self.rows.saturating_sub(1));
        let left = (colf - colf.floor()) <= 0.5;
        (col, row, left)
    }
}

pub(crate) struct TerminalElement {
    view: gpui_kit::Entity<TerminalView>,
}

impl TerminalElement {
    pub(crate) fn new(view: gpui_kit::Entity<TerminalView>) -> Self {
        Self { view }
    }

    /// Take a one-frame snapshot from the alacritty grid (see build_grid at tty7
    /// element.rs:1839).
    ///
    /// Rendering never queues on the grid lock; the holder may be the reader
    /// thread feeding a large burst of output, and one pane's write speed must
    /// not stall the whole window's frame rate. When the lock is unavailable,
    /// return None and the caller redraws the previous frame; only the first
    /// frame and the frame after a resize (when the old frame has the wrong
    /// shape) use must_block to wait.
    fn build_grid(
        &self,
        colors: &TermColors,
        buf: &mut Vec<RenderCell>,
        rows: usize,
        cols: usize,
        must_block: bool,
        cx: &App,
    ) -> Option<GridSnapshot> {
        let term_arc = self.view.read(cx).terminal.term.clone();
        let term = match term_arc.try_lock_unfair() {
            Some(term) => term,
            None if must_block => term_arc.lock(),
            None => return None,
        };
        // Clear buf only while holding the lock: early returns must leave the previous frame intact for the caller to redraw
        buf.clear();
        buf.resize(rows * cols, RenderCell::default());

        let content = term.renderable_content();
        let display_offset = content.display_offset as i32;
        let selection = content.selection;
        let mut any_selected = false;
        for cell in content.display_iter {
            let row = cell.point.line.0 + display_offset;
            let col = cell.point.column.0;
            if row < 0 || row as usize >= rows || col >= cols {
                continue;
            }
            let rc = snapshot_cell(cell.cell, cell.point, colors, selection.as_ref());
            any_selected |= rc.selected;
            buf[row as usize * cols + col] = rc;
        }

        let cur = content.cursor;
        let crow = cur.point.line.0 + display_offset;
        let cursor = if crow >= 0 && (crow as usize) < rows && cur.point.column.0 < cols {
            Some(GridCursor {
                row: crow as usize,
                col: cur.point.column.0,
                // When the program hides the cursor (?25l) alacritty reports Hidden; the TUI draws its own fake cursor
                hidden: matches!(cur.shape, CursorShape::Hidden),
                kind: CursorKind::from_shape(cur.shape),
            })
        } else {
            None
        };
        Some(GridSnapshot {
            cursor,
            any_selected,
        })
    }

    /// Mouse handlers are registered in paint (geom/hitbox are the current
    /// frame's values). They only handle: press to focus and start a selection,
    /// drag to update, release to finish (see tty7 element.rs:2079; link/menu
    /// dropped).
    fn register_mouse_handlers(&self, geom: CellGeom, hitbox: HitboxId, window: &mut Window) {
        let view = self.view.clone();
        window.on_mouse_event(move |ev: &MouseDownEvent, phase, window, cx| {
            if !phase.bubble() || !hitbox.is_hovered(window) {
                return;
            }
            let (col, row, left) = geom.pos_to_cell(ev.position);
            let button = ev.button;
            let clicks = ev.click_count;
            let shift = ev.modifiers.shift;
            let handle = view.read(cx).focus_handle.clone();
            window.focus(&handle, cx);
            view.update(cx, |v, cx| {
                if button == MouseButton::Left {
                    v.on_select_start(col, row, left, clicks, shift, cx);
                }
            });
        });

        let view = self.view.clone();
        window.on_mouse_event(move |ev: &MouseMoveEvent, _phase, _window, cx| {
            if ev.pressed_button != Some(MouseButton::Left) {
                return;
            }
            let (col, row, left) = geom.pos_to_cell(ev.position);
            view.update(cx, |v, cx| v.on_select_update(col, row, left, cx));
        });

        let view = self.view.clone();
        window.on_mouse_event(move |_ev: &MouseUpEvent, phase, _window, cx| {
            if !phase.bubble() {
                return;
            }
            view.update(cx, |v, cx| v.on_select_end(cx));
        });
    }
}

impl IntoElement for TerminalElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

pub(crate) struct TermLayout {
    cell_width: Pixels,
    line_height: Pixels,
    cols: usize,
    rows: usize,
    hitbox: Hitbox,
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = TermLayout;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        style.flex_grow = 1.0;
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        // Monospace font and size follow the theme (changes in settings take effect immediately)
        let font_size = cx.theme().mono_font_size;
        let base_font = font(cx.theme().mono_font_family.clone());

        // Monospace font: measure the width of "M" as the cell width (same measurement as tty7)
        let sample = window.text_system().shape_line(
            SharedString::new_static("M"),
            font_size,
            &[TextRun {
                len: 1,
                font: base_font,
                color: Hsla::default(),
                background_color: None,
                underline: None,
                strikethrough: None,
            }],
            None,
        );
        let cell_width = sample.width.max(px(1.));
        let line_height_mul = self.view.read(cx).line_height_mul;
        let line_height = px((font_size.as_f32() * line_height_mul).round()).max(px(1.));

        let cols = (bounds.size.width.as_f32() / cell_width.as_f32())
            .floor()
            .max(1.0) as usize;
        let rows = (bounds.size.height.as_f32() / line_height.as_f32())
            .floor()
            .max(1.0) as usize;

        // Only resize when the size actually changed (Term and PTY are both Arc/lock; safe to call in prepaint)
        self.view.update(cx, |view, _| {
            view.set_grid_size(cols, rows, cell_width, line_height, window.scale_factor());
        });

        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);

        TermLayout {
            cell_width,
            line_height,
            cols,
            rows,
            hitbox,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let geom = CellGeom {
            origin: bounds.origin,
            cell_width: prepaint.cell_width,
            line_height: prepaint.line_height,
            cols: prepaint.cols,
            rows: prepaint.rows,
        };
        let colors = TermColors::resolve(cx);
        let font_size = cx.theme().mono_font_size;
        let base_font = font(cx.theme().mono_font_family.clone());
        let focused = self.view.read(cx).focus_handle.is_focused(window);
        let cursor_visible = self.view.read(cx).cursor_visible;

        // Borrow this pane's previous frame: build_grid overwrites it when it gets the lock, otherwise it is redrawn as-is
        let mut buf = self
            .view
            .update(cx, |view, _| std::mem::take(&mut view.grid_buf));
        let previous = self.view.read(cx).grid_snap.clone();
        let must_block = previous.is_none() || buf.len() != geom.rows * geom.cols;
        let built = self.build_grid(&colors, &mut buf, geom.rows, geom.cols, must_block, cx);
        if let Some(snap) = &built {
            let snap = snap.clone();
            self.view.update(cx, |view, _| view.grid_snap = Some(snap));
        }
        let Some(snap) = built.or(previous) else {
            // must_block guarantees build_grid returns a value on both of these paths
            self.view.update(cx, |view, _| view.grid_buf = buf);
            return;
        };
        let cursor = snap.cursor;
        let render_cursor = cursor.filter(|c| !c.hidden);

        // The block cursor is inverted into buf before painting backgrounds (reverse video); the glyph goes through the normal path on top
        if focused
            && cursor_visible
            && let Some(c) = render_cursor
            && c.kind == CursorKind::Block
        {
            invert_cursor_cell(&mut buf, geom.cols, c.row, c.col, &colors);
        }

        let cursor_bounds = cursor.map(|c| geom.cell_rect(c.row, c.col, 1));
        let focus_handle = self.view.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            TerminalInputHandler::new(self.view.clone(), cursor_bounds),
            cx,
        );
        let marked = self.view.read(cx).marked_text.clone();

        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            paint_backgrounds(window, &geom, &buf);
            if snap.any_selected {
                paint_cell_runs(window, &geom, &buf, colors.selection_bg, |c| c.selected);
            }
            paint_glyphs(window, cx, &geom, &buf, font_size, &base_font);
            paint_cursor(
                window,
                &geom,
                render_cursor,
                focused,
                cursor_visible,
                colors.caret,
            );
            paint_marked(
                window,
                cx,
                &geom,
                cursor.map(|c| (c.row, c.col)),
                &marked,
                font_size,
                &base_font,
                colors.default_fg,
                colors.default_bg,
            );
        });

        self.view.update(cx, |view, _| view.grid_buf = buf);
        self.register_mouse_handlers(geom, prepaint.hitbox.id, window);
        window.set_cursor_style(CursorStyle::IBeam, &prepaint.hitbox);
    }
}

/// Background color runs: consecutive same-color cells in a row are merged into
/// one quad (tty7 paint_backgrounds). A wide char's spacer is folded into the run
/// of the lead cell to its left, filling both halves at once.
fn paint_backgrounds(window: &mut Window, geom: &CellGeom, buf: &[RenderCell]) {
    for row in 0..geom.rows {
        let mut col = 0;
        while col < geom.cols {
            let cell = &buf[row * geom.cols + col];
            if !cell.draw_bg {
                col += 1;
                continue;
            }
            let bg = cell.bg;
            let start = col;
            while col < geom.cols {
                let c = &buf[row * geom.cols + col];
                if c.spacer || (c.draw_bg && c.bg == bg) {
                    col += 1;
                } else {
                    break;
                }
            }
            window.paint_quad(fill(geom.cell_rect(row, start, col - start), bg));
        }
    }
}

/// Selection highlight runs (tty7 paint_cell_runs)
fn paint_cell_runs(
    window: &mut Window,
    geom: &CellGeom,
    buf: &[RenderCell],
    color: Hsla,
    mut covered: impl FnMut(&RenderCell) -> bool,
) {
    for row in 0..geom.rows {
        let mut col = 0;
        while col < geom.cols {
            if !covered(&buf[row * geom.cols + col]) {
                col += 1;
                continue;
            }
            let start = col;
            while col < geom.cols {
                let cell = &buf[row * geom.cols + col];
                if covered(cell) || cell.spacer {
                    col += 1;
                } else {
                    break;
                }
            }
            window.paint_quad(fill(geom.cell_rect(row, start, col - start), color));
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
struct GlyphStyle {
    fg: Hsla,
    bold: bool,
    italic: bool,
    strikeout: bool,
    underline: UnderlineKind,
    underline_color: Option<Hsla>,
}

impl GlyphStyle {
    fn of(cell: &RenderCell) -> Self {
        Self {
            fg: cell.fg,
            bold: cell.bold,
            italic: cell.italic,
            strikeout: cell.strikeout,
            underline: cell.underline,
            underline_color: cell.underline_color,
        }
    }

    fn draws_on_blanks(&self) -> bool {
        self.underline != UnderlineKind::None || self.strikeout
    }

    fn underline_style(&self) -> Option<gpui_kit::UnderlineStyle> {
        (self.underline != UnderlineKind::None).then(|| gpui_kit::UnderlineStyle {
            thickness: px(1.),
            color: Some(self.underline_color.unwrap_or(self.fg)),
            wavy: self.underline == UnderlineKind::Curly,
        })
    }

    fn strikethrough_style(&self) -> Option<StrikethroughStyle> {
        self.strikeout.then_some(StrikethroughStyle {
            thickness: px(1.),
            color: Some(self.fg),
        })
    }
}

fn is_blank(cell: &RenderCell) -> bool {
    (cell.c == '\0' || cell.c == ' ') && cell.marks.is_none()
}

#[derive(Debug, PartialEq)]
enum RowSeg {
    /// A contiguous run of same-style ASCII (including skipped blanks in the middle, excluding both ends)
    Run {
        start: usize,
        cells: usize,
        text: String,
    },
    /// Wide char (CJK/emoji), occupying 2 cells
    Wide { start: usize, text: SharedString },
    /// Other single-cell non-ASCII chars
    Solo { col: usize },
    /// Grapheme cluster carrying zero-width combining chars
    Cluster {
        col: usize,
        cells: usize,
        text: String,
        wide_base: bool,
    },
}

fn push_cell(text: &mut String, cell: &RenderCell) {
    text.push(cell.c);
    text.extend(cell.marks.iter().flat_map(|marks| marks.iter()));
}

/// Special combining marks such as Thai Sara Am: alacritty puts them in a
/// separate cell, but typographically they must shape together with the
/// preceding char (see tty7 element.rs:736)
fn is_sara_am(c: char) -> bool {
    matches!(c, '\u{0E33}' | '\u{0EB3}')
}

fn sara_am_at(row: &[RenderCell], col: usize) -> Option<&RenderCell> {
    row.get(col)
        .filter(|cell| !cell.spacer && is_sara_am(cell.c))
}

/// The two regional indicators of a flag emoji must cluster into one grapheme
fn is_regional_indicator(c: char) -> bool {
    matches!(c, '\u{1F1E6}'..='\u{1F1FF}')
}

fn regional_indicator_at(row: &[RenderCell], col: usize) -> Option<&RenderCell> {
    row.get(col)
        .filter(|cell| !cell.spacer && is_regional_indicator(cell.c))
}

/// Slice one row of RenderCell into paintable segments (see segment_row at tty7 element.rs:754)
fn segment_row(row: &[RenderCell]) -> Vec<RowSeg> {
    let mut segs = Vec::new();
    let mut col = 0;
    while col < row.len() {
        let cell = &row[col];
        if cell.spacer {
            col += 1;
            continue;
        }
        if is_blank(cell) {
            let style = GlyphStyle::of(cell);
            if !style.draws_on_blanks() {
                col += 1;
                continue;
            }
            // Blanks carrying underline/strikethrough still need painting (decorations span spaces)
            let start = col;
            let mut text = String::new();
            while col < row.len()
                && !row[col].spacer
                && is_blank(&row[col])
                && GlyphStyle::of(&row[col]) == style
            {
                text.push(' ');
                col += 1;
            }
            segs.push(RowSeg::Run {
                start,
                cells: col - start,
                text,
            });
            continue;
        }
        // Flag pairs: pair them before any combining-mark branch; splitting them would draw two letter boxes
        if is_regional_indicator(cell.c)
            && let Some(next) = regional_indicator_at(row, col + 1)
        {
            let mut text = String::with_capacity(8);
            push_cell(&mut text, cell);
            push_cell(&mut text, next);
            segs.push(RowSeg::Cluster {
                col,
                cells: 2,
                text,
                wide_base: false,
            });
            col += 2;
            continue;
        }
        if let Some(marks) = &cell.marks {
            let wide_base = col + 1 < row.len() && row[col + 1].spacer;
            let mut cells = if wide_base { 2 } else { 1 };
            let mut text = String::with_capacity(1 + marks.len());
            push_cell(&mut text, cell);
            if !wide_base
                && !is_sara_am(cell.c)
                && let Some(am) = sara_am_at(row, col + 1)
            {
                push_cell(&mut text, am);
                cells = 2;
            }
            segs.push(RowSeg::Cluster {
                col,
                cells,
                text,
                wide_base,
            });
            col += cells;
            continue;
        }
        if !cell.c.is_ascii_graphic() {
            if col + 1 < row.len() && row[col + 1].spacer {
                segs.push(RowSeg::Wide {
                    start: col,
                    text: char_string(cell.c),
                });
                col += 2;
            } else if !is_sara_am(cell.c)
                && let Some(am) = sara_am_at(row, col + 1)
            {
                let mut text = String::with_capacity(2);
                push_cell(&mut text, cell);
                push_cell(&mut text, am);
                segs.push(RowSeg::Cluster {
                    col,
                    cells: 2,
                    text,
                    wide_base: false,
                });
                col += 2;
            } else {
                segs.push(RowSeg::Solo { col });
                col += 1;
            }
            continue;
        }

        let style = GlyphStyle::of(cell);
        let start = col;
        let mut text = String::new();
        text.push(cell.c);
        let mut cells = 1;
        col += 1;
        let mut gap = 0;
        while col < row.len() {
            let c = &row[col];
            if is_blank(c) && !c.spacer {
                if style.draws_on_blanks() {
                    break;
                }
                gap += 1;
                col += 1;
                continue;
            }
            if c.spacer
                || c.marks.is_some()
                || !c.c.is_ascii_graphic()
                || GlyphStyle::of(c) != style
            {
                break;
            }
            for _ in 0..gap {
                text.push(' ');
            }
            cells += gap;
            gap = 0;
            text.push(c.c);
            cells += 1;
            col += 1;
        }
        segs.push(RowSeg::Run { start, cells, text });
    }
    segs
}

thread_local! {
    static CHAR_STRINGS: RefCell<HashMap<char, SharedString>> = RefCell::new(HashMap::new());
}

/// SharedString cache for single chars: Solo/Wide segments are built every
/// frame, avoiding repeated allocations (same as tty7 element.rs:898)
fn char_string(c: char) -> SharedString {
    CHAR_STRINGS.with(|m| {
        let mut m = m.borrow_mut();
        m.entry(c)
            .or_insert_with(|| SharedString::from(c.to_string()))
            .clone()
    })
}

/// Build a font with bold/italic applied (tty7 element.rs:149 build_font).
/// Terminals must fully disable ligatures: one ligature occupying a single cell
/// would make the whole line drift. gpui's built-in disable_ligatures only turns
/// off calt, so liga/clig must be named as well (long note at tty7 :167).
fn build_font(base: &Font, bold: bool, italic: bool) -> Font {
    let mut f = base.clone();
    f.weight = if bold {
        FontWeight::BOLD
    } else {
        FontWeight::NORMAL
    };
    f.style = if italic {
        FontStyle::Italic
    } else {
        FontStyle::Normal
    };
    if f.features.tag_value_list().is_empty() {
        f.features = ligatures_off();
    }
    f
}

fn ligatures_off() -> gpui_kit::FontFeatures {
    gpui_kit::FontFeatures(std::sync::Arc::new(vec![
        ("calt".to_string(), 0),
        ("liga".to_string(), 0),
        ("clig".to_string(), 0),
    ]))
}

/// Shape and paint per segment (a trimmed tty7 paint_glyphs: no powerline/boxdraw
/// self-drawing, no ink measurement or scaling, no drift correction; ligatures
/// are off so ASCII runs are naturally monospace, and out-of-bounds parts are
/// clipped by the content mask)
fn paint_glyphs(
    window: &mut Window,
    cx: &mut App,
    geom: &CellGeom,
    buf: &[RenderCell],
    font_size: Pixels,
    base_font: &Font,
) {
    let faces = [
        build_font(base_font, false, false),
        build_font(base_font, true, false),
        build_font(base_font, false, true),
        build_font(base_font, true, true),
    ];

    let run_buf = &mut [TextRun {
        len: 0,
        font: base_font.clone(),
        color: Hsla::default(),
        background_color: None,
        underline: None,
        strikethrough: None,
    }];

    for row in 0..geom.rows {
        let row_base = row * geom.cols;
        let y = geom.origin.y + geom.line_height * (row as f32);
        let row_cells = &buf[row_base..row_base + geom.cols];

        for seg in segment_row(row_cells) {
            let (start, cells, text, force_width) = match seg {
                RowSeg::Run { start, cells, text } => (
                    start,
                    cells,
                    SharedString::from(text),
                    // force_width = cell width: every glyph is clamped into its own cell (monospace alignment)
                    Some(geom.cell_width),
                ),
                RowSeg::Wide { start, text } => (start, 2, text, Some(geom.cell_width * 2.)),
                RowSeg::Solo { col } => (col, 1, char_string(buf[row_base + col].c), None),
                RowSeg::Cluster {
                    col,
                    cells,
                    text,
                    wide_base,
                } => (
                    col,
                    cells,
                    SharedString::from(text),
                    (cells == 2).then(|| geom.cell_width * if wide_base { 2. } else { 1. }),
                ),
            };

            let style = GlyphStyle::of(&buf[row_base + start]);
            let face_ix = (style.bold as usize) | ((style.italic as usize) << 1);
            run_buf[0] = TextRun {
                len: text.len(),
                font: faces[face_ix].clone(),
                color: style.fg,
                background_color: None,
                underline: style.underline_style(),
                strikethrough: style.strikethrough_style(),
            };

            let x = geom.origin.x + geom.cell_width * (start as f32);
            let shaped = window
                .text_system()
                .shape_line(text, font_size, run_buf, force_width);
            // Paint each segment into the cell rect it occupies; oversized glyphs are clipped and do not spill
            let clip = Bounds::new(
                point(x, y),
                size(geom.cell_width * cells as f32, geom.line_height),
            );
            window.with_content_mask(Some(ContentMask { bounds: clip }), |window| {
                _ = shaped.paint(
                    point(x, y),
                    geom.line_height,
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                );
            });
        }
    }
}

/// Turn the cell under the focused block cursor into reverse video: a solid
/// cursor-color background plus a background-colored glyph (tty7
/// invert_cursor_cell, with ink simplified to the default background color)
fn invert_cursor_cell(
    buf: &mut [RenderCell],
    cols: usize,
    row: usize,
    col: usize,
    colors: &TermColors,
) {
    // The TUI may park the cursor on the right half of a wide char; the glyph lives on the lead cell, so step back
    let col = match buf.get(row * cols + col) {
        Some(c) if c.spacer && col > 0 => col - 1,
        _ => col,
    };
    let Some(cell) = buf.get_mut(row * cols + col) else {
        return;
    };
    cell.bg = colors.caret;
    cell.draw_bg = true;
    cell.fg = colors.default_bg;
}

/// Cursor (tty7 paint_cursor): draw a hollow box when unfocused; when focused
/// and the blink phase is on, draw by shape. Block is already inverted in buf,
/// so there is nothing to do here
fn paint_cursor(
    window: &mut Window,
    geom: &CellGeom,
    cursor: Option<GridCursor>,
    focused: bool,
    cursor_visible: bool,
    caret: Hsla,
) {
    let Some(c) = cursor else {
        return;
    };
    let rect = geom.cell_rect(c.row, c.col, 1);
    if !focused {
        window.paint_quad(outline(rect, caret, BorderStyle::Solid));
        return;
    }
    if !cursor_visible {
        return;
    }
    match c.kind {
        CursorKind::Block => {}
        CursorKind::Bar => {
            let w = (geom.cell_width * 0.15).max(px(1.)).min(px(3.));
            window.paint_quad(fill(
                Bounds::new(rect.origin, size(w, rect.size.height)),
                caret,
            ));
        }
        CursorKind::Underline => {
            let h = (geom.line_height * 0.12).max(px(1.)).min(px(3.));
            let y = rect.origin.y + rect.size.height - h;
            window.paint_quad(fill(
                Bounds::new(point(rect.origin.x, y), size(rect.size.width, h)),
                caret,
            ));
        }
    }
}

/// Whether the preedit has anything worth painting: a mix of pure blanks/control
/// chars (some IMEs open an empty composition) must not punch a hole in the
/// cursor cell (a lesson from tty7 #966)
fn preedit_has_ink(marked: &str) -> bool {
    use unicode_segmentation::UnicodeSegmentation as _;
    marked
        .graphemes(true)
        .any(|g| g.chars().any(|c| !c.is_whitespace() && !c.is_control()))
}

/// IME preedit text: draw a theme-colored background plus underlined text at the cursor cell (tty7 paint_marked)
#[allow(clippy::too_many_arguments)] // packing the paint-context params would be more convoluted; allowed per existing convention
fn paint_marked(
    window: &mut Window,
    cx: &mut App,
    geom: &CellGeom,
    cursor: Option<(usize, usize)>,
    marked: &str,
    font_size: Pixels,
    base_font: &Font,
    default_fg: Hsla,
    default_bg: Hsla,
) {
    if !preedit_has_ink(marked) {
        return;
    }
    let Some((row, col)) = cursor else {
        return;
    };
    let x = geom.origin.x + geom.cell_width * (col as f32);
    let y = geom.origin.y + geom.line_height * (row as f32);
    let run = TextRun {
        len: marked.len(),
        font: base_font.clone(),
        color: default_fg,
        background_color: None,
        underline: Some(gpui_kit::UnderlineStyle {
            thickness: px(1.),
            color: Some(default_fg),
            wavy: false,
        }),
        strikethrough: None,
    };
    let shaped = window.text_system().shape_line(
        SharedString::from(marked.to_owned()),
        font_size,
        &[run],
        None,
    );
    let bg_rect = Bounds::new(point(x, y), size(shaped.width, geom.line_height));
    window.paint_quad(fill(bg_rect, default_bg));
    _ = shaped.paint(
        point(x, y),
        geom.line_height,
        TextAlign::Left,
        None,
        window,
        cx,
    );
}
