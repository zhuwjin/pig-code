//! 右侧面板「子代理」tab：后台子代理完整对话的只读视图（A3b）。
//! 数据流：通知卡点击 → Op::LoadSubagent → core 读
//! {sessions_dir}/{session_id}.agents/{agent_id}.jsonl → Event::SubagentHistory
//! → set_history 填内容。加载到达前显示「加载中…」。

use gpui_kit::assets::IconName as AssetsIconName;
use gpui_kit::component::text::{TextView, TextViewState};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use pig_protocol::SubagentItem;

/// 单行展示项；tool 行带展开态与输出视口滚动句柄，assistant 行带 markdown 状态
struct SubagentRow {
    item: SubagentItem,
    /// assistant 行的 markdown 渲染状态（其它行 None）
    markdown: Option<Entity<TextViewState>>,
    /// tool 行展开态
    expanded: bool,
    /// tool 行展开输出的滚动句柄（track_scroll 持久滚动位置）
    output_scroll: ScrollHandle,
}

pub struct SubagentPanel {
    session_id: String,
    /// tab/头部标题（通知卡 description；SubagentHistory 到达后以 meta 为准）
    title: String,
    /// "{provider} · {model}"（加载后填）
    subtitle: String,
    /// None = 加载中
    rows: Option<Vec<SubagentRow>>,
    scroll: ScrollHandle,
}

impl SubagentPanel {
    pub fn new(session_id: String, title: String) -> Self {
        Self {
            session_id,
            title,
            subtitle: String::new(),
            rows: None,
            scroll: ScrollHandle::new(),
        }
    }

    /// 事件路由的会话归属校验（其它会话的迟到事件忽略）
    pub fn matches_session(&self, session_id: &str) -> bool {
        self.session_id == session_id
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    /// SubagentHistory 到达：assistant 行建 markdown 渲染状态（一次性 set_text，
    /// 参考 thread_view 的 Markdown 段用法）
    pub fn set_history(
        &mut self,
        title: String,
        subtitle: String,
        items: Vec<SubagentItem>,
        cx: &mut Context<Self>,
    ) {
        self.title = title;
        self.subtitle = subtitle;
        self.rows = Some(
            items
                .into_iter()
                .map(|item| {
                    let markdown = (item.role == "assistant").then(|| {
                        let state = cx.new(|cx| TextViewState::markdown("", cx));
                        let text = item.text.clone();
                        state.update(cx, |state, cx| state.set_text(&text, cx));
                        state
                    });
                    SubagentRow {
                        item,
                        markdown,
                        expanded: false,
                        output_scroll: ScrollHandle::new(),
                    }
                })
                .collect(),
        );
        cx.notify();
    }

    /// 自测用：(标题, 已加载 items 数)；未加载为 None
    pub fn debug_state(&self) -> Option<(String, usize)> {
        self.rows
            .as_ref()
            .map(|rows| (self.title.clone(), rows.len()))
    }

    /// tool 行：图标 + 工具名 + 摘要（单行省略），点击展开/收起输出卡
    fn render_tool_row(&self, ix: usize, row: &SubagentRow, cx: &mut Context<Self>) -> AnyElement {
        let subtle = cx.theme().muted_foreground;
        let subtlest = subtle.opacity(0.6);
        let summary = row
            .item
            .text
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        v_flex()
            .w_full()
            .child(
                h_flex()
                    .id(("subagent-tool", ix))
                    .w_full()
                    .gap_2()
                    .py_1()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(rows) = &mut this.rows
                            && let Some(row) = rows.get_mut(ix)
                        {
                            row.expanded = !row.expanded;
                        }
                        cx.notify();
                    }))
                    .child(
                        Icon::new(AssetsIconName::Wrench)
                            .size_3p5()
                            .text_color(subtlest),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_sm()
                            .text_color(subtlest)
                            .child(row.item.tool.clone().unwrap_or_else(|| "工具".to_string())),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_sm()
                            .text_color(subtle)
                            .child(summary),
                    )
                    .child(
                        Icon::new(if row.expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .size_3p5()
                        .text_color(subtlest),
                    ),
            )
            // 展开输出：等宽限高卡（thread_view 工具卡展开区的简化版）
            .when(row.expanded, |this| {
                this.when_some(row.item.output.clone(), |this, output| {
                    this.child(
                        div()
                            .id(("subagent-tool-body", ix))
                            .w_full()
                            .max_h(px(120.))
                            .overflow_y_scroll()
                            .track_scroll(&row.output_scroll)
                            .rounded_lg()
                            .border_1()
                            .border_color(cx.theme().border)
                            .bg(cx.theme().group_box)
                            .px_3()
                            .py_2()
                            .text_xs()
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_color(subtle)
                            .child(output),
                    )
                })
            })
            .into_any_element()
    }
}

impl Render for SubagentPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let subtle = cx.theme().muted_foreground;
        let Some(rows) = &self.rows else {
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .child(div().text_sm().text_color(subtle).child("加载中…"))
                .into_any_element();
        };
        div()
            .id("subagent-panel-scroll")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .child(
                v_flex()
                    .w_full()
                    .gap_3()
                    .p_3()
                    // 头部：标题 + 模型副标题
                    .child(
                        v_flex()
                            .w_full()
                            .gap_0p5()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(self.title.clone()),
                            )
                            .when(!self.subtitle.is_empty(), |this| {
                                this.child(
                                    div()
                                        .text_xs()
                                        .text_color(subtle)
                                        .child(self.subtitle.clone()),
                                )
                            }),
                    )
                    .children(rows.iter().enumerate().map(|(ix, row)| {
                        match row.item.role.as_str() {
                            // user 行：「任务」小字标签 + 正文
                            "user" => v_flex()
                                .w_full()
                                .gap_1()
                                .child(div().text_xs().text_color(subtle).child("任务"))
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(cx.theme().foreground)
                                        .child(row.item.text.clone()),
                                )
                                .into_any_element(),
                            // tool 行：可展开
                            "tool" => self.render_tool_row(ix, row, cx),
                            // assistant 行：markdown 渲染
                            _ => match &row.markdown {
                                Some(state) => TextView::new(state)
                                    .selectable(true)
                                    .text_sm()
                                    .into_any_element(),
                                None => div()
                                    .text_sm()
                                    .text_color(cx.theme().foreground)
                                    .child(row.item.text.clone())
                                    .into_any_element(),
                            },
                        }
                    })),
            )
            .into_any_element()
    }
}
