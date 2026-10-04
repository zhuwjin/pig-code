//! 模型调用轨迹弹窗：读当前会话的 `{session_id}.model-io.jsonl`
//!（pig-core 每次主会话步骤落盘一条），列表 + 展开详情，
//! 对齐 ZCode「模型调用轨迹」面板的信息结构。

use super::*;

/// 弹窗状态（AppView.trajectory，None = 关闭）
pub(crate) struct TrajectoryState {
    pub(crate) records: Vec<pig_core::model_io::ModelIoRecord>,
    /// 文件读取失败（记录为空时展示）
    pub(crate) error: Option<String>,
    /// 当前展开的记录下标（按展示序 = 倒序后的下标）；None = 全收起
    pub(crate) expanded: Option<usize>,
}

impl TrajectoryState {
    /// 读当前数据目录下该会话的调用轨迹（文件缺失 = 空列表，非错误）
    pub(crate) fn load(session_id: &str) -> Self {
        let sessions_dir = pig_core::data_dir().join("sessions");
        let path = pig_core::model_io::model_io_path(&sessions_dir, session_id);
        if !path.exists() {
            return Self {
                records: vec![],
                error: None,
                expanded: None,
            };
        }
        Self {
            records: pig_core::model_io::read_all(&path),
            error: None,
            expanded: None,
        }
    }
}

/// 展示用文本截断（字符边界；轨迹里单条内容已截 4000，展示再收 2000）
fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let head: String = text.chars().take(max).collect();
        format!("{head}…（已截断）")
    }
}

/// 输入消息角色中文标签（对齐 ZCode：系统提示词/用户消息/助手消息/工具结果）
fn role_label(role: &str) -> &str {
    match role {
        "system" => "系统提示词",
        "user" => "用户消息",
        "assistant" => "助手消息",
        "tool" => "工具结果",
        other => other,
    }
}

fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

impl AppView {
    /// 按当前会话重读调用轨迹落盘记录（打开 tab / 切会话 / 回合完成 / 手动刷新）
    pub(crate) fn reload_trajectory(&mut self) {
        if let Some(session_id) = self.current.clone() {
            self.trajectory = Some(TrajectoryState::load(&session_id));
        }
    }

    /// 标题栏菜单入口：打开右侧「调用轨迹」tab（open_right_tab 内部会重读数据）
    pub(crate) fn open_trajectory(&mut self, cx: &mut Context<Self>) {
        self.open_right_tab(RightTab::Trajectory, cx);
    }

    /// 右侧面板「调用轨迹」tab 内容：汇总头 + 倒序记录列表（整页滚动），行展开详情
    pub(crate) fn render_trajectory_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(state) = &self.trajectory else {
            // 打开 tab 即会重读，这里兜底未打开会话的场景
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child("开始会话后，这里会显示模型调用记录")
                .into_any_element();
        };
        // 汇总：总调用数 + 总 token（输入含缓存命中，输出单列）
        let calls = state.records.len();
        let (in_total, out_total) = state.records.iter().fold((0u64, 0u64), |(i, o), r| {
            (i + r.usage.input + r.usage.cache_read, o + r.usage.output)
        });

        v_flex()
            .id("trajectory-panel")
            .size_full()
            .overflow_y_scroll()
            .gap_2()
            .p_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(div().text_sm().font_semibold().child("模型调用轨迹"))
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!(
                                "{calls} 次调用 · 输入 {} · 输出 {} tokens",
                                fmt_tokens(in_total),
                                fmt_tokens(out_total)
                            )),
                    )
                    .child(
                        Button::new("refresh-trajectory")
                            .ghost()
                            .small()
                            .icon(IconName::RotateCw)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.reload_trajectory();
                                cx.notify();
                            })),
                    ),
            )
            .child(if calls == 0 {
                h_flex()
                    .items_center()
                    .gap_2()
                    .py_6()
                    .text_color(cx.theme().muted_foreground)
                    .child(Icon::new(IconName::Inbox).size_5())
                    .child(div().text_sm().child(match &state.error {
                        Some(error) => format!("读取调用轨迹失败：{error}"),
                        None => "暂无模型调用记录（本会话还没有请求）".to_string(),
                    }))
                    .into_any_element()
            } else {
                // 展示层增量（对齐 ZCode 时间线）：首条展示完整起始上下文；后续
                // 条目只展示相对上一条新增的输入，且滤掉 assistant（回复已作为
                // 上一条的输出呈现）；上下文变短（compact 重置）时回退完整展示
                let mut rows: Vec<(
                    &pig_core::model_io::ModelIoRecord,
                    Vec<pig_core::model_io::ModelIoMessage>,
                )> = Vec::new();
                let mut prev_len = 0usize;
                for record in &state.records {
                    let full = &record.input;
                    let display: Vec<_> = if rows.is_empty() || full.len() < prev_len {
                        full.clone()
                    } else {
                        full[prev_len..]
                            .iter()
                            .filter(|m| m.role != "assistant")
                            .cloned()
                            .collect()
                    };
                    prev_len = full.len();
                    rows.push((record, display));
                }
                // 倒序：最新调用在最上
                v_flex()
                    .gap_2()
                    .children(
                        rows.into_iter()
                            .rev()
                            .enumerate()
                            .map(|(row_ix, (record, display))| {
                                self.render_trajectory_row(
                                    row_ix,
                                    calls - row_ix,
                                    record,
                                    &display,
                                    cx,
                                )
                            })
                            .collect::<Vec<_>>(),
                    )
                    .into_any_element()
            })
            .into_any_element()
    }

    /// 单条调用：折叠行（序号/来源/模型/结束原因/用量/耗时）+ 展开详情；
    /// display_input 为展示层增量输入（首条 = 完整起始上下文）
    fn render_trajectory_row(
        &self,
        row_ix: usize,
        index: usize,
        record: &pig_core::model_io::ModelIoRecord,
        display_input: &[pig_core::model_io::ModelIoMessage],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let expanded = self
            .trajectory
            .as_ref()
            .is_some_and(|s| s.expanded == Some(row_ix));
        let (finish_label, finish_color) = match record.finish.as_str() {
            "stop" => ("正常结束", cx.theme().success),
            "tool_calls" => ("工具调用", cx.theme().primary),
            "cancelled" => ("已取消", cx.theme().muted_foreground),
            _ => ("失败", cx.theme().danger),
        };

        v_flex()
            .gap_1()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().border)
            .px_3()
            .py_2()
            .child(
                h_flex()
                    .id(("trajectory-row", row_ix))
                    .gap_2()
                    .items_center()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(state) = &mut this.trajectory {
                            state.expanded = (state.expanded != Some(row_ix)).then_some(row_ix);
                        }
                        cx.notify();
                    }))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .w(px(28.))
                            .child(format!("#{index}")),
                    )
                    .child(
                        div()
                            .text_xs()
                            .px_1()
                            .rounded_sm()
                            .bg(cx.theme().accent)
                            .child("主会话"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .min_w_0()
                            .truncate()
                            .child(format!("{} · {}", record.provider, record.model)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .px_1()
                            .rounded_sm()
                            .bg(cx.theme().accent)
                            .text_color(finish_color)
                            .child(finish_label),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!(
                                "↑{} ↓{} · {}ms（首字 {}ms）",
                                fmt_tokens(record.usage.input + record.usage.cache_read),
                                fmt_tokens(record.usage.output),
                                record.duration_ms,
                                record.ttft_ms
                            )),
                    )
                    .child(
                        Icon::new(if expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .size_3()
                        .text_color(cx.theme().muted_foreground),
                    ),
            )
            .when(expanded, |this| {
                this.child(
                    v_flex()
                        .gap_2()
                        .pt_1()
                        .child(
                            v_flex()
                                .gap_1()
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(if index == 1 {
                                            format!(
                                                "输入消息（{} 条，长内容已截断）",
                                                display_input.len()
                                            )
                                        } else {
                                            format!(
                                                "新增输入（{} 条，长内容已截断）",
                                                display_input.len()
                                            )
                                        }),
                                )
                                .children(display_input.iter().map(|msg| {
                                    v_flex()
                                        .gap_0p5()
                                        .px_2()
                                        .py_1()
                                        .rounded(cx.theme().radius)
                                        .bg(cx.theme().accent)
                                        .child(
                                            h_flex()
                                                .gap_1()
                                                .child(
                                                    div()
                                                        .text_xs()
                                                        .font_medium()
                                                        .child(role_label(&msg.role).to_string()),
                                                )
                                                .when_some(msg.tool_call_id.clone(), |this, id| {
                                                    this.child(
                                                        div()
                                                            .text_xs()
                                                            .text_color(cx.theme().muted_foreground)
                                                            .child(format!("← {id}")),
                                                    )
                                                })
                                                .when(msg.images > 0, |this| {
                                                    this.child(
                                                        div()
                                                            .text_xs()
                                                            .text_color(cx.theme().muted_foreground)
                                                            .child(format!("🖼 {}", msg.images)),
                                                    )
                                                })
                                                .when(!msg.tool_calls.is_empty(), |this| {
                                                    this.child(
                                                        div()
                                                            .text_xs()
                                                            .text_color(cx.theme().muted_foreground)
                                                            .child(msg.tool_calls.join(", ")),
                                                    )
                                                }),
                                        )
                                        .when_some(msg.content.clone(), |this, content| {
                                            this.child(
                                                div()
                                                    .text_xs()
                                                    .text_color(cx.theme().muted_foreground)
                                                    .child(clip(&content, 2000)),
                                            )
                                        })
                                })),
                        )
                        .when_some(
                            (!record.reasoning.is_empty()).then(|| clip(&record.reasoning, 2000)),
                            |this, text| this.child(detail_block("思考过程", &text, cx)),
                        )
                        .when_some(
                            (!record.text.is_empty()).then(|| clip(&record.text, 2000)),
                            |this, text| this.child(detail_block("输出", &text, cx)),
                        )
                        .when(!record.tool_calls.is_empty(), |this| {
                            this.child(
                                v_flex()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(format!(
                                                "工具调用（{}）",
                                                record.tool_calls.len()
                                            )),
                                    )
                                    .children(record.tool_calls.iter().map(|call| {
                                        v_flex()
                                            .gap_0p5()
                                            .px_2()
                                            .py_1()
                                            .rounded(cx.theme().radius)
                                            .bg(cx.theme().accent)
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .font_medium()
                                                    .child(format!("{}（{}）", call.name, call.id)),
                                            )
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(cx.theme().muted_foreground)
                                                    .child(clip(&call.arguments, 2000)),
                                            )
                                    })),
                            )
                        })
                        .when_some(record.error.clone(), |this, error| {
                            this.child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().danger)
                                    .child(format!("错误：{}", clip(&error, 2000))),
                            )
                        }),
                )
            })
            .into_any_element()
    }
}

/// 详情文本块：小标题 + 等宽内容
fn detail_block(title: &str, text: &str, cx: &mut Context<AppView>) -> AnyElement {
    v_flex()
        .gap_1()
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(title.to_string()),
        )
        .child(
            div()
                .rounded(cx.theme().radius)
                .bg(cx.theme().accent)
                .px_2()
                .py_1()
                .text_xs()
                .font_family(cx.theme().mono_font_family.clone())
                .text_color(cx.theme().foreground)
                .child(text.to_string()),
        )
        .into_any_element()
}
