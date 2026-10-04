//! 模型调用轨迹面板：读当前会话的 `{session_id}.model-io.jsonl`（pig-core 每次
//! 主会话步骤落盘一条；UI 直读文件不走协议）。布局对齐 ZCode「模型调用轨迹」：
//! 按调用分组（序号 / 来源 / 结束原因胶囊 / IN·OUT token / 耗时 / 时刻），
//! 每次调用下「输入」「输出」两个卡片；卡片内逐消息一行（彩色角色标签 +
//! 单行预览 + chevron），各行独立展开看全文，展开行头部带耗时、时间戳与
//! 复制按钮。

use std::collections::HashSet;

use super::*;

/// 面板状态（AppView.trajectory，None = 未打开）
pub(crate) struct TrajectoryState {
    pub(crate) records: Vec<pig_core::model_io::ModelIoRecord>,
    /// 文件读取失败（记录为空时展示）
    pub(crate) error: Option<String>,
    /// 展开的消息行 key（"{turn}:{row_ix}"；turn 唯一标识一次调用，刷新后
    /// 展开态可保留）
    pub(crate) expanded: HashSet<String>,
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
                expanded: HashSet::new(),
            };
        }
        Self {
            records: pig_core::model_io::read_all(&path),
            error: None,
            expanded: HashSet::new(),
        }
    }
}

/// 行的视觉角色：决定标签文案与颜色（对齐 ZCode trajectoryRoleTextClass 六种）
#[derive(Clone, Copy, PartialEq, Eq)]
enum VisualRole {
    System,
    User,
    Assistant,
    Reasoning,
    ToolCall,
    ToolResult,
}

impl VisualRole {
    fn label(self) -> &'static str {
        match self {
            Self::System => "系统提示词",
            Self::User => "用户消息",
            Self::Assistant => "助手消息",
            Self::Reasoning => "思考过程",
            Self::ToolCall => "工具调用",
            Self::ToolResult => "工具结果",
        }
    }

    /// 标签颜色（ZCode 深/浅两套 hex + 80% 不透明度；system 用主题灰）
    fn color(self, cx: &App) -> Hsla {
        let dark = cx.theme().is_dark();
        let hex = match self {
            Self::System => return cx.theme().muted_foreground,
            Self::User => {
                if dark {
                    0x60a5fa
                } else {
                    0x2563eb
                }
            }
            Self::Assistant => {
                if dark {
                    0x2dd4bf
                } else {
                    0x0f766e
                }
            }
            Self::Reasoning => {
                if dark {
                    0xa78bfa
                } else {
                    0x7c3aed
                }
            }
            Self::ToolCall => {
                if dark {
                    0xf59e0b
                } else {
                    0xd97706
                }
            }
            Self::ToolResult => {
                if dark {
                    0x38bdf8
                } else {
                    0x0284c7
                }
            }
        };
        let color: Hsla = rgb(hex).into();
        color.opacity(0.8)
    }
}

/// 输入消息 → 视觉角色（tool 归「工具结果」，未知 role 按 system 灰处理）
fn input_role(msg: &pig_core::model_io::ModelIoMessage) -> VisualRole {
    match msg.role.as_str() {
        "system" => VisualRole::System,
        "user" => VisualRole::User,
        "assistant" => VisualRole::Assistant,
        "tool" => VisualRole::ToolResult,
        _ => VisualRole::System,
    }
}

/// 调用来源标签（source 字段预留了 subagent/compact 等扩展）
fn source_label(source: &str) -> &str {
    match source {
        "main" => "主会话",
        "subagent" => "子代理",
        "compact" => "上下文压缩",
        other => other,
    }
}

/// 千分位数字（对齐 ZCode toLocaleString：48,442）
fn fmt_num(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (ix, ch) in digits.chars().enumerate() {
        if ix > 0 && (digits.len() - ix).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// 耗时（ZCode formatTrajectoryDuration：<1s 毫秒；<10s 两位小数秒；否则一位）
fn fmt_duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 10_000 {
        format!("{:.2}s", ms as f64 / 1000.0)
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

/// 毫秒时间戳 → 本地时间（local-offset 失败返回 None，调用方降级为空串）
fn local_dt(ts_ms: u64) -> Option<time::OffsetDateTime> {
    let utc = time::OffsetDateTime::from_unix_timestamp_nanos(ts_ms as i128 * 1_000_000).ok()?;
    let offset = time::UtcOffset::current_local_offset().ok()?;
    Some(utc.to_offset(offset))
}

/// 12 小时制与 AM/PM（0 点 → 12 AM，12 点 → 12 PM）
fn hour12(hour: u8) -> (u8, &'static str) {
    let ampm = if hour < 12 { "AM" } else { "PM" };
    (
        match hour % 12 {
            0 => 12,
            h => h,
        },
        ampm,
    )
}

/// 时刻（对齐 ZCode formatTrajectoryClockTime：02:41:48 PM）
fn fmt_clock(ts_ms: u64) -> String {
    let Some(dt) = local_dt(ts_ms) else {
        return String::new();
    };
    let (h, ampm) = hour12(dt.hour());
    format!("{h:02}:{:02}:{:02} {ampm}", dt.minute(), dt.second())
}

/// 展开行时间戳（对齐 ZCode formatTrajectoryDateTime：10/4/2026, 2:42:08 PM）
fn fmt_datetime(ts_ms: u64) -> String {
    let Some(dt) = local_dt(ts_ms) else {
        return String::new();
    };
    let (h, ampm) = hour12(dt.hour());
    format!(
        "{}/{}/{}, {}:{:02}:{:02} {ampm}",
        u8::from(dt.month()),
        dt.day(),
        dt.year(),
        h,
        dt.minute(),
        dt.second()
    )
}

/// 单行预览：所有空白（含换行）折叠为一个空格（ZCode messagePreview 同款）
fn preview(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
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

/// 消息行渲染参数（rec_ix + row_ix 组元素 id；key 为展开态键）
struct RowSpec {
    rec_ix: usize,
    /// 卡片内行序（「输入」「输出」连续编号，兼作斑马纹奇偶）
    row_ix: usize,
    key: String,
    role: VisualRole,
    preview: String,
    full: String,
    duration_ms: u64,
    ts_ms: u64,
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

    /// 右侧面板「调用轨迹」tab 内容：汇总副标题 + 按时间正序的调用卡片列表
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
        // 副标题：调用数 · 总 token（IN 含缓存命中 + OUT）· 出现过的模型（保序去重）
        let calls = state.records.len();
        let (in_total, out_total) = state.records.iter().fold((0u64, 0u64), |(i, o), r| {
            (i + r.usage.input + r.usage.cache_read, o + r.usage.output)
        });
        let mut models: Vec<&str> = Vec::new();
        for record in &state.records {
            if !record.model.is_empty() && !models.contains(&record.model.as_str()) {
                models.push(record.model.as_str());
            }
        }
        let summary = if models.is_empty() {
            format!("{calls} 次调用 · {} tok", fmt_num(in_total + out_total))
        } else {
            format!(
                "{calls} 次调用 · {} tok · {}",
                fmt_num(in_total + out_total),
                models.join(", ")
            )
        };

        v_flex()
            .id("trajectory-panel")
            .size_full()
            .overflow_y_scroll()
            .gap_3()
            .p_3()
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(summary),
                    )
                    .child(div().flex_1())
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
                let mut cards = Vec::new();
                let mut prev_len = 0usize;
                for (rec_ix, record) in state.records.iter().enumerate() {
                    let full = &record.input;
                    let display: Vec<_> = if rec_ix == 0 || full.len() < prev_len {
                        full.clone()
                    } else {
                        full[prev_len..]
                            .iter()
                            .filter(|m| m.role != "assistant")
                            .cloned()
                            .collect()
                    };
                    prev_len = full.len();
                    cards.push(self.render_call_card(rec_ix, record, &display, cx));
                }
                v_flex().w_full().gap_4().children(cards).into_any_element()
            })
            .into_any_element()
    }

    /// 单次调用卡片：分组头（序号/来源/结束原因/IN·OUT·耗时·时刻）+
    /// 「输入」「输出」两个 section 卡片 + 错误块；display_input 为展示层增量输入
    fn render_call_card(
        &self,
        rec_ix: usize,
        record: &pig_core::model_io::ModelIoRecord,
        display_input: &[pig_core::model_io::ModelIoMessage],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // 主题取值先物化为本地变量（Hsla Copy / 字体 clone），后续闭包要 &mut cx
        let (muted, danger, border, accent, mono) = {
            let theme = cx.theme();
            (
                theme.muted_foreground,
                theme.danger,
                theme.border,
                theme.accent,
                theme.mono_font_family.clone(),
            )
        };
        let (finish_label, finish_color) = match record.finish.as_str() {
            "stop" => ("正常结束", muted),
            "tool_calls" => ("工具调用", muted),
            "cancelled" => ("已取消", muted),
            _ => ("失败", danger),
        };
        // 输入消息行
        let mut input_rows: Vec<AnyElement> = Vec::new();
        let mut row_ix = 0usize;
        for msg in display_input {
            let mut full = msg.content.clone().unwrap_or_default();
            if !msg.tool_calls.is_empty() {
                if !full.is_empty() {
                    full.push('\n');
                }
                full.push_str(&format!("[调用工具: {}]", msg.tool_calls.join(", ")));
            }
            if msg.images > 0 {
                if !full.is_empty() {
                    full.push('\n');
                }
                full.push_str(&format!("[图片 x{}]", msg.images));
            }
            if full.is_empty() {
                full = "—".to_string();
            }
            let row_preview = preview(&full);
            input_rows.push(self.render_msg_row(
                RowSpec {
                    rec_ix,
                    row_ix,
                    key: format!("{}:{row_ix}", record.turn),
                    role: input_role(msg),
                    preview: row_preview,
                    full,
                    duration_ms: record.duration_ms,
                    ts_ms: record.ts_ms,
                },
                cx,
            ));
            row_ix += 1;
        }
        // 输出消息行：思考过程 → 助手消息 → 各工具调用（行序与输入连续）
        let mut output_rows: Vec<AnyElement> = Vec::new();
        let mut push_output = |role: VisualRole, full: String, rows: &mut Vec<AnyElement>| {
            let row_preview = preview(&full);
            rows.push(self.render_msg_row(
                RowSpec {
                    rec_ix,
                    row_ix,
                    key: format!("{}:{row_ix}", record.turn),
                    role,
                    preview: row_preview,
                    full,
                    duration_ms: record.duration_ms,
                    ts_ms: record.ts_ms,
                },
                cx,
            ));
            row_ix += 1;
        };
        if !record.reasoning.is_empty() {
            push_output(
                VisualRole::Reasoning,
                record.reasoning.clone(),
                &mut output_rows,
            );
        }
        if !record.text.is_empty() {
            push_output(VisualRole::Assistant, record.text.clone(), &mut output_rows);
        }
        for call in &record.tool_calls {
            push_output(
                VisualRole::ToolCall,
                format!("{}\n{}", call.name, call.arguments),
                &mut output_rows,
            );
        }

        v_flex()
            .w_full()
            .gap_2()
            .child(
                // 分组头（ZCode CallMetadata：IN n · OUT n · 时长 · 时刻）
                h_flex()
                    .w_full()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .w(px(20.))
                            .flex_shrink_0()
                            .text_xs()
                            .font_family(mono.clone())
                            .text_color(muted)
                            .child(format!("{:02}", rec_ix + 1)),
                    )
                    .child(
                        div()
                            .text_sm()
                            .font_medium()
                            .child(source_label(&record.source).to_string()),
                    )
                    .child(
                        div()
                            .px_2()
                            .rounded_full()
                            .border_1()
                            .border_color(border)
                            .bg(accent.opacity(0.4))
                            .text_xs()
                            .text_color(finish_color)
                            .child(finish_label),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_xs()
                            .font_family(mono)
                            .text_color(muted)
                            .child(format!(
                                "IN {} · OUT {} · {} · {}",
                                fmt_num(record.usage.input + record.usage.cache_read),
                                fmt_num(record.usage.output),
                                fmt_duration(record.duration_ms),
                                fmt_clock(record.ts_ms)
                            )),
                    ),
            )
            .when(!input_rows.is_empty(), |this| {
                this.child(section_card("输入", input_rows, cx))
            })
            .when(!output_rows.is_empty(), |this| {
                this.child(section_card("输出", output_rows, cx))
            })
            .when_some(record.error.clone(), |this, error| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(danger)
                        .child(format!("错误：{}", clip(&error, 2000))),
                )
            })
            .into_any_element()
    }

    /// 单条消息行（输入/输出卡片共用）：折叠 = 彩色角色标签 + 单行预览 +
    /// chevron；展开 = 标签 + 耗时·时间戳 + 复制按钮 + 完整内容
    fn render_msg_row(&self, spec: RowSpec, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let mono = theme.mono_font_family.clone();
        let zebra_bg = theme.accent.opacity(0.3);
        let hover_bg = theme.accent;
        let role_color = spec.role.color(cx);
        let open = self
            .trajectory
            .as_ref()
            .is_some_and(|s| s.expanded.contains(&spec.key));
        let row_id = spec.rec_ix * 4096 + spec.row_ix;
        let key = spec.key.clone();
        let zebra = spec.row_ix % 2 == 1;

        let chevron = Icon::new(if open {
            IconName::ChevronDown
        } else {
            IconName::ChevronRight
        })
        .size_3()
        .text_color(muted);

        let header = h_flex()
            .id(("traj-row", row_id))
            .w_full()
            .min_h(px(32.))
            .pl_3()
            .pr_2()
            .py_1()
            .items_center()
            .gap_2()
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                if let Some(state) = &mut this.trajectory
                    && !state.expanded.remove(&key)
                {
                    state.expanded.insert(key.clone());
                }
                cx.notify();
            }))
            .child(
                div()
                    .w(px(72.))
                    .flex_shrink_0()
                    .text_xs()
                    .font_family(mono.clone())
                    .text_color(role_color)
                    .child(spec.role.label()),
            );

        if open {
            let copy_text = spec.full.clone();
            let copy_btn = div()
                .id(("traj-copy", row_id))
                .cursor_pointer()
                .p_1()
                .rounded_sm()
                .hover(move |d| d.bg(hover_bg))
                .on_click(cx.listener(move |_, _, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copy_text.clone()));
                    cx.stop_propagation();
                }))
                .child(Icon::new(IconName::Copy).size_3().text_color(muted));
            v_flex()
                .w_full()
                .when(zebra, |d| d.bg(zebra_bg))
                .child(
                    header
                        .child(div().flex_1())
                        .child(
                            div()
                                .text_xs()
                                .font_family(mono)
                                .text_color(muted)
                                .child(format!(
                                    "{} · {}",
                                    fmt_duration(spec.duration_ms),
                                    fmt_datetime(spec.ts_ms)
                                )),
                        )
                        .child(copy_btn)
                        .child(chevron),
                )
                .child(
                    div()
                        .w_full()
                        .px_3()
                        .pb_2()
                        .text_xs()
                        .text_color(cx.theme().foreground)
                        .child(clip(&spec.full, 2000)),
                )
                .into_any_element()
        } else {
            header
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .font_family(mono)
                        .text_color(muted)
                        .child(spec.preview),
                )
                .child(chevron)
                .when(zebra, |d| d.bg(zebra_bg))
                .into_any_element()
        }
    }
}

/// 「输入」/「输出」section 卡片：标题条 + 行间细分隔线的消息行列表
fn section_card(title: &str, rows: Vec<AnyElement>, cx: &mut Context<AppView>) -> AnyElement {
    let theme = cx.theme();
    let mut card = v_flex()
        .w_full()
        .rounded_lg()
        .border_1()
        .border_color(theme.border)
        .overflow_hidden()
        .child(
            div()
                .h(px(32.))
                .w_full()
                .px_3()
                .flex()
                .items_center()
                .bg(theme.accent.opacity(0.4))
                .text_xs()
                .font_medium()
                .text_color(theme.foreground)
                .child(title.to_string()),
        );
    for row in rows {
        card = card
            .child(div().h(px(1.)).w_full().bg(theme.border.opacity(0.5)))
            .child(row);
    }
    card.into_any_element()
}
