use gpui_kit::assets::IconName as AssetsIconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use pig_protocol::{GitDiffNote, GitFileChange};

pub struct FileChangeEntry {
    pub path: String,
    pub diff: String,
    pub additions: u32,
    pub deletions: u32,
}

/// Panel data source: git working tree scope (unstaged / staged).
/// The session changes panel has moved to the end of each turn in the
/// message stream (the TurnChanges section of thread_view).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ReviewSource {
    Unstaged,
    Staged,
}

impl ReviewSource {
    fn label(self) -> std::borrow::Cow<'static, str> {
        match self {
            Self::Unstaged => rust_i18n::t!("review.unstaged"),
            Self::Staged => rust_i18n::t!("review.staged"),
        }
    }
}

#[derive(Clone)]
pub enum ReviewEvent {
    /// Request a working tree git status refresh (emitted when the panel
    /// switches source or refresh is clicked; AppView forwards
    /// Op::GitStatus)
    RefreshGit,
    /// Open a git file's diff (AppView forwards Op::GitDiff)
    OpenGitDiff { path: String, staged: bool },
}

impl EventEmitter<ReviewEvent> for ReviewPanel {}

pub struct ReviewPanel {
    /// Session changes snapshot (hidden state: used by the sidebar's
    /// session +N/-N badges and self-tests; not shown in the UI)
    files: Vec<FileChangeEntry>,
    diff_scroll: ScrollHandle,
    source: ReviewSource,
    is_git: bool,
    git_unstaged: Vec<GitFileChange>,
    git_staged: Vec<GitFileChange>,
    git_selected: Option<String>,
    /// Currently open git diff (path, raw diff text, placeholder/truncation
    /// note)
    git_diff: Option<(String, String, Option<GitDiffNote>)>,
    git_diff_loading: bool,
}

impl ReviewPanel {
    pub fn new(_: &mut Context<Self>) -> Self {
        Self {
            files: vec![],
            diff_scroll: ScrollHandle::new(),
            source: ReviewSource::Unstaged,
            is_git: true,
            git_unstaged: vec![],
            git_staged: vec![],
            git_selected: None,
            git_diff: None,
            git_diff_loading: false,
        }
    }

    pub fn upsert(
        &mut self,
        path: String,
        diff: String,
        additions: u32,
        deletions: u32,
        cx: &mut Context<Self>,
    ) {
        // Net delta back to zero (edited back to the original content):
        // removed from the list, consistent with ZCode/kimi-code
        if additions == 0 && deletions == 0 {
            self.remove(&path, cx);
            return;
        }
        if let Some(entry) = self.files.iter_mut().find(|e| e.path == path) {
            entry.diff = diff;
            entry.additions = additions;
            entry.deletions = deletions;
        } else {
            self.files.push(FileChangeEntry {
                path,
                diff,
                additions,
                deletions,
            });
        }
        cx.notify();
    }

    pub fn remove(&mut self, path: &str, cx: &mut Context<Self>) {
        self.files.retain(|e| e.path != path);
        cx.notify();
    }

    pub fn totals(&self) -> (u32, u32) {
        self.files
            .iter()
            .fold((0, 0), |(a, d), e| (a + e.additions, d + e.deletions))
    }

    /// For self-tests: (file count, total additions, total deletions,
    /// whether any diff is non-empty)
    pub fn debug_state(&self) -> (usize, u32, u32, bool) {
        let (adds, dels) = self.totals();
        (
            self.files.len(),
            adds,
            dels,
            self.files.iter().any(|e| !e.diff.is_empty()),
        )
    }

    /// Working tree git status arrived (response to Op::GitStatus)
    pub fn set_git_status(
        &mut self,
        is_git: bool,
        unstaged: Vec<GitFileChange>,
        staged: Vec<GitFileChange>,
        cx: &mut Context<Self>,
    ) {
        self.is_git = is_git;
        self.git_unstaged = unstaged;
        self.git_staged = staged;
        // Clear the diff view when the selected item disappears
        let list = self.git_list();
        if self
            .git_selected
            .as_ref()
            .is_some_and(|sel| !list.iter().any(|e| &e.path == sel))
        {
            self.git_selected = None;
            self.git_diff = None;
        }
        cx.notify();
    }

    /// Single-file git diff arrived (note is the structured placeholder
    /// for truncated/too-large/binary)
    pub fn set_git_diff(
        &mut self,
        path: String,
        diff: String,
        note: Option<GitDiffNote>,
        cx: &mut Context<Self>,
    ) {
        if self.git_selected.as_deref() == Some(path.as_str()) {
            self.git_diff = Some((path, diff, note));
            self.git_diff_loading = false;
            cx.notify();
        }
    }

    fn git_list(&self) -> &Vec<GitFileChange> {
        match self.source {
            ReviewSource::Unstaged => &self.git_unstaged,
            ReviewSource::Staged => &self.git_staged,
        }
    }

    fn git_totals(&self) -> (usize, u32, u32) {
        let list = self.git_list();
        let (a, d) = list
            .iter()
            .fold((0, 0), |(a, d), e| (a + e.additions, d + e.deletions));
        (list.len(), a, d)
    }

    /// Render a unified diff line by line: old/new line number columns +
    /// faint backgrounds on added/deleted lines + a hunk header background.
    /// Line number column widths adapt to this diff's max line number
    /// digits (the empty column of a pure-add/pure-delete file shrinks to
    /// a narrow slit) instead of a fixed 36px that leaves large blanks;
    /// both columns are flex_shrink_0 fixed-width so long lines cannot
    /// squeeze them.
    /// Per-hunk accept/reject is left for M6+.
    fn render_diff(diff: &str, cx: &mut Context<Self>) -> Vec<AnyElement> {
        enum Kind {
            Meta,
            Hunk,
            Add,
            Del,
            Context,
        }
        struct Row {
            kind: Kind,
            old_no: Option<u32>,
            new_no: Option<u32>,
            line: String,
        }
        let mut parsed = Vec::new();
        let mut old_line = 0u32;
        let mut new_line = 0u32;
        let mut max_old = 0u32;
        let mut max_new = 0u32;
        for line in diff.lines() {
            let (kind, old_no, new_no) = if line.starts_with("+++") || line.starts_with("---") {
                (Kind::Meta, None, None)
            } else if line.starts_with("@@") {
                // @@ -old_start,_ +new_start,_ @@
                let mut it = line.split_whitespace();
                old_line = it
                    .nth(1)
                    .and_then(|s| s.strip_prefix('-'))
                    .and_then(|s| s.split(',').next())
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                new_line = it
                    .next()
                    .and_then(|s| s.strip_prefix('+'))
                    .and_then(|s| s.split(',').next())
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                (Kind::Hunk, None, None)
            } else if line.starts_with('+') {
                let no = new_line;
                new_line += 1;
                (Kind::Add, None, Some(no))
            } else if line.starts_with('-') {
                let no = old_line;
                old_line += 1;
                (Kind::Del, Some(no), None)
            } else {
                let (o, n) = (old_line, new_line);
                old_line += 1;
                new_line += 1;
                (Kind::Context, Some(o), Some(n))
            };
            max_old = max_old.max(old_no.unwrap_or(0));
            max_new = max_new.max(new_no.unwrap_or(0));
            parsed.push(Row {
                kind,
                old_no,
                new_no,
                line: line.to_string(),
            });
        }

        // Column width adapts to the max line number digits; a column with
        // no line numbers (pure add/pure delete) shrinks to a narrow slit
        let col_w = |max: u32| {
            if max == 0 {
                px(4.)
            } else {
                px(10. + (max.ilog10() + 1) as f32 * 8.)
            }
        };
        let old_w = col_w(max_old);
        let new_w = col_w(max_new);

        parsed
            .into_iter()
            .map(|row| {
                let (text_color, bg) = match row.kind {
                    Kind::Meta => (cx.theme().muted_foreground, None),
                    Kind::Hunk => (cx.theme().info, Some(cx.theme().accent.opacity(0.5))),
                    Kind::Add => (cx.theme().success, Some(cx.theme().success.opacity(0.08))),
                    Kind::Del => (cx.theme().danger, Some(cx.theme().danger.opacity(0.08))),
                    Kind::Context => (cx.theme().muted_foreground, None),
                };
                h_flex()
                    .w_full()
                    .when_some(bg, |this, bg| this.bg(bg))
                    // Line number columns are fixed-width and do not
                    // shrink: when a long line overflows, flex shrinking
                    // would narrow them and misalign line numbers across
                    // rows (some hug the left, some sit mid-column)
                    .child(
                        div()
                            .w(old_w)
                            .flex_shrink_0()
                            .text_right()
                            .text_color(cx.theme().muted_foreground.opacity(0.6))
                            .child(row.old_no.map(|n| n.to_string()).unwrap_or_default()),
                    )
                    .child(
                        div()
                            .w(new_w)
                            .flex_shrink_0()
                            .text_right()
                            .text_color(cx.theme().muted_foreground.opacity(0.6))
                            .child(row.new_no.map(|n| n.to_string()).unwrap_or_default()),
                    )
                    .child(
                        div()
                            .px_2()
                            .text_color(text_color)
                            .whitespace_nowrap()
                            .child(row.line),
                    )
                    .into_any_element()
            })
            .collect()
    }

    /// Data source switch chip (unstaged / staged)
    fn render_source_tab(&self, source: ReviewSource, cx: &mut Context<Self>) -> AnyElement {
        let active = self.source == source;
        let label = match source {
            ReviewSource::Unstaged => format!("{} {}", source.label(), self.git_unstaged.len()),
            ReviewSource::Staged => format!("{} {}", source.label(), self.git_staged.len()),
        };
        div()
            .id(("review-source", source as usize))
            .px_2()
            .py_0p5()
            .rounded_full()
            .cursor_pointer()
            .text_xs()
            .when(active, |this| {
                this.bg(cx.theme().accent).text_color(cx.theme().foreground)
            })
            .when(!active, |this| {
                this.text_color(cx.theme().muted_foreground)
                    .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.source != source {
                    this.source = source;
                    cx.emit(ReviewEvent::RefreshGit);
                    cx.notify();
                }
            }))
            .child(label)
            .into_any_element()
    }

    /// Git change row: status letter + relative path + +N/-N (click fetches
    /// the git diff)
    fn render_git_row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let staged = self.source == ReviewSource::Staged;
        let entry = self.git_list()[ix].clone();
        let active = self.git_selected.as_deref() == Some(entry.path.as_str());
        let status_color = match entry.status.as_str() {
            "A" | "?" => cx.theme().success,
            "D" => cx.theme().danger,
            "C" => cx.theme().warning,
            _ => cx.theme().muted_foreground,
        };
        let row_path = entry.path.clone();

        h_flex()
            .id(("review-git-file", ix))
            .mx_2()
            .px_2()
            .py_1()
            .gap_2()
            .rounded(cx.theme().radius)
            .when(active, |this| this.bg(cx.theme().accent))
            .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.git_selected = Some(row_path.clone());
                this.git_diff = None;
                this.git_diff_loading = true;
                // Start from the top when switching files
                this.diff_scroll.set_offset(Default::default());
                cx.emit(ReviewEvent::OpenGitDiff {
                    path: row_path.clone(),
                    staged,
                });
                cx.notify();
            }))
            .child(
                div()
                    .w(px(14.))
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(status_color)
                    .child(entry.status.clone()),
            )
            .child(
                div()
                    .text_sm()
                    .flex_1()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .font_family(cx.theme().mono_font_family.clone())
                    .child(entry.path.clone()),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().success)
                    .child(format!("+{}", entry.additions)),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().danger)
                    .child(format!("-{}", entry.deletions)),
            )
            .into_any_element()
    }
}

impl ReviewPanel {
    /// List view: data source tabs + refresh + summary, with the full file
    /// list below
    fn render_list_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let summary = if !self.is_git {
            rust_i18n::t!("review.not_git_repo").to_string()
        } else {
            let (n, adds, dels) = self.git_totals();
            if n == 1 {
                rust_i18n::t!("review.summary_one", n = n, adds = adds, dels = dels).to_string()
            } else {
                rust_i18n::t!("review.summary", n = n, adds = adds, dels = dels).to_string()
            }
        };

        let mut rows = Vec::new();
        if self.is_git {
            for ix in 0..self.git_list().len() {
                rows.push(self.render_git_row(ix, cx));
            }
        }

        v_flex()
            .size_full()
            .child(
                h_flex()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .gap_1()
                    .children(
                        [ReviewSource::Unstaged, ReviewSource::Staged]
                            .into_iter()
                            .map(|source| self.render_source_tab(source, cx)),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("git-refresh")
                            .ghost()
                            .xsmall()
                            .label(rust_i18n::t!("review.refresh"))
                            .on_click(cx.listener(|_, _, _, cx| {
                                cx.emit(ReviewEvent::RefreshGit);
                            })),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(summary),
                    ),
            )
            .child(
                v_flex()
                    .id("review-file-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .py_1()
                    .children(rows)
                    .when(!self.is_git, |this| {
                        this.child(
                            div()
                                .px_3()
                                .py_2()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(rust_i18n::t!("review.not_git_hint")),
                        )
                    })
                    .when(self.is_git && self.git_list().is_empty(), |this| {
                        this.child(
                            div()
                                .px_3()
                                .py_2()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(rust_i18n::t!("review.clean")),
                        )
                    }),
            )
            .into_any_element()
    }

    /// Diff view (the whole panel switches after opening a file): top back
    /// bar (← back to list + head-truncated path + re-fetch) + a
    /// full-height unified diff, no longer stacked above the file list.
    fn render_diff_view(&self, path: String, cx: &mut Context<Self>) -> AnyElement {
        let diff_rows: Vec<AnyElement> = if self.git_diff_loading {
            vec![
                div()
                    .px_2()
                    .py_1()
                    .text_color(cx.theme().muted_foreground)
                    .child(rust_i18n::t!("review.loading_diff"))
                    .into_any_element(),
            ]
        } else {
            let mut rows = self
                .git_diff
                .as_ref()
                .map(|(_, diff, _)| Self::render_diff(diff, cx))
                .unwrap_or_default();
            // Structured placeholder note (truncated/too-large/binary):
            // localized and appended at the end of the diff rows (the diff
            // field is pure diff text; when too-large/binary the diff is
            // empty and this row is all the content)
            if let Some(note) = self.git_diff.as_ref().and_then(|(_, _, note)| *note) {
                rows.push(
                    div()
                        .px_2()
                        .py_1()
                        .text_color(cx.theme().muted_foreground)
                        .child(git_diff_note_text(note))
                        .into_any_element(),
                );
            }
            rows
        };
        let staged = self.source == ReviewSource::Staged;
        let refresh_path = path.clone();

        v_flex()
            .size_full()
            .child(
                h_flex()
                    .w_full()
                    .px_2()
                    .py_1p5()
                    .gap_1()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        div()
                            .id("diff-back")
                            .p_1()
                            .rounded(cx.theme().radius)
                            .cursor_pointer()
                            .hover(|this| this.bg(cx.theme().accent))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.git_selected = None;
                                this.git_diff = None;
                                this.git_diff_loading = false;
                                cx.notify();
                            }))
                            .child(
                                Icon::new(AssetsIconName::ArrowLeft)
                                    .size_4()
                                    .text_color(cx.theme().muted_foreground),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_sm()
                            .font_family(cx.theme().mono_font_family.clone())
                            .child(shorten_path(&path, 30)),
                    )
                    .child(
                        div()
                            .id("diff-refresh")
                            .p_1()
                            .rounded(cx.theme().radius)
                            .cursor_pointer()
                            .hover(|this| this.bg(cx.theme().accent))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.git_diff = None;
                                this.git_diff_loading = true;
                                cx.emit(ReviewEvent::OpenGitDiff {
                                    path: refresh_path.clone(),
                                    staged,
                                });
                                cx.notify();
                            }))
                            .child(
                                Icon::new(AssetsIconName::RotateCw)
                                    .size_4()
                                    .text_color(cx.theme().muted_foreground),
                            ),
                    ),
            )
            .child(
                div()
                    .id("diff-view")
                    .flex_1()
                    .min_h_0()
                    .px_2()
                    .py_1()
                    .overflow_y_scroll()
                    .track_scroll(&self.diff_scroll)
                    .text_xs()
                    .font_family(cx.theme().mono_font_family.clone())
                    .children(diff_rows),
            )
            .into_any_element()
    }
}

/// GitDiffNote → localized placeholder text (the truncated/too-large/binary
/// note of the git panel's diff view)
fn git_diff_note_text(note: GitDiffNote) -> String {
    match note {
        GitDiffNote::Truncated => rust_i18n::t!("git.note.truncated").to_string(),
        GitDiffNote::TooLarge => rust_i18n::t!("git.note.too_large").to_string(),
        GitDiffNote::Binary => rust_i18n::t!("git.note.binary").to_string(),
    }
}

/// Head truncation for paths: keeps the tail when too long (the file name
/// matters most), with the cut landing on a path segment boundary
pub(crate) fn shorten_path(path: &str, max_chars: usize) -> String {
    let len = path.chars().count();
    if len <= max_chars {
        return path.to_string();
    }
    let keep = max_chars.saturating_sub(2); // "…/" takes two characters
    let byte_ix = path
        .char_indices()
        .nth(len - keep)
        .map(|(ix, _)| ix)
        .unwrap_or(0);
    let tail = &path[byte_ix..];
    match tail.find('/') {
        Some(ix) => format!("…/{}", &tail[ix + 1..]),
        None => format!("…{tail}"),
    }
}

impl Render for ReviewPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Opening a file switches the whole panel to the diff view; ←
        // returns to the file list
        match self.git_selected.clone() {
            Some(path) => self.render_diff_view(path, cx),
            None => self.render_list_view(cx),
        }
    }
}
