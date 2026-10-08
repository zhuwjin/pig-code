//! Glob/Grep result list card: the two search tools' expanded area is a plain
//! rounded box (same chrome as the generic tool box) whose rows are the
//! results — one clickable row per result (Grep content mode: elided path +
//! matched line content; Glob / files_with_matches: path only). Clicking a row
//! opens the file in the right-side panel at that line (ThreadEvent::OpenFile;
//! the panel scrolls to and highlights the target line). Bracketed/
//! parenthesized footer notes render as dim text rows; unparsable or empty
//! output falls back to the generic box.
//!
//! (Not to be confused with search.rs, the in-session Ctrl+F search.)

use super::*;

/// Render row cap
const MAX_ROWS: usize = 400;
/// Path column width on Grep content rows (content takes the rest)
const PATH_COL_W: f32 = 220.;

/// One parsed result row
pub(crate) struct SearchResultRow {
    /// Workspace-relative (or absolute) path, as printed by core
    pub path: String,
    /// 1-based line number (Grep content rows)
    pub line: Option<usize>,
    /// Matched line content (Grep content rows) / the count (count-mode rows)
    pub content: Option<String>,
}

/// Parsed search output: clickable rows + trailing note lines
pub(crate) struct SearchResults {
    pub rows: Vec<SearchResultRow>,
    pub notes: Vec<String>,
}

/// Footer/note line shapes shared by both tools: "[...]" footers and
/// "(no matches)"-style parenthesized notes
fn is_note_line(line: &str) -> bool {
    (line.starts_with('[') && line.ends_with(']')) || line.starts_with('(')
}

/// Glob output: path lines in the first blank-line-separated section,
/// bracketed footer notes in the following sections (core joins the sections
/// with "\n\n"). None when there is no result row ("(no matching files)")
pub(crate) fn parse_glob_output(output: &str) -> Option<SearchResults> {
    let mut sections = output.split("\n\n");
    let first = sections.next()?;
    let mut rows = Vec::new();
    let mut notes = Vec::new();
    for line in first.lines().filter(|l| !l.trim().is_empty()) {
        if is_note_line(line) {
            notes.push(line.to_string());
        } else {
            rows.push(SearchResultRow {
                path: line.to_string(),
                line: None,
                content: None,
            });
        }
    }
    if rows.is_empty() {
        return None;
    }
    notes.extend(
        sections
            .flat_map(|s| s.lines())
            .filter(|l| !l.trim().is_empty())
            .map(str::to_string),
    );
    Some(SearchResults { rows, notes })
}

/// Grep output: content mode gives `{path}:{line}: {content}` rows (context
/// lines included, they open their own line too); the `--` window separators
/// are dropped; bracketed/parenthesized footers become notes; anything else
/// (files_with_matches bare paths, `{path}:{count}` rows) becomes a path-only
/// row. None when there is no result row ("(no matches)")
pub(crate) fn parse_grep_output(output: &str) -> Option<SearchResults> {
    let mut rows = Vec::new();
    let mut notes = Vec::new();
    for line in output.lines() {
        if line.trim().is_empty() || line == "--" {
            continue;
        }
        if is_note_line(line) {
            notes.push(line.to_string());
            continue;
        }
        if let Some(row) = parse_content_row(line) {
            rows.push(row);
            continue;
        }
        // Count mode: `{path}:{count}` (no space after the second colon) —
        // open the file, show the count as the row content
        if let Some((path, count)) = line.rsplit_once(':')
            && !path.is_empty()
            && !count.is_empty()
            && count.bytes().all(|b| b.is_ascii_digit())
        {
            rows.push(SearchResultRow {
                path: path.to_string(),
                line: None,
                content: Some(count.to_string()),
            });
            continue;
        }
        rows.push(SearchResultRow {
            path: line.to_string(),
            line: None,
            content: None,
        });
    }
    (!rows.is_empty()).then_some(SearchResults { rows, notes })
}

/// `{path}:{line}: {content}` — the split point is the FIRST colon followed by
/// digits + ": " (drive letters `C:` and colons inside the path never match
/// that shape; content containing colons stays intact)
fn parse_content_row(line: &str) -> Option<SearchResultRow> {
    let bytes = line.as_bytes();
    for (ix, &b) in bytes.iter().enumerate() {
        if b != b':' {
            continue;
        }
        let rest = &line[ix + 1..];
        let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if digits > 0 && rest[digits..].starts_with(": ") {
            let no = rest[..digits].parse().ok()?;
            return Some(SearchResultRow {
                path: line[..ix].to_string(),
                line: Some(no),
                content: Some(rest[digits + 2..].to_string()),
            });
        }
    }
    None
}

impl ThreadView {
    /// The Glob/Grep expanded result list (see the module docs)
    pub(crate) fn render_search_card(
        &self,
        message_ix: usize,
        segment_ix: usize,
        results: &SearchResults,
        body_scroll: &ScrollHandle,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let subtle = cx.theme().muted_foreground;
        let subtlest = subtle.opacity(0.6);
        let key = message_ix * 1024 + segment_ix;
        let total = results.rows.len();
        let shown = total.min(MAX_ROWS);
        let mut row_els: Vec<AnyElement> = Vec::new();
        for (rix, row) in results.rows.iter().take(shown).enumerate() {
            let path = row.path.clone();
            let line = row.line;
            // Tooltip: the full path (the cell elides it) + the line number
            let tip = match line {
                Some(line) => format!("{path}:{line}"),
                None => path.clone(),
            };
            // Row hover turns the text blue (driven by the on_hover listener
            // below + the search_row_hover state; group_hover proved
            // unreliable on these deeply nested rows). The blue is the
            // palette's blue-500 — the theme's `info` (cyan-400) reads too
            // teal next to the reference, and blue-400 not blue enough
            let hover_blue = gpui_kit::component::theme::blue_500();
            let hovered = self.search_row_hover == Some((message_ix, segment_ix, rix));
            let text_color = if hovered {
                hover_blue
            } else {
                cx.theme().foreground
            };
            // Path cell: fixed-width elided column when the row carries
            // content; full-width when the path is the whole result. Base
            // color: path-only rows (Glob, files_with_matches) are the result
            // itself and get the same bright foreground as the code-card
            // bodies; on content rows the path is a locator and stays muted
            let path_color = if hovered {
                hover_blue
            } else if row.content.is_some() {
                subtle
            } else {
                cx.theme().foreground
            };
            let path_cell = div()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_color(path_color)
                .child(row.path.clone());
            let path_cell = if row.content.is_some() {
                path_cell.flex_shrink_0().w(px(PATH_COL_W))
            } else {
                path_cell.flex_1().min_w_0()
            };
            row_els.push(
                h_flex()
                    .id(("search-row", key * 512 + rix))
                    .test_support()
                    .w_full()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py(px(2.))
                    .rounded_md()
                    .cursor_pointer()
                    .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                    .on_hover(cx.listener(move |this, is_hovered, _, cx| {
                        let key = Some((message_ix, segment_ix, rix));
                        // Notify on transitions only (no per-move re-render)
                        if (*is_hovered && this.search_row_hover != key)
                            || (!*is_hovered && this.search_row_hover == key)
                        {
                            this.search_row_hover = if *is_hovered { key } else { None };
                            cx.notify();
                        }
                    }))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(ThreadEvent::OpenFile {
                            path: path.clone(),
                            line,
                        });
                    }))
                    .child(path_cell)
                    .when_some(row.content.clone(), |this, content| {
                        this.child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_color(text_color)
                                .child(content),
                        )
                    })
                    .into_any_element(),
            );
        }
        if total > shown {
            row_els.push(
                div()
                    .w_full()
                    .py_1()
                    .text_center()
                    .text_color(subtlest)
                    .child(rust_i18n::t!("thread.omitted_lines", n = total - shown).to_string())
                    .into_any_element(),
            );
        }
        // Trailing notes (pagination budget/skipped files etc., core-provided
        // English text shown verbatim)
        for note in &results.notes {
            row_els.push(
                div()
                    .w_full()
                    .px_2()
                    .py_1()
                    .text_color(subtlest)
                    .child(note.clone())
                    .into_any_element(),
            );
        }

        div()
            .w_full()
            .rounded_xl()
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().secondary)
            // Small outer padding: the rows carry their own, and their hover
            // background should not touch the border
            .p_1()
            .text_xs()
            .line_height(px(CODE_LINE_H))
            .font_family(cx.theme().mono_font_family.clone())
            .child(
                div()
                    .id(("search-body", key))
                    .w_full()
                    .max_h(px(super::cards::GENERIC_BOX_MAX_H))
                    .overflow_y_scroll()
                    // Vertical-only container: without the axis lock a
                    // horizontal wheel gesture would scroll vertically
                    .restrict_scroll_to_axis()
                    .track_scroll(body_scroll)
                    .child(v_flex().w_full().children(row_els)),
            )
            .into_any_element()
    }
}
