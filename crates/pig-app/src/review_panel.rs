use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

pub struct FileChangeEntry {
    pub path: String,
    pub diff: String,
    pub additions: u32,
    pub deletions: u32,
}

#[derive(Clone)]
pub enum ReviewEvent {
    Revert(String),
}

impl EventEmitter<ReviewEvent> for ReviewPanel {}

pub struct ReviewPanel {
    files: Vec<FileChangeEntry>,
    selected: Option<String>,
}

impl ReviewPanel {
    pub fn new(_: &mut Context<Self>) -> Self {
        Self {
            files: vec![],
            selected: None,
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
        if let Some(entry) = self.files.iter_mut().find(|e| e.path == path) {
            entry.diff = diff;
            entry.additions = additions;
            entry.deletions = deletions;
        } else {
            self.files.push(FileChangeEntry {
                path: path.clone(),
                diff,
                additions,
                deletions,
            });
        }
        if self.selected.is_none() {
            self.selected = Some(path);
        }
        cx.notify();
    }

    pub fn remove(&mut self, path: &str, cx: &mut Context<Self>) {
        self.files.retain(|e| e.path != path);
        if self.selected.as_deref() == Some(path) {
            self.selected = self.files.first().map(|e| e.path.clone());
        }
        cx.notify();
    }

    pub fn totals(&self) -> (u32, u32) {
        self.files
            .iter()
            .fold((0, 0), |(a, d), e| (a + e.additions, d + e.deletions))
    }

    /// 自测用：(文件数, 总新增, 总删除, 任一 diff 非空)
    pub fn debug_state(&self) -> (usize, u32, u32, bool) {
        let (adds, dels) = self.totals();
        (
            self.files.len(),
            adds,
            dels,
            self.files.iter().any(|e| !e.diff.is_empty()),
        )
    }

    /// 逐行渲染 unified diff：旧/新行号两列 + 增删行淡底色 + hunk 头底色。
    /// 逐 hunk 接受/拒绝留 M6+。
    fn render_diff(diff: &str, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut rows = Vec::new();
        let mut old_line = 0u32;
        let mut new_line = 0u32;
        for line in diff.lines() {
            enum Kind {
                Meta,
                Hunk,
                Add,
                Del,
                Context,
            }
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

            let (text_color, bg) = match kind {
                Kind::Meta => (cx.theme().muted_foreground, None),
                Kind::Hunk => (cx.theme().info, Some(cx.theme().accent.opacity(0.5))),
                Kind::Add => (cx.theme().success, Some(cx.theme().success.opacity(0.08))),
                Kind::Del => (cx.theme().danger, Some(cx.theme().danger.opacity(0.08))),
                Kind::Context => (cx.theme().muted_foreground, None),
            };
            let line = line.to_string();
            rows.push(
                h_flex()
                    .w_full()
                    .when_some(bg, |this, bg| this.bg(bg))
                    .child(
                        div()
                            .w(px(36.))
                            .text_right()
                            .text_color(cx.theme().muted_foreground.opacity(0.6))
                            .child(old_no.map(|n| n.to_string()).unwrap_or_default()),
                    )
                    .child(
                        div()
                            .w(px(36.))
                            .text_right()
                            .text_color(cx.theme().muted_foreground.opacity(0.6))
                            .child(new_no.map(|n| n.to_string()).unwrap_or_default()),
                    )
                    .child(
                        div()
                            .px_2()
                            .text_color(text_color)
                            .whitespace_nowrap()
                            .child(line),
                    )
                    .into_any_element(),
            );
        }
        rows
    }

    fn render_file_row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let entry = &self.files[ix];
        let path = entry.path.clone();
        let revert_path = entry.path.clone();
        let label_path = entry.path.clone();
        let additions = entry.additions;
        let deletions = entry.deletions;
        let active = self.selected.as_deref() == Some(path.as_str());

        h_flex()
            .id(("review-file", ix))
            .mx_2()
            .px_2()
            .py_1()
            .gap_2()
            .rounded(cx.theme().radius)
            .when(active, |this| this.bg(cx.theme().accent))
            .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.selected = Some(path.clone());
                cx.notify();
            }))
            .child(
                div()
                    .text_sm()
                    .flex_1()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .font_family("monospace")
                    .child(label_path),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().success)
                    .child(format!("+{additions}")),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().danger)
                    .child(format!("-{deletions}")),
            )
            .child(
                Button::new(("revert", ix))
                    .ghost()
                    .xsmall()
                    .label("撤销")
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(ReviewEvent::Revert(revert_path.clone()));
                    })),
            )
            .into_any_element()
    }
}

impl Render for ReviewPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (adds, dels) = self.totals();
        let selected_diff = self
            .selected
            .as_ref()
            .and_then(|path| self.files.iter().find(|e| &e.path == path))
            .map(|e| e.diff.clone());

        let mut rows = Vec::with_capacity(self.files.len());
        for ix in 0..self.files.len() {
            rows.push(self.render_file_row(ix, cx));
        }

        v_flex()
            .size_full()
            .border_l_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(div().text_sm().font_semibold().child("文件变更"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("{} 个文件 · +{adds} -{dels}", self.files.len())),
                    ),
            )
            .child(
                v_flex()
                    .py_1()
                    .children(rows)
                    .when(self.files.is_empty(), |this| {
                        this.child(
                            div()
                                .px_3()
                                .py_2()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("暂无变更。agent 修改文件后会出现在这里。"),
                        )
                    }),
            )
            .child(
                div()
                    .id("diff-view")
                    .flex_1()
                    .m_2()
                    .p_2()
                    .overflow_y_scroll()
                    .rounded(cx.theme().radius)
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().background)
                    .text_xs()
                    .font_family("monospace")
                    .children(
                        selected_diff
                            .map(|diff| Self::render_diff(&diff, cx))
                            .unwrap_or_default(),
                    ),
            )
    }
}
