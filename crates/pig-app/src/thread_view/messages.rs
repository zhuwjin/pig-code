use super::*;

/// 导航条横条宽度/透明度的弹簧参数：近临界阻尼（ζ≈0.93），
/// 平滑收拢不拖尾、无明显过冲
const NAV_BAR_SPRING: SpringConfig = SpringConfig::new(260., 30., 1.);

/// 导航预览卡的进出场动画时长（悬停稳定 120ms 开卡不变，动画只管淡入淡出）
const NAV_CARD_ANIM_DUR: std::time::Duration = std::time::Duration::from_millis(160);

/// 压缩分隔条：分隔线 — 内容 — 分隔线（「正在压缩上下文」进行态与
/// 「上下文已压缩」完成态共用骨架，对标 ZCode 的上下文压缩分隔行）
pub(crate) fn render_compact_divider(content: AnyElement, cx: &App) -> AnyElement {
    let line = || div().flex_grow(1.).h(px(1.)).bg(cx.theme().border);
    h_flex()
        .w_full()
        .items_center()
        .gap_3()
        .py_2()
        .child(line())
        .child(content)
        .child(line())
        .into_any_element()
}

/// @提及分段：文本里出现的 @path（files 命中）切成 Mention，其余为 Text。
/// 长路径优先（防前缀互吃）；命中点后一个字符须是路径终止符（防 @a.rs2 误配 @a.rs）
#[derive(Debug, PartialEq)]
pub(crate) enum MentionSegment {
    Text(String),
    Mention(String),
}

/// 旧记录的「引用文件: a, b」后缀（core 曾拼进展示文本；新记录不再写入，
/// chip 已内联表达）：只在 files 非空且完整命中时剥除，手写同形文本不受影响
pub(crate) fn strip_reference_suffix(text: &str, files: &[String]) -> String {
    if files.is_empty() {
        return text.to_string();
    }
    let suffix = format!("\n\n引用文件: {}", files.join(", "));
    text.replacen(&suffix, "", 1)
}

pub(crate) fn split_mention_segments(text: &str, files: &[String]) -> Vec<MentionSegment> {
    let mut segments = vec![MentionSegment::Text(text.to_string())];
    let mut sorted: Vec<&String> = files.iter().collect();
    sorted.sort_by_key(|f| std::cmp::Reverse(f.len()));
    for file in sorted {
        let needle = format!("@{file}");
        let mut next = Vec::with_capacity(segments.len() + 1);
        for segment in segments {
            let MentionSegment::Text(text) = segment else {
                next.push(segment);
                continue;
            };
            let mut rest = text.as_str();
            loop {
                let Some(pos) = rest.find(&needle) else {
                    next.push(MentionSegment::Text(rest.to_string()));
                    break;
                };
                let boundary = rest.as_bytes().get(pos + needle.len()).is_none_or(|b| {
                    !matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'/' | b'.' | b'-')
                });
                if !boundary {
                    let skip = pos + 1;
                    next.push(MentionSegment::Text(rest[..skip].to_string()));
                    rest = &rest[skip..];
                    continue;
                }
                if pos > 0 {
                    next.push(MentionSegment::Text(rest[..pos].to_string()));
                }
                next.push(MentionSegment::Mention(file.clone()));
                rest = &rest[pos + needle.len()..];
            }
        }
        segments = next;
    }
    // 相邻 Text 段合并（边界保护跳过失败命中时会留下相邻文本段）
    let mut merged: Vec<MentionSegment> = Vec::with_capacity(segments.len());
    for segment in segments {
        match (merged.last_mut(), &segment) {
            (Some(MentionSegment::Text(prev)), MentionSegment::Text(text)) => prev.push_str(text),
            _ => merged.push(segment),
        }
    }
    merged.retain(|s| !matches!(s, MentionSegment::Text(t) if t.is_empty()));
    merged
}

/// 导航预览卡本体（打开卡与出场快照共用）：标题 2 行 + 助手摘要 3 行
fn nav_card_body(data: &NavCardData, cx: &App) -> Div {
    let (_, _, user_preview, assistant_preview, is_text) = data;
    v_flex()
        .w(px(320.))
        .p_3()
        .gap_2()
        .bg(cx.theme().popover)
        .border_1()
        .border_color(cx.theme().border)
        .rounded_lg()
        .shadow_lg()
        .child(
            div()
                .text_sm()
                .font_medium()
                .line_clamp(2)
                .child(user_preview.clone()),
        )
        .child(
            div()
                .text_sm()
                // 文本回复 80% 亮度，占位文案最暗档
                //（对齐 ZCode 的 popover-foreground/80 与 foreground-subtle 分档）
                .text_color(if *is_text {
                    cx.theme().foreground.opacity(0.8)
                } else {
                    cx.theme().muted_foreground
                })
                .line_clamp(3)
                .child(assistant_preview.clone()),
        )
}

impl ThreadView {
    pub(crate) fn render_user_message(
        &self,
        ix: usize,
        message: &ChatMessage,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // 后台子代理完成/失败的合成消息：渲染为通知卡而非用户气泡
        //（剥标签只在显示层，message.text 原文不动，live 与回放共用此路径）
        if let Some(note) = as_task_notification(&message.text) {
            return self.render_task_notification(ix, &note, message, cx);
        }
        // 展示文本与 @提及分段（剥旧记录后缀 + 内联 chip）；文本里没出现的
        // files（程序化附件等）回落到气泡上方的传统 chip 行
        let display_text = strip_reference_suffix(&message.text, &message.files);
        let segments = split_mention_segments(&display_text, &message.files);
        let unmatched: Vec<String> = {
            let inline: std::collections::HashSet<&str> = segments
                .iter()
                .filter_map(|s| match s {
                    MentionSegment::Mention(path) => Some(path.as_str()),
                    _ => None,
                })
                .collect();
            message
                .files
                .iter()
                .filter(|f| !inline.contains(f.as_str()))
                .cloned()
                .collect()
        };
        v_flex()
            .w_full()
            .items_end()
            .gap_1()
            .when(!unmatched.is_empty(), |this| {
                this.child(h_flex().gap_1().children(unmatched.iter().map(|file| {
                    h_flex()
                        .gap_1()
                        .px_2()
                        .py_0p5()
                        .rounded(cx.theme().radius)
                        .bg(cx.theme().accent)
                        .child(
                            Icon::new(IconName::FileText)
                                .size_3()
                                .text_color(cx.theme().muted_foreground),
                        )
                        .child(div().text_xs().child(file.clone()))
                })))
            })
            .child(
                v_flex()
                    .max_w(relative(0.8))
                    .px_3()
                    .py_2()
                    .gap_2()
                    .rounded_2xl()
                    .bg(cx.theme().accent)
                    .text_sm()
                    .with_animation(
                        "user-msg-enter",
                        Animation::new(std::time::Duration::from_millis(150))
                            .with_easing(ease_out_quint()),
                        |el, delta| el.top(px(4.0 * (1.0 - delta))).opacity(delta),
                    )
                    // 图片附件：缩略图横排（换行），在文本上方（气泡内容第一行）；
                    // 丢失/坏字节 → 文本 chip 降级
                    .when(!message.images.is_empty(), |this| {
                        this.child(h_flex().gap_2().flex_wrap().children(
                            message.images.iter().enumerate().map(|(image_ix, image)| {
                                self.render_user_image(ix, image_ix, image, cx)
                            }),
                        ))
                    })
                    // @提及内联渲染：文本段保持窗口级选择（共享 handle + 阅读序），
                    // chip = 文件图标 + 下划线文件名，点击在右侧面板打开文件
                    .when(!display_text.is_empty(), |this| {
                        let handle = message
                            .selection
                            .as_ref()
                            .expect("render 时已惰性创建选择 handle")
                            .0
                            .clone();
                        if segments
                            .iter()
                            .any(|s| matches!(s, MentionSegment::Mention(_)))
                        {
                            this.child(h_flex().flex_wrap().items_center().children(
                                segments.iter().enumerate().map(|(six, segment)| {
                                    match segment {
                                        MentionSegment::Text(text) => SelectableText::with_handle(
                                            ("user-msg-text", ix * 1024 + six),
                                            handle.clone(),
                                            text.clone(),
                                        )
                                        .document_order(six as u64)
                                        .into_any_element(),
                                        MentionSegment::Mention(path) => {
                                            let file_name =
                                                path.rsplit('/').next().unwrap_or(path).to_string();
                                            let open_path = path.clone();
                                            h_flex()
                                                .id(("user-mention", ix * 1024 + six))
                                                .test_support()
                                                .gap_1()
                                                .cursor_pointer()
                                                .rounded(cx.theme().radius)
                                                .hover(|this| {
                                                    this.bg(cx.theme().accent.opacity(0.6))
                                                })
                                                .on_click(cx.listener(move |_, _, _, cx| {
                                                    cx.emit(ThreadEvent::OpenFile {
                                                        path: open_path.clone(),
                                                        line: None,
                                                    });
                                                }))
                                                .child(
                                                    Icon::new(IconName::FileText)
                                                        .size_3p5()
                                                        .text_color(cx.theme().muted_foreground),
                                                )
                                                .child(div().text_sm().underline().child(file_name))
                                                .into_any_element()
                                        }
                                    }
                                }),
                            ))
                        } else {
                            this.child(
                                SelectableText::with_handle(
                                    ("user-msg-text", ix),
                                    handle,
                                    display_text.clone(),
                                )
                                .document_order(ix as u64),
                            )
                        }
                    }),
            )
            .into_any_element()
    }

    /// 后台子代理的合成通知块（A3d，kimi-code 同款）：
    /// 右对齐「✓ 由后台发送（Agent）」小标签 + 用户气泡同款底色的限宽气泡
    ///（标题 / 已完成·耗时 / 结果文件行 / 默认折叠的原始 payload）。
    /// 点击气泡开右侧子代理对话 tab（复制路径按钮与 payload 折叠行的命中区
    /// stop_propagation 不冒泡；缺 agent_id 属性时不挂点击——core 产物恒有，
    /// 缺省只出现在手工构造文本的容错场景）。
    pub(crate) fn render_task_notification(
        &self,
        message_ix: usize,
        note: &TaskNotification,
        message: &ChatMessage,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let failed = note.status.as_deref() == Some("failed");
        let subtle = cx.theme().muted_foreground;
        let subtlest = subtle.opacity(0.6);
        let title = note
            .description
            .clone()
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| "后台子代理".to_string());
        // 状态行：已完成/失败（N 步）· 耗时 …（属性缺哪个省哪个）
        let status_word = if failed { "失败" } else { "已完成" };
        let steps = note.turns.as_deref().map(|t| format!("{t} 步"));
        let cost = note
            .duration_ms
            .map(|ms| format!("耗时 {}", format_notification_duration(ms)));
        let status_line = match (steps, cost) {
            (Some(steps), Some(cost)) => format!("{status_word} {steps} · {cost}"),
            (Some(steps), None) => format!("{status_word} {steps}"),
            (None, Some(cost)) => format!("{status_word} · {cost}"),
            (None, None) => status_word.to_string(),
        };
        // UI 态在 render 前的预备循环里已惰性创建；防御 None（理论上不会走到）
        let ui = message.notification_ui.as_ref();
        let payload_open = ui.is_some_and(|u| u.payload_open);
        let copied = ui.is_some_and(|u| u.copied);
        let payload_scroll = ui.map(|u| u.payload_scroll.clone());
        let record_size = ui.and_then(|u| u.record_size);
        let agent_id_click = note
            .agent_id
            .clone()
            .map(|agent_id| (agent_id, title.clone()));
        v_flex()
            .w_full()
            .items_end()
            .gap_1()
            // 上方右对齐小标签：✓/✗ 由后台发送（Agent）
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Icon::new(if failed {
                            IconName::Close
                        } else {
                            IconName::CircleCheck
                        })
                        .size_3()
                        .text_color(if failed {
                            cx.theme().danger
                        } else {
                            cx.theme().success
                        }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(if failed { cx.theme().danger } else { subtle })
                            .child("由后台发送（Agent）"),
                    ),
            )
            // 气泡（A3d：与用户消息气泡同款底色/圆角/padding，宽随内容、上限 520px；
            // 失败版不换底色，只有状态词与标签走 danger）
            .child(
                v_flex()
                    .id(("task-notification", message_ix))
                    .max_w(px(520.))
                    .gap_2()
                    .px_3()
                    .py_2()
                    .rounded_2xl()
                    .bg(cx.theme().accent)
                    .when_some(agent_id_click, |this, (agent_id, title)| {
                        this.cursor_pointer()
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(ThreadEvent::OpenSubagent {
                                    agent_id: agent_id.clone(),
                                    title: title.clone(),
                                });
                            }))
                    })
                    // 第 1 行：标题（description，缺省「后台子代理」）
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(cx.theme().foreground)
                            .child(title),
                    )
                    // 第 2 行：状态（已完成/失败 · 耗时）
                    .child(
                        div()
                            .text_xs()
                            .text_color(if failed { cx.theme().danger } else { subtle })
                            .child(status_line),
                    )
                    // 第 3 行：结果文件（doc 图标 + 中段省略路径 + 大小 + 复制路径按钮）；
                    // 指向 result.md（core 产物恒带 result 属性；缺省时整行省略）
                    .when_some(note.result.clone(), |this, result_path| {
                        let size_text = match record_size {
                            Some(Some(bytes)) => format_file_size(bytes),
                            Some(None) => "记录已删除".to_string(),
                            None => String::new(),
                        };
                        this.child(
                            h_flex()
                                .w_full()
                                .gap_1()
                                .child(
                                    Icon::new(IconName::FileText)
                                        .size_3p5()
                                        .text_color(subtlest),
                                )
                                .child(
                                    div()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .text_xs()
                                        .text_color(subtle)
                                        .child(elide_record_path(&result_path)),
                                )
                                .when(!size_text.is_empty(), |this| {
                                    this.child(
                                        div()
                                            .flex_shrink_0()
                                            .text_xs()
                                            .text_color(subtlest)
                                            .child(size_text),
                                    )
                                })
                                .child(div().flex_1())
                                .child(
                                    Button::new(("task-notification-copy", message_ix))
                                        .xsmall()
                                        .outline()
                                        .label(if copied { "已复制" } else { "复制路径" })
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            // 复制路径不冒泡到卡体（不开子代理 tab）
                                            cx.stop_propagation();
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                result_path.clone(),
                                            ));
                                            if let Some(ui) = this
                                                .messages
                                                .get_mut(message_ix)
                                                .and_then(|m| m.notification_ui.as_mut())
                                            {
                                                ui.copied = true;
                                            }
                                            cx.notify();
                                        })),
                                ),
                        )
                    })
                    // 第 4 行：「原始 payload」折叠行（默认收起；命中区不冒泡到卡体）
                    .child(
                        h_flex()
                            .id(("task-notification-payload-toggle", message_ix))
                            .gap_1()
                            .py_0p5()
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                if let Some(ui) = this
                                    .messages
                                    .get_mut(message_ix)
                                    .and_then(|m| m.notification_ui.as_mut())
                                {
                                    ui.payload_open = !ui.payload_open;
                                }
                                cx.notify();
                            }))
                            .child(
                                Icon::new(if payload_open {
                                    IconName::ChevronDown
                                } else {
                                    IconName::ChevronRight
                                })
                                .size_3()
                                .text_color(subtlest),
                            )
                            .child(div().text_xs().text_color(subtle).child("原始 payload")),
                    )
                    // 展开区：payload 原文（含标签全文），等宽 + 更深底 + 限高 240 内滚
                    .when(payload_open, |this| {
                        let block = div()
                            .id(("task-notification-payload", message_ix))
                            .w_full()
                            .max_h(px(240.))
                            .overflow_y_scroll()
                            .rounded(cx.theme().radius)
                            .bg(cx.theme().muted)
                            .p_2()
                            .text_xs()
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_color(subtle)
                            .child(message.text.clone());
                        match payload_scroll {
                            // 滚轮落在 payload 区时不穿透到外层消息列表
                            Some(handle) => this.child(
                                block
                                    .track_scroll(&handle)
                                    .on_scroll_wheel(consume_scroll(&handle)),
                            ),
                            None => this.child(block),
                        }
                    })
                    // 与用户气泡同款的入场动画（链尾：AnimationElement 不再支持交互方法）
                    .with_animation(
                        "user-msg-enter",
                        Animation::new(std::time::Duration::from_millis(150))
                            .with_easing(ease_out_quint()),
                        |el, delta| el.top(px(4.0 * (1.0 - delta))).opacity(delta),
                    ),
            )
            .into_any_element()
    }

    /// 用户消息的单张图片附件：缩略图（最长边 72px，等比不放大，圆角），
    /// 点击开灯箱看大图；文件丢失/解码失败 → 「[图片 N（已失效）]」文本 chip（不可点）
    pub(crate) fn render_user_image(
        &self,
        message_ix: usize,
        image_ix: usize,
        image: &UserImage,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match &image.thumb {
            Some(thumb) => div()
                .id(("user-image", message_ix * 256 + image_ix))
                .relative()
                .w(px(72.))
                .h(px(72.))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_lightbox(message_ix, image_ix, window, cx);
                }))
                .child(
                    gpui_kit::img(thumb.clone())
                        .w(px(72.))
                        .h(px(72.))
                        .object_fit(ObjectFit::Cover)
                        .rounded_md(),
                )
                .child(
                    div()
                        .absolute()
                        .bottom_1()
                        .right_1()
                        .px_1()
                        .py_0p5()
                        .rounded_full()
                        .bg(gpui_kit::black().opacity(0.72))
                        .text_xs()
                        .text_color(gpui_kit::white())
                        .child(message_image_number(image_ix).to_string()),
                )
                .into_any_element(),
            None => div()
                .px_2()
                .py_1()
                .rounded(cx.theme().radius)
                .bg(cx.theme().muted)
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(format!(
                    "[图片 {}（已失效）]",
                    message_image_number(image_ix)
                ))
                .into_any_element(),
        }
    }

    /// 回合工作行（对齐 ZCode AssistantHistoryStatus）：回合结束后该轮的
    /// 思考块/工具卡折叠成这一行，点击整行展开/收起。chevron 常显（截图同款），
    /// 不做展开高度动画——工作段与正文段在消息内交错，不是连续区块
    fn render_work_row(
        &self,
        ix: usize,
        message: &ChatMessage,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let label = match message.work_state {
            Some(WorkState::Completed { duration: Some(d) }) => {
                // 毫秒 → 秒向最近取整、至少 1 秒（对齐 ZCode workDuration）
                let secs = (d.as_millis() as f64 / 1000.).round() as u64;
                fmt_work_duration(secs.max(1), "已工作")
            }
            // 回放里无 TurnStats 的历史回合：没有真实时长
            Some(WorkState::Completed { duration: None }) => "已处理".to_string(),
            Some(WorkState::Stopped) => "已停止".to_string(),
            None => return div().into_any_element(),
        };
        let open = message.work_open;
        h_flex()
            .id(("work-row", ix))
            .test_support()
            .w_full()
            .items_center()
            .gap_1()
            .cursor_pointer()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(label)
            .child(
                Icon::new(if open {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .size_3p5()
                .text_color(cx.theme().muted_foreground),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                if let Some(message) = this.messages.get_mut(ix) {
                    message.work_open = !message.work_open;
                }
                cx.notify();
            }))
            .into_any_element()
    }

    pub(crate) fn render_message(
        &self,
        ix: usize,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let message = &self.messages[ix];
        match message.role {
            Role::User => self.render_user_message(ix, message, cx),
            Role::System => match message.system_kind {
                SystemNoteKind::Plain => div()
                    .w_full()
                    .text_center()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(message.text.clone())
                    .into_any_element(),
                SystemNoteKind::Compacted => div()
                    .id(("compact-note", ix))
                    .test_support()
                    .w_full()
                    .child(render_compact_divider(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .child(
                                Icon::new(AssetIconName::Archive)
                                    .size_3p5()
                                    .text_color(cx.theme().muted_foreground),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("上下文已压缩"),
                            )
                            .into_any_element(),
                        cx,
                    ))
                    .into_any_element(),
            },
            Role::Assistant => {
                let has_work = message
                    .segments
                    .iter()
                    .any(|s| matches!(s, Segment::Thinking { .. } | Segment::ToolCall { .. }));
                // 回合已结束且未手动展开：思考块/工具卡收进工作行，只留正文
                let collapse_work = message.work_state.is_some() && !message.work_open;
                // 每行带稳定键（段下标/固定名），工作行出现与折叠切换不会
                // 让其余行的 seg-enter 动画键移位重播
                let mut segments: Vec<(String, AnyElement)> =
                    Vec::with_capacity(message.segments.len() + 2);
                if message.work_state.is_some() && has_work {
                    segments.push(("work".to_string(), self.render_work_row(ix, message, cx)));
                }
                for (six, segment) in message.segments.iter().enumerate() {
                    // 审批不占独立行：待批准状态显示在对应的工具调用行上
                    //（ApprovalRequested 紧跟在该工具的 ToolCallBegin 之后发出）
                    if matches!(segment, Segment::Approval { .. }) {
                        continue;
                    }
                    if collapse_work
                        && matches!(segment, Segment::Thinking { .. } | Segment::ToolCall { .. })
                    {
                        continue;
                    }
                    let key = format!("seg-{six}");
                    segments.push((
                        key,
                        match segment {
                            Segment::Thinking {
                                text,
                                open,
                                duration,
                                ticker,
                                body_scroll,
                                ticker_scroll,
                                expand_anim,
                                ..
                            } => self.render_thinking(
                                ix,
                                six,
                                text,
                                *open,
                                *duration,
                                ticker,
                                body_scroll,
                                ticker_scroll,
                                expand_anim,
                                window,
                                cx,
                            ),
                            Segment::Markdown { state, .. } => {
                                // 表格对齐 ZCode（w-max min-w-full，PR #2）：列宽按实测
                                // 内容分配、贴合内容（wrap 表格按字符数比例分列，「前四
                                // slot」这种短文本列会被压到折行）；帧宽不足时列先收缩
                                // 换行、到列地板后整体横向滚动（上游无滚动条，窗口极窄时
                                // 超宽可横滚但无视觉提示）。
                                // 不要动 table_cell 的 padding：列宽测量含 CELL_PAD_PX(16)，
                                // 改大会让所有列的内容盒比测量窄、短列反而折行（实测）。
                                // 行尾吞字（#3293，inline flow 全角标点量宽少算）
                                // 已由 0.7.1 根治：按整形后绘制宽度收紧重排。
                                let mut table = StyleRefinement::default();
                                table.overflow.x = Some(Overflow::Scroll);
                                TextView::new(state)
                                    .selectable(true)
                                    .stream_fade(self.streaming)
                                    .text_sm()
                                    .style(TextViewStyle::default().table(table))
                                    // 搜索跳转的 reveal 兜底：外层消息列表是
                                    // v_flex().overflow_y_scroll() 的 div 滚动容器，
                                    // 不是 gpui::list——reveal_range 不会自动滚它
                                    //（行不可见时上游报 Hidden，见 TextView::on_reveal
                                    // 文档）。这里按行 bounds（窗口坐标）手动把目标行
                                    // 滚进可视区；行已可见时上游报 Shown，不会调这里
                                    .on_reveal({
                                        let scroll_handle = self.scroll_handle.clone();
                                        move |line, _window, _cx| {
                                            let view = scroll_handle.bounds();
                                            let mut offset = scroll_handle.offset();
                                            if line.top() < view.top() {
                                                offset.y += view.top() - line.top();
                                            } else if line.bottom() > view.bottom() {
                                                offset.y -= line.bottom() - view.bottom();
                                            } else {
                                                return;
                                            }
                                            scroll_handle.set_offset(offset);
                                        }
                                    })
                                    .into_any_element()
                            }
                            Segment::ToolCall {
                                tool,
                                summary,
                                output,
                                is_error,
                                done,
                                expanded,
                                edit,
                                live_note,
                                agent_cards,
                                read_ui,
                                bash_ui,
                                expand_anim,
                                body_scroll,
                            } => {
                                let approval_pending = matches!(
                                    message.segments.get(six + 1),
                                    Some(Segment::Approval { decision: None, .. })
                                );
                                self.render_tool_card(
                                    ix,
                                    six,
                                    tool,
                                    summary,
                                    live_note.as_deref(),
                                    output,
                                    *is_error,
                                    *done,
                                    *expanded,
                                    approval_pending,
                                    edit.as_ref(),
                                    agent_cards,
                                    read_ui.as_ref(),
                                    bash_ui.as_ref(),
                                    expand_anim,
                                    body_scroll,
                                    window,
                                    cx,
                                )
                            }
                            Segment::Approval { .. } => unreachable!(),
                            Segment::TurnChanges {
                                rows,
                                open,
                                expand_anim,
                            } => self.render_turn_changes(ix, six, rows, *open, expand_anim, cx),
                        },
                    ));
                }
                if let Some(footer) = &message.footer {
                    segments.push((
                        "footer".to_string(),
                        h_flex()
                            .gap_2()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(footer.clone())
                            .into_any_element(),
                    ));
                }
                // 操作行（ZCode ConversationAssistantTextActions 同款）：悬停浮现，
                // 复制整轮 Markdown 原文 + 会话分叉；进行中的回合整行不出
                let turn_in_flight = self.streaming && ix == self.messages.len() - 1;
                let has_markdown = message
                    .segments
                    .iter()
                    .any(|s| matches!(s, Segment::Markdown { .. }));
                if !turn_in_flight {
                    let group_id = format!("assistant-msg-{ix}");
                    segments.push((
                        "actions".to_string(),
                        h_flex()
                            .id(("msg-actions", ix))
                            .test_support()
                            .gap_1()
                            // 透明而非 invisible：命中区保留（ZCode opacity-0 同款），
                            // 悬停消息时浮现
                            .opacity(0.)
                            .group_hover(group_id, |this| this.opacity(1.))
                            .when(has_markdown, |this| {
                                this.child(
                                    Button::new(("msg-copy", ix))
                                        .ghost()
                                        .xsmall()
                                        .icon(if message.copied {
                                            IconName::Check
                                        } else {
                                            IconName::Copy
                                        })
                                        .when(message.copied, |this| {
                                            this.text_color(cx.theme().success)
                                        })
                                        .tooltip("复制")
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            if let Some(message) = this.messages.get_mut(ix) {
                                                let text = message
                                                    .segments
                                                    .iter()
                                                    .filter_map(|s| match s {
                                                        Segment::Markdown { text, .. } => {
                                                            Some(text.as_str())
                                                        }
                                                        _ => None,
                                                    })
                                                    .collect::<Vec<_>>()
                                                    .join("\n\n");
                                                cx.write_to_clipboard(ClipboardItem::new_string(
                                                    text,
                                                ));
                                                message.copied = true;
                                                message.copied_gen += 1;
                                                let generation = message.copied_gen;
                                                // 1.2s 后回弹勾号（ZCode 1200ms 同款）；
                                                // 连点按代次作废旧计时器
                                                cx.spawn(
                                                    async move |this: WeakEntity<ThreadView>, cx| {
                                                        cx.background_executor()
                                                            .timer(std::time::Duration::from_millis(1200))
                                                            .await;
                                                        let _ = this.update(cx, |this, cx| {
                                                            if let Some(message) =
                                                                this.messages.get_mut(ix)
                                                                && message.copied_gen == generation
                                                            {
                                                                message.copied = false;
                                                                cx.notify();
                                                            }
                                                        });
                                                    },
                                                )
                                                .detach();
                                            }
                                            cx.notify();
                                        })),
                                )
                            })
                            .child(
                                Button::new(("msg-fork", ix))
                                    .ghost()
                                    .xsmall()
                                    .icon(AssetIconName::GitFork)
                                    .tooltip("分叉")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        // User 角色与 core 的 User 记录 1:1
                                        //（含后台子代理 task-notification 合成消息）
                                        let turns = this.messages[..=ix]
                                            .iter()
                                            .filter(|m| m.role == Role::User)
                                            .count();
                                        cx.emit(ThreadEvent::Fork { turns });
                                        cx.notify();
                                    })),
                            )
                            .into_any_element(),
                    ));
                }
                if self.plan_pending && ix == self.messages.len() - 1 {
                    segments.push((
                        "plan".to_string(),
                        h_flex()
                            .w_full()
                            .gap_2()
                            .px_3()
                            .py_2()
                            .rounded(cx.theme().radius)
                            .border_1()
                            .border_color(cx.theme().success)
                            .bg(cx.theme().success.opacity(0.08))
                            .child(
                                Icon::new(IconName::CircleCheck)
                                    .size_4()
                                    .text_color(cx.theme().success),
                            )
                            .child(div().text_sm().flex_1().child("计划已就绪"))
                            .child(
                                Button::new("execute-plan")
                                    .primary()
                                    .small()
                                    .label("执行计划 ▶")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.trigger_execute_plan(cx);
                                    })),
                            )
                            .into_any_element(),
                    ));
                }
                v_flex()
                    .group(format!("assistant-msg-{ix}"))
                    .w_full()
                    .gap_3()
                    .children(segments.into_iter().map(|(key, segment)| {
                        div()
                            .with_animation(
                                format!("seg-enter-{ix}-{key}"),
                                Animation::new(std::time::Duration::from_millis(150))
                                    .with_easing(ease_out_quint()),
                                |el, delta| el.opacity(delta),
                            )
                            .child(segment)
                    }))
                    .into_any_element()
            }
        }
    }

    /// turn 导航条（ZCode ConversationTurnNavigator 同款）：消息流左缘的竖排
    /// 小横条，一条用户消息一根。悬停时目标与相邻横条山峰式加宽；悬停稳定
    /// 120ms 后在横条右侧弹出该轮预览卡（用户消息前 2 行 + 助手回复前 3 行，
    /// 离开 80ms 关闭）；点击跳转对应消息。
    /// 无悬停时高亮视口顶部所属的 turn；流式中的最后一根保持最低亮度。
    pub(crate) fn render_turn_nav(
        &mut self,
        user_ixs: &[usize],
        active: Option<usize>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let foreground = cx.theme().foreground;
        let subtlest = cx.theme().muted_foreground.opacity(0.6);
        // 只有最后一轮可能处于流式（对齐 ZCode：同一 running turn 只有最后
        // 一条 query 呈现 running 强调）
        let running_ix = if self.streaming {
            user_ixs.last().copied()
        } else {
            None
        };
        let focus_pos = self
            .nav_hover
            .and_then(|hover| user_ixs.iter().position(|&ix| ix == hover));
        // rail 高度上限：对齐 ZCode 的 max-h calc(100% - 6rem)
        let rail_max_h = self.scroll_handle.bounds().size.height - px(96.);
        let rail_max_h = if rail_max_h < px(0.) {
            px(0.)
        } else {
            rail_max_h
        };
        // 清掉已不存在消息的横条 bounds
        self.nav_bar_bounds
            .borrow_mut()
            .retain(|&ix, _| ix < self.messages.len());
        // 预览卡内容只给当前打开的那根横条算（不必每帧为全部横条生成预览文本）。
        // freshly_opened = 上一帧无卡（从关闭态新开）：只有它才播入场淡入，
        // 横条间切换不重复播
        let freshly_opened = self.nav_card.is_some() && self.nav_card_last.is_none();
        let card: Option<NavCardData> = self.nav_card.and_then(|ix| {
            let bounds = self
                .nav_bar_bounds
                .borrow()
                .get(&ix)
                .map(|cell| cell.get())?;
            if bounds.size.width <= px(0.) {
                return None; // 首帧 prepaint 前还没有 bounds
            }
            let user_preview = nav_preview_text(&[self.messages[ix].text.as_str()], "（无文本）");
            let (assistant_preview, assistant_is_text) = self.nav_assistant_preview(ix, user_ixs);
            Some((
                ix,
                bounds,
                user_preview,
                assistant_preview,
                assistant_is_text,
            ))
        });
        self.nav_card_last = card.clone();
        let exit_card = self.nav_card_exit.clone();

        div()
            .absolute()
            .top_0()
            .bottom_0()
            .left(px(8.))
            .flex()
            .flex_col()
            .justify_center()
            .child(
                v_flex()
                    .id("turn-nav-rail")
                    .w(px(32.))
                    .max_h(rail_max_h)
                    .overflow_y_scroll()
                    .track_scroll(&self.nav_rail_scroll)
                    // 滚轮落在导航条上只滚 rail，不联动消息列表
                    .on_scroll_wheel(consume_scroll(&self.nav_rail_scroll))
                    .children(user_ixs.iter().enumerate().map(|(pos, &ix)| {
                        // 山峰式加宽：悬停项 2.6x，相邻 1.7x / 1.25x（对齐 ZCode 档位）；
                        // 最大 31.2px，不超出 32px 的 rail 宽度
                        let (scale, mut opacity, focus_color): (f32, f32, bool) =
                            match focus_pos.map(|focus| pos.abs_diff(focus)) {
                                Some(0) => (2.6_f32, 1.0, true),
                                Some(1) => (1.7, 0.86, false),
                                Some(2) => (1.25, 0.72, false),
                                _ => (1.0, 0.58, false),
                            };
                        // 无悬停时由滚动位置驱动的活动项强调
                        let show_active = focus_pos.is_none() && active == Some(ix);
                        if show_active {
                            opacity = 0.9;
                        }
                        if running_ix == Some(ix) {
                            opacity = opacity.max(0.72);
                        }
                        let color = if focus_color || show_active {
                            foreground
                        } else {
                            subtlest
                        };
                        let bounds_cell = self
                            .nav_bar_bounds
                            .borrow_mut()
                            .entry(ix)
                            .or_insert_with(|| Rc::new(Cell::new(Bounds::default())))
                            .clone();
                        div()
                            .id(("turn-nav-bar", ix))
                            .w_full()
                            .h(px(10.))
                            .flex()
                            .items_center()
                            .cursor_pointer()
                            .on_prepaint(move |bounds, _, _| bounds_cell.set(bounds))
                            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                                if *hovered {
                                    this.nav_hover = Some(ix);
                                    // 悬停稳定 120ms 才开卡（对齐 ZCode openDelay），
                                    // 快速滑过不闪卡
                                    cx.spawn(async move |this, cx| {
                                        cx.background_executor()
                                            .timer(std::time::Duration::from_millis(120))
                                            .await;
                                        this.update(cx, |this, cx| {
                                            if this.nav_hover == Some(ix) {
                                                this.nav_card = Some(ix);
                                                cx.notify();
                                            }
                                        })
                                        .ok();
                                    })
                                    .detach();
                                } else {
                                    // 只清自己这根的悬停：gpui 按绘制顺序逐元素
                                    // 判定 hover，向上滑（bar2→bar1）时新横条的
                                    // enter 先触发、旧横条的 leave 后到，无条件
                                    // 清空会把刚设置的 enter 抹掉
                                    if this.nav_hover == Some(ix) {
                                        this.nav_hover = None;
                                    }
                                    // 离开 80ms 才关闭（对齐 ZCode closeDelay）；
                                    // 关闭条件 = 打开的横条不再被悬停——离开导航条
                                    // 与「移到别的横条」都走关闭（ZCode 每根横条
                                    // 独立 HoverCard：移动即关闭重开，快速扫过不弹）
                                    cx.spawn(async move |this, cx| {
                                        cx.background_executor()
                                            .timer(std::time::Duration::from_millis(80))
                                            .await;
                                        this.update(cx, |this, cx| {
                                            if this
                                                .nav_card
                                                .is_some_and(|open| this.nav_hover != Some(open))
                                            {
                                                this.close_nav_card(cx);
                                            }
                                        })
                                        .ok();
                                    })
                                    .detach();
                                }
                                cx.notify();
                            }))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                // 跳走后暂停跟随；若目标就在底部附近，render
                                // 里的 at_bottom 检查会恢复跟随
                                this.follow_bottom = false;
                                this.nav_jump = true;
                                this.scroll_handle.scroll_to_top_of_item(ix);
                                cx.notify();
                            }))
                            .child(
                                // 透明度弹簧（内层：活动/悬停/运行态强调）+ 宽度
                                // 弹簧（外层：山峰加宽）。元素 id 保持弹簧状态，
                                // 目标变化平滑接力；颜色是离散两档，仍瞬时切换
                                div()
                                    .h(px(2.))
                                    .rounded_full()
                                    .bg(color)
                                    .with_spring(
                                        ("turn-nav-bar-o", ix),
                                        SpringAnimation::new(NAV_BAR_SPRING).to(opacity),
                                        |el, o| el.opacity(o),
                                    )
                                    .with_spring(
                                        ("turn-nav-bar-w", ix),
                                        SpringAnimation::new(NAV_BAR_SPRING).to(scale),
                                        |el, s| el.map_element(|d| d.w(px(12. * s))),
                                    ),
                            )
                            .into_any_element()
                    })),
            )
            // 预览卡：deferred 到窗口层绘制（逃出 rail 的滚动裁剪），锚定横条右侧
            //（ZCode 是 side=right align=start sideOffset=8 的 HoverCard；gpui-kit
            // 的 HoverCard 只有 corner 锚定、弹不到触发器右侧，故按 Positioner 自绘）。
            // 入场淡入仅「从关闭态新开」时播（freshly_opened），横条间切换不重播
            .when_some(card, |this, data @ (ix, bounds, _, _, _)| {
                let body: AnyElement = if freshly_opened {
                    nav_card_body(&data, cx)
                        .with_animation(
                            ("nav-card-enter", ix),
                            Animation::new(NAV_CARD_ANIM_DUR).with_easing(ease_out_quint()),
                            |el, d| el.opacity(d),
                        )
                        .into_any_element()
                } else {
                    nav_card_body(&data, cx).into_any_element()
                };
                this.child(
                    deferred(
                        Positioner::side(bounds)
                            .placement(Placement::Right)
                            .align(Align::Start)
                            .offset(px(8.))
                            .margin(px(8.))
                            .occlude()
                            .child(body),
                    )
                    .with_priority(1),
                )
            })
            // 出场卡：关闭一刻的快照播淡出（160ms），清理计时器按代次作废
            .when_some(exit_card, |this, data @ (_, bounds, _, _, _)| {
                let generation = self.nav_card_exit_gen;
                this.child(
                    deferred(
                        Positioner::side(bounds)
                            .placement(Placement::Right)
                            .align(Align::Start)
                            .offset(px(8.))
                            .margin(px(8.))
                            .occlude()
                            .child(nav_card_body(&data, cx).with_animation(
                                ("nav-card-exit", generation),
                                Animation::new(NAV_CARD_ANIM_DUR),
                                |el, d| el.opacity(1.0 - d),
                            )),
                    )
                    .with_priority(1),
                )
            })
            .into_any_element()
    }

    /// 关闭导航预览卡：渲染快照移入出场位播 160ms 淡出（代次进动画 id），
    /// 清理计时器按代次作废。离开导航条与「移到别的横条」共用此路径
    pub(crate) fn close_nav_card(&mut self, cx: &mut Context<Self>) {
        if let Some(data) = self.nav_card_last.take() {
            self.nav_card_exit_gen += 1;
            let generation = self.nav_card_exit_gen;
            self.nav_card_exit = Some(data);
            cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(NAV_CARD_ANIM_DUR + std::time::Duration::from_millis(40))
                    .await;
                this.update(cx, |this, cx| {
                    if this.nav_card_exit_gen == generation {
                        this.nav_card_exit = None;
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        }
        self.nav_card = None;
        cx.notify();
    }

    /// 导航预览卡的助手摘要：该用户消息之后第一条助手消息的 Markdown 文本拼接
    ///（对齐 ZCode：assistantTextRows 合并、最多 2 段 220 字符）。
    /// 无文本时按流式状态给占位文案；返回的 bool 表示是否为真实回复文本。
    pub(crate) fn nav_assistant_preview(&self, ix: usize, user_ixs: &[usize]) -> (String, bool) {
        let running = self.streaming && user_ixs.last() == Some(&ix);
        let texts: Vec<&str> = self.messages[ix + 1..]
            .iter()
            .find(|message| message.role == Role::Assistant)
            .map(|message| {
                message
                    .segments
                    .iter()
                    .filter_map(|segment| match segment {
                        Segment::Markdown { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        if texts.is_empty() {
            return (
                if running {
                    "正在生成…"
                } else {
                    "（暂无文本回复）"
                }
                .to_string(),
                false,
            );
        }
        (nav_preview_text(&texts, "（暂无文本回复）"), true)
    }
}

/// 拆成 (目录部分含结尾分隔符, 文件名)；无分隔符时目录为空
pub(crate) fn split_path(path: &str) -> (String, String) {
    match path.rfind(['/', '\\']) {
        Some(ix) => (path[..=ix].to_string(), path[ix + 1..].to_string()),
        None => (String::new(), path.to_string()),
    }
}

/// token 数自动单位：<1k 原样；k/M 级整除显示整数、否则一位小数
pub(crate) fn fmt_tokens(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        let k = n as f64 / 1_000.0;
        if k.fract().abs() < 0.05 {
            format!("{}k", k.round() as u64)
        } else {
            format!("{k:.1}k")
        }
    } else {
        let m = n as f64 / 1_000_000.0;
        if m.fract().abs() < 0.05 {
            format!("{}M", m.round() as u64)
        } else {
            format!("{m:.1}M")
        }
    }
}

/// 回合脚注的 token 统计段：未缓存输入 · 缓存命中（命中率）· 输出 ·
/// 首字时间 · 解码速度（不含首字；api_ms 为 0 的退化数据退回墙钟）
pub(crate) fn format_turn_stats(stats: &pig_protocol::TurnUsageStats) -> String {
    let total_input = stats.input + stats.cache_read;
    let hit_rate = if total_input > 0 {
        format!(
            " ({:.1}%)",
            stats.cache_read as f64 / total_input as f64 * 100.0
        )
    } else {
        String::new()
    };
    // 速度按纯解码时间算（API 总时长 − 首字等待，不含工具执行/审批等待）；
    // api_ms 为 0（mock 亚毫秒回合等退化数据）时退回墙钟时间
    let api_ms = if stats.api_ms > 0 {
        stats.api_ms
    } else {
        stats.duration_ms
    };
    // 平均首字 = 首字等待总和 ÷ 请求次数（多步回合一堆 TTFT 取平均；
    // api_steps 为 0 时按一步算，不除零）
    let steps = stats.api_steps.max(1);
    let ttft = if stats.ttft_ms > 0 {
        format!(
            " · 首字 {:.1}s",
            stats.ttft_ms as f64 / steps as f64 / 1000.0
        )
    } else {
        String::new()
    };
    let decode_ms = api_ms.saturating_sub(stats.ttft_ms);
    let speed = if decode_ms > 0 {
        format!(
            " · {:.1} tok/s",
            stats.output as f64 / (decode_ms as f64 / 1000.0)
        )
    } else {
        String::new()
    };
    format!(
        " · 输入 {}（未缓存）· 命中 {}{hit_rate} · 输出 {}{ttft}{speed}",
        fmt_tokens(stats.input),
        fmt_tokens(stats.cache_read),
        fmt_tokens(stats.output),
    )
}

/// 导航预览卡文本：按空行分段、段内连续空白折叠为空格，取前 2 段以换行拼接，
/// 超 220 字符截断补「...」（对齐 ZCode conversationTurnNavigatorHelpers 的
/// buildPreviewText：maxPreviewChars 220 / maxPreviewParagraphs 2）
pub(crate) fn nav_preview_text(parts: &[&str], fallback: &str) -> String {
    let joined = parts.join("\n\n");
    let mut paragraphs: Vec<String> = Vec::new();
    let mut current = String::new();
    for line in joined.trim().lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        if words.is_empty() {
            if !current.is_empty() {
                paragraphs.push(std::mem::take(&mut current));
                if paragraphs.len() == 2 {
                    break;
                }
            }
        } else {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(&words.join(" "));
        }
    }
    if paragraphs.len() < 2 && !current.is_empty() {
        paragraphs.push(current);
    }
    if paragraphs.is_empty() {
        return fallback.to_string();
    }
    let text = paragraphs.join("\n");
    const MAX_CHARS: usize = 220;
    if text.chars().count() <= MAX_CHARS {
        text
    } else {
        let truncated: String = text.chars().take(MAX_CHARS - 3).collect();
        format!("{}...", truncated.trim_end())
    }
}
