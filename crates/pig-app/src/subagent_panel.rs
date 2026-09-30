//! 右侧面板「子代理」tab：后台/前台子代理完整对话的只读视图（A3b 起）。
//! 数据流：通知卡/代理卡点击 → Op::LoadSubagent → core 读
//! {sessions_dir}/{session_id}.agents/{agent_id}.jsonl → Event::SubagentHistory
//! → set_history 填内容。加载到达前显示「加载中…」。
//! A3d 起支持实时：core 每个 step 把新增消息投影为 SubagentActivity 逐条推来，
//! 面板追加（running 期间底部有「运行中」指示，finished 后消失）；
//! 增量与全量之间的窄竞态由 finished 后的全量重拉收口（AppView 侧发起）。

use gpui_kit::assets::IconName as AssetsIconName;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::text::{TextView, TextViewState};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
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
    /// 子代理仍在运行（SubagentHistory 的注册表口径初始化，SubagentActivity 增删）
    running: bool,
    /// 全量加载到达前收到的活动项（乱序缓冲；set_history 时拼在尾部）
    pending: Vec<SubagentItem>,
    /// 累计收到的活动项数（自测断言用）
    activity_items: usize,
    /// 跟随模式（thread_view 同款）：追加/加载时自动贴底；用户上翻暂停
    ///（浮出「最新消息」按钮），回到底部（任意方式）或点按钮恢复
    following: bool,
    scroll: ScrollHandle,
}

/// 展示项 → 行：assistant 行建 markdown 渲染状态（一次性 set_text，
/// 参考 thread_view 的 Markdown 段用法）。初始全量与实时追加共用。
fn build_row(item: SubagentItem, cx: &mut Context<SubagentPanel>) -> SubagentRow {
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
}

impl SubagentPanel {
    pub fn new(session_id: String, title: String) -> Self {
        Self {
            session_id,
            title,
            subtitle: String::new(),
            rows: None,
            running: false,
            pending: vec![],
            activity_items: 0,
            following: true,
            scroll: ScrollHandle::new(),
        }
    }

    /// 贴底判定（thread_view 同款）：gpui 滚动偏移是负值（顶部 0 → 底部
    /// -max_offset），at_bottom ⟺ offset + max_offset ≈ 0
    fn at_bottom(&self) -> bool {
        self.scroll.offset().y + self.scroll.max_offset().y <= px(2.)
    }

    /// 事件路由的会话归属校验（其它会话的迟到事件忽略）
    pub fn matches_session(&self, session_id: &str) -> bool {
        self.session_id == session_id
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    /// SubagentHistory 到达：全量填充 + 拼上先到的活动缓冲
    pub fn set_history(
        &mut self,
        title: String,
        subtitle: String,
        items: Vec<SubagentItem>,
        running: bool,
        cx: &mut Context<Self>,
    ) {
        self.title = title;
        self.subtitle = subtitle;
        self.running = running;
        let mut all = items;
        all.append(&mut self.pending);
        self.rows = Some(all.into_iter().map(|item| build_row(item, cx)).collect());
        // 跟随时贴底（初始加载必然 following=true——打开即读最新）；
        // scroll_to_bottom 是延迟标记，下一帧布局后才落到真底部
        if self.following {
            self.scroll.scroll_to_bottom();
        }
        cx.notify();
    }

    /// SubagentActivity 增量追加；跟随时贴底（用户上翻后不打扰）
    pub fn push_item(&mut self, item: SubagentItem, cx: &mut Context<Self>) {
        self.activity_items += 1;
        match &mut self.rows {
            // 全量尚未到达：缓冲（set_history 拼尾），等 finished 后全量重拉收口
            None => self.pending.push(item),
            Some(rows) => {
                rows.push(build_row(item, cx));
                if self.following {
                    self.scroll.scroll_to_bottom();
                }
            }
        }
        cx.notify();
    }

    /// 子代理结束（含取消/被杀）：关「运行中」指示
    pub fn set_finished(&mut self, cx: &mut Context<Self>) {
        self.running = false;
        cx.notify();
    }

    /// 自测用：(标题, 已加载 items 数)；未加载为 None
    pub fn debug_state(&self) -> Option<(String, usize)> {
        self.rows
            .as_ref()
            .map(|rows| (self.title.clone(), rows.len()))
    }

    /// 自测用：(running, 已加载行数（含缓冲）, 累计活动项数)
    pub fn debug_live(&self) -> (bool, usize, usize) {
        (
            self.running,
            self.rows.as_ref().map(|r| r.len()).unwrap_or(0) + self.pending.len(),
            self.activity_items,
        )
    }

    /// 自测用：(following, at_bottom)——跟随态与贴底判定
    pub fn debug_scroll(&self) -> (bool, bool) {
        (self.following, self.at_bottom())
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
        // 用户回到底部（滚轮/拖滚动条任意方式）自动恢复跟随（thread_view 同款）
        if !self.following && self.at_bottom() {
            self.following = true;
        }
        let body: AnyElement = match &self.rows {
            None => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .child(div().text_sm().text_color(subtle).child("加载中…"))
                .into_any_element(),
            Some(rows) => v_flex()
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
                }))
                // 底部「运行中」指示（子代理结束时随 finished 消失）
                .when(self.running, |this| {
                    this.child(
                        h_flex()
                            .gap_2()
                            .child(Spinner::new().small().color(subtle))
                            .child(div().text_xs().text_color(subtle).child("子代理运行中…")),
                    )
                })
                .into_any_element(),
        };
        div()
            .relative()
            .size_full()
            .child(
                div()
                    .id("subagent-panel-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, window, cx| {
                        let delta = event.delta.pixel_delta(window.line_height());
                        // 用户上翻：暂停跟随并浮出「最新消息」按钮（不吞这次事件，
                        // 列表照常滚动）。内容没超高（不可滚动）时上翻不暂停——
                        // 否则短面板里滚一下也会浮出按钮
                        if delta.y > px(0.)
                            && this.following
                            && this.scroll.max_offset().y > px(0.)
                        {
                            this.following = false;
                            cx.notify();
                        }
                        // 能滚时吞掉滚轮，不穿透到面板外的三栏/主消息流
                        if this.scroll.max_offset().y > px(0.) {
                            cx.stop_propagation();
                        }
                    }))
                    .child(body),
            )
            // 未跟随时浮出「最新消息」按钮：点击回到底部并恢复跟随
            .when(!self.following, |this| {
                this.child(
                    div().absolute().bottom_3().right_3().child(
                        h_flex()
                            .id("subagent-latest-fab")
                            .items_center()
                            .gap_1()
                            .px_3()
                            .py_1p5()
                            .rounded_full()
                            .bg(cx.theme().popover)
                            .border_1()
                            .border_color(cx.theme().border)
                            .shadow_md()
                            .cursor_pointer()
                            // 默认 hitbox 不拦下层：点击会穿透到面板内容——
                            // 挡掉穿透，滚轮仍透传给列表
                            .block_mouse_except_scroll()
                            .child(
                                Icon::new(AssetsIconName::ArrowDown)
                                    .size_3p5()
                                    .text_color(cx.theme().foreground),
                            )
                            .child(div().text_xs().child("最新消息"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.following = true;
                                this.scroll.scroll_to_bottom();
                                cx.notify();
                            })),
                    ),
                )
            })
            .into_any_element()
    }
}
