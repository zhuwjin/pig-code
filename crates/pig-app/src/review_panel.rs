use gpui_kit::assets::IconName as AssetsIconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use pig_protocol::GitFileChange;

pub struct FileChangeEntry {
    pub path: String,
    pub diff: String,
    pub additions: u32,
    pub deletions: u32,
}

/// 面板数据源：git 工作区口径（未暂存 / 已暂存）。
/// 会话改动面板已移到消息流每轮 turn 末尾（thread_view 的 TurnChanges 段）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ReviewSource {
    Unstaged,
    Staged,
}

impl ReviewSource {
    fn label(self) -> &'static str {
        match self {
            Self::Unstaged => "未暂存",
            Self::Staged => "已暂存",
        }
    }
}

#[derive(Clone)]
pub enum ReviewEvent {
    /// 请求刷新工作区 git 状态（面板切源或点刷新时发出，AppView 转发 Op::GitStatus）
    RefreshGit,
    /// 打开某 git 文件的 diff（AppView 转发 Op::GitDiff）
    OpenGitDiff { path: String, staged: bool },
}

impl EventEmitter<ReviewEvent> for ReviewPanel {}

pub struct ReviewPanel {
    /// 会话改动快照（隐藏状态：侧栏会话 +N/-N 徽章与自测用，UI 不展示）
    files: Vec<FileChangeEntry>,
    diff_scroll: ScrollHandle,
    source: ReviewSource,
    is_git: bool,
    git_unstaged: Vec<GitFileChange>,
    git_staged: Vec<GitFileChange>,
    git_selected: Option<String>,
    /// 当前打开的 git diff (path, 原文)
    git_diff: Option<(String, String)>,
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
        // 净额归零（改回到原始内容）：从列表移除，与 ZCode/kimi-code 口径一致
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

    /// 工作区 git 状态到达（Op::GitStatus 的回应）
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
        // 选中项消失时清空 diff 视图
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

    /// 单文件 git diff 到达
    pub fn set_git_diff(&mut self, path: String, diff: String, cx: &mut Context<Self>) {
        if self.git_selected.as_deref() == Some(path.as_str()) {
            self.git_diff = Some((path, diff));
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

    /// 逐行渲染 unified diff：旧/新行号两列 + 增删行淡底色 + hunk 头底色。
    /// 行号列宽按本 diff 的最大行号位数自适应（纯新增/纯删除文件的空列收成
    /// 窄缝），不再固定 36px 留出大块空白；两列 flex_shrink_0 定宽防长行挤压。
    /// 逐 hunk 接受/拒绝留 M6+。
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

        // 列宽按最大行号位数自适应；该列无行号（纯新增/纯删除）时收成窄缝
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
                    // 行号列定宽不收缩：长行溢出时 flex 收缩会把行号列压窄，
                    // 各行行号错位（有的靠前有的靠中）
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

    /// 数据源切换 chip（未暂存 / 已暂存）
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

    /// git 改动行：状态字母 + 相对路径 + +N/-N（点击拉取 git diff）
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
                // 换文件从头看起
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
    /// 列表视图：数据源 tab + 刷新 + 汇总，下面整片文件列表
    fn render_list_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let summary = if !self.is_git {
            "非 git 仓库".to_string()
        } else {
            let (n, adds, dels) = self.git_totals();
            format!("{n} 个文件 · +{adds} -{dels}")
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
                            .label("刷新")
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
                                .child("当前工作区不是 git 仓库，git 改动不可用。"),
                        )
                    })
                    .when(self.is_git && self.git_list().is_empty(), |this| {
                        this.child(
                            div()
                                .px_3()
                                .py_2()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("工作区干净，没有改动。"),
                        )
                    }),
            )
            .into_any_element()
    }

    /// diff 视图（点开文件后整面板切换）：顶部返回栏（← 返回列表 + 头部截断的
    /// 路径 + 重新拉取）+ 全高 unified diff，不再与文件列表上下堆叠。
    fn render_diff_view(&self, path: String, cx: &mut Context<Self>) -> AnyElement {
        let diff_rows: Vec<AnyElement> = if self.git_diff_loading {
            vec![
                div()
                    .px_2()
                    .py_1()
                    .text_color(cx.theme().muted_foreground)
                    .child("加载 diff 中…")
                    .into_any_element(),
            ]
        } else {
            self.git_diff
                .as_ref()
                .map(|(_, diff)| Self::render_diff(diff, cx))
                .unwrap_or_default()
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

/// 路径头部截断：太长时保留尾部（文件名最相关），截断点落在路径段边界
fn shorten_path(path: &str, max_chars: usize) -> String {
    let len = path.chars().count();
    if len <= max_chars {
        return path.to_string();
    }
    let keep = max_chars.saturating_sub(2); // "…/" 占两字符
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
        // 点开文件：整面板切到 diff 视图；← 返回文件列表
        match self.git_selected.clone() {
            Some(path) => self.render_diff_view(path, cx),
            None => self.render_list_view(cx),
        }
    }
}
