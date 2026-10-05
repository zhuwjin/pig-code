use super::cards::{measure_ticker_width, ticker_roll_content};
use super::{
    TickerRoll, adjacent_image_index, as_task_notification, clamp_lightbox_pan,
    collect_lightbox_positions, elide_record_path, format_file_size, format_notification_duration,
    lightbox_display_size, lightbox_fit_scale, lightbox_pan_after_zoom, message_image_number,
    parse_image_link, split_image_links, ticker_target_line,
};
use std::time::{Duration, Instant};

#[test]
fn task_notification_strips_outer_tags() {
    // 无属性的最简形态（含换行；属性解析另测）：识别为通知
    assert!(
            as_task_notification(
                "<task-notification>\n后台子代理 a1（explore）已完成（3 步）。\n\n结果正文\n</task-notification>"
            )
            .is_some()
        );
    // 外围空白容错（trim 后再判定）
    assert!(as_task_notification("  <task-notification>正文</task-notification>\n").is_some());
}

#[test]
fn task_notification_rejects_plain_messages() {
    assert!(as_task_notification("普通用户消息").is_none());
    // 只有前缀/只有后缀都不算
    assert!(as_task_notification("<task-notification>没封口").is_none());
    assert!(as_task_notification("没开头</task-notification>").is_none());
    // 标签不在整段首尾（前面有正文）不算
    assert!(as_task_notification("引用：<task-notification>x</task-notification>").is_none());
    // 相似标签名（前缀后非 '>'/空白）不算
    assert!(as_task_notification("<task-notification-foo>x</task-notification>").is_none());
}

#[test]
fn task_notification_keeps_nested_tags() {
    // 嵌套同名标签不误判：strip_suffix 只认最外层闭标签，整段仍识别为通知
    assert!(
        as_task_notification(
            "<task-notification>外<task-notification>内</task-notification>外</task-notification>"
        )
        .is_some()
    );
}

#[test]
fn task_notification_parses_open_tag_attributes() {
    // core 实际产物：开标签带 agent_id/profile/status/turns/model/description/
    // duration_ms/record/result
    let note = as_task_notification(
            "<task-notification agent_id=\"a1-2\" profile=\"explore\" status=\"completed\" turns=\"3\" model=\"Mock · mock-model\" description=\"子代理自测委派\" duration_ms=\"12345\" record=\"/tmp/x/sessions/s1.agents/a1-2.jsonl\" result=\"/tmp/x/sessions/s1.agents/a1-2.result.md\">\n后台子代理 a1-2（explore）已完成（3 步）。\n\n结果\n</task-notification>",
        )
        .expect("带属性通知应解析");
    assert_eq!(note.agent_id.as_deref(), Some("a1-2"));
    assert_eq!(note.status.as_deref(), Some("completed"));
    assert_eq!(note.turns.as_deref(), Some("3"));
    assert_eq!(note.description.as_deref(), Some("子代理自测委派"));
    assert_eq!(note.duration_ms, Some(12345));
    assert_eq!(
        note.record.as_deref(),
        Some("/tmp/x/sessions/s1.agents/a1-2.jsonl")
    );
    assert_eq!(
        note.result.as_deref(),
        Some("/tmp/x/sessions/s1.agents/a1-2.result.md")
    );
    // 失败版
    let failed = as_task_notification(
            "<task-notification agent_id=\"a1-3\" profile=\"explore\" status=\"failed\" turns=\"20\" model=\"Mock · mock-model\" description=\"x\">\n失败原因\n</task-notification>",
        )
        .expect("失败通知应解析");
    assert_eq!(failed.status.as_deref(), Some("failed"));
}

#[test]
fn task_notification_tolerates_missing_attributes() {
    // 属性逐个独立解析：全部缺失 → 全 None
    //（解析健壮性；渲染侧按字段缺省——标题回退「后台子代理」、无 agent_id 不挂点击）
    let note = as_task_notification("<task-notification>正文</task-notification>")
        .expect("无属性通知应解析");
    assert!(note.agent_id.is_none());
    assert!(note.status.is_none());
    assert!(note.turns.is_none());
    assert!(note.description.is_none());
    assert!(note.duration_ms.is_none());
    assert!(note.record.is_none());
    assert!(note.result.is_none());
    // 部分属性缺失：逐字段 None 回落；非数字耗时 → None
    let partial = as_task_notification(
        "<task-notification agent_id=\"a9-1\" duration_ms=\"abc\">x</task-notification>",
    )
    .expect("部分属性应解析");
    assert_eq!(partial.agent_id.as_deref(), Some("a9-1"));
    assert!(partial.status.is_none());
    assert!(partial.duration_ms.is_none());
}

#[test]
fn notification_display_formatters() {
    // 耗时：<60s → X.X 秒；≥60s → m 分 ss 秒
    assert_eq!(format_notification_duration(2345), "2.3 秒");
    assert_eq!(format_notification_duration(60_000), "1 分 00 秒");
    assert_eq!(format_notification_duration(61_500), "1 分 01 秒");
    // 记录路径中段省略：短路径原样，长路径留首尾
    assert_eq!(elide_record_path("/tmp/a.jsonl"), "/tmp/a.jsonl");
    let long = "/var/folders/xx/yy/data/sessions/s1-2.agents/a123-1.jsonl";
    let elided = elide_record_path(long);
    assert!(elided.starts_with("/…/"), "{elided}");
    assert!(elided.ends_with("s1-2.agents/a123-1.jsonl"), "{elided}");
    // 文件大小
    assert_eq!(format_file_size(512), "512 B");
    assert_eq!(format_file_size(2048), "2.0 KB");
    assert_eq!(format_file_size(3 * 1024 * 1024), "3.0 MB");
}

#[test]
fn task_notification_sanitized_description_parses() {
    // core 消毒后的 description：无引号无换行、≤60 字符，串搜解析不受影响
    let sanitized: String = "描述 with space 与 CJK".to_string();
    let text = format!(
        "<task-notification agent_id=\"a1-1\" status=\"completed\" turns=\"1\" model=\"m\" description=\"{sanitized}\">\nb\n</task-notification>"
    );
    let note = as_task_notification(&text).expect("消毒后描述应解析");
    assert_eq!(note.description.as_deref(), Some(sanitized.as_str()));
}

#[test]
fn user_image_display_numbers_are_local_to_message() {
    assert_eq!(message_image_number(0), 1);
    assert_eq!(message_image_number(1), 2);
    assert_eq!(message_image_number(8), 9);
}

#[test]
fn lightbox_navigation_orders_available_images_within_message() {
    let positions = collect_lightbox_positions(4, [true, false, true, false]);
    assert_eq!(positions, vec![(4, 0), (4, 2)]);
    assert!(!positions.iter().any(|(message_ix, _)| *message_ix != 4));
    assert_eq!(adjacent_image_index(1, positions.len(), -1), Some(0));
    assert_eq!(adjacent_image_index(0, positions.len(), 1), Some(1));
    assert_eq!(adjacent_image_index(0, positions.len(), -1), None);
    assert_eq!(
        adjacent_image_index(positions.len() - 1, positions.len(), 1),
        None
    );
    assert_eq!(adjacent_image_index(0, 0, 1), None);
}

#[test]
fn lightbox_geometry_fits_and_clamps() {
    let viewport = (1000.0, 800.0);
    let scale = lightbox_fit_scale((2000, 1000), viewport);
    assert!(scale > 0.0);
    let size = lightbox_display_size((2000, 1000), viewport, 1.0);
    assert!(size.0 <= 900.0);
    assert!(size.1 <= 720.0);

    let clamped = clamp_lightbox_pan((900.0, -900.0), (1200.0, 1000.0), viewport);
    assert_eq!(clamped, (100.0, -100.0));
    assert_eq!(
        clamp_lightbox_pan((30.0, -30.0), (500.0, 400.0), viewport),
        (0.0, 0.0)
    );
}

#[test]
fn lightbox_zoom_keeps_pointer_anchor() {
    let viewport = (1000.0, 800.0);
    let origin = (50.0, 100.0);
    let pan = (20.0, -10.0);
    let pointer = (700.0, 500.0);
    let next = lightbox_pan_after_zoom(pan, (1600, 900), viewport, origin, pointer, 1.0, 2.0);
    let scale = lightbox_fit_scale((1600, 900), viewport);
    let before_point = (
        (pointer.0 - origin.0 - viewport.0 * 0.5 - pan.0) / scale,
        (pointer.1 - origin.1 - viewport.1 * 0.5 - pan.1) / scale,
    );
    let after_point = (
        (pointer.0 - origin.0 - viewport.0 * 0.5 - next.0) / (scale * 2.0),
        (pointer.1 - origin.1 - viewport.1 * 0.5 - next.1) / (scale * 2.0),
    );
    assert!((before_point.0 - after_point.0).abs() < 0.001);
    assert!((before_point.1 - after_point.1).abs() < 0.001);
}

#[test]
fn split_image_links_extracts_trailing_attachment_links() {
    // 正文 + 链接行
    let (body, indices) =
        split_image_links("看图说话\n\n[图片 1](pig-code-composer://attachments/m1)");
    assert_eq!(body, "看图说话");
    assert_eq!(indices, vec![1]);
    // 多图空格分隔
    let (body, indices) = split_image_links(
        "多图\n\n[图片 1](pig-code-composer://attachments/m1) [图片 2](pig-code-composer://attachments/m2)",
    );
    assert_eq!(body, "多图");
    assert_eq!(indices, vec![1, 2]);
    // 纯图消息（无正文前缀，整段即链接行）
    let (body, indices) = split_image_links("[图片 3](pig-code-composer://attachments/m3)");
    assert_eq!(body, "");
    assert_eq!(indices, vec![3]);
    // 正文本身含空行：只剥最后的链接行
    let (body, indices) =
        split_image_links("第一段\n\n第二段\n\n[图片 2](pig-code-composer://attachments/m2)");
    assert_eq!(body, "第一段\n\n第二段");
    assert_eq!(indices, vec![2]);
}

#[test]
fn split_image_links_leaves_non_links_untouched() {
    // 无链接 → 原样
    let (body, indices) = split_image_links("普通消息");
    assert_eq!(body, "普通消息");
    assert!(indices.is_empty());
    // 尾行混入非链接 token → 整体不拆
    let (body, indices) =
        split_image_links("看图\n\n[图片 1](pig-code-composer://attachments/m1) 别的");
    assert_eq!(
        body,
        "看图\n\n[图片 1](pig-code-composer://attachments/m1) 别的"
    );
    assert!(indices.is_empty());
    // label 与 URL 序号不一致 → 不算附件链接
    assert_eq!(
        parse_image_link("[图片 1](pig-code-composer://attachments/m2)"),
        None
    );
    // 其它协议/形态 → 不算
    assert_eq!(parse_image_link("[图片 1](https://x.com/m1)"), None);
    assert!(parse_image_link("[图片 12](pig-code-composer://attachments/m12)").is_some());
}

#[test]
fn ticker_target_line_picks_last_non_empty_line() {
    assert_eq!(ticker_target_line(""), None);
    assert_eq!(ticker_target_line("  \n\t\n"), None);
    // 最后一个非空行，行号是原文行下标（纵滚的 key）
    assert_eq!(
        ticker_target_line("第一行\n\n第二行\n\n"),
        Some((2, "第二行".to_string()))
    );
    // 裸 \r 与制表符压成单空格（渲染层把 \r 当换行，滚动行必须保持单行）
    assert_eq!(
        ticker_target_line("第一行\n第 二\t行\r尾"),
        Some((1, "第 二 行 尾".to_string()))
    );
    // 行号稳定：同行追加不换 key
    assert_eq!(ticker_target_line("abc"), Some((0, "abc".to_string())));
    assert_eq!(
        ticker_target_line("abc def"),
        Some((0, "abc def".to_string()))
    );
}

#[test]
fn ticker_roll_first_line_shows_without_animation() {
    let mut roll = TickerRoll::default();
    // 首行直接显示：不触发滚动（无定时器）、无退场行、不播入场动画
    assert!(!roll.feed((0, "第一行".to_string())));
    assert_eq!(roll.displayed, Some((0, "第一行".to_string())));
    assert!(!roll.rolled_in);
    assert!(roll.exiting.is_none());
}

#[test]
fn ticker_roll_refreshes_same_line_in_place() {
    let mut roll = TickerRoll::default();
    roll.feed((0, "想".to_string()));
    // 同行号追加：原位刷新，不滚动
    assert!(!roll.feed((0, "想更多".to_string())));
    assert_eq!(roll.displayed, Some((0, "想更多".to_string())));
    assert!(roll.exiting.is_none());
    assert!(!roll.rolling);
}

#[test]
fn ticker_roll_promotes_new_line_and_queues_during_hold() {
    let mut roll = TickerRoll::default();
    roll.feed((0, "a".to_string()));
    // 行号变且不在停留期：立即滚动
    assert!(roll.feed((1, "b".to_string())));
    assert_eq!(roll.displayed, Some((1, "b".to_string())));
    assert_eq!(roll.exiting, Some((0, "a".to_string())));
    assert!(roll.rolled_in);
    assert!(roll.rolling);
    // 停留期内：第一条排队保位，第二条占第二格，再来的替换第二格
    assert!(!roll.feed((2, "c".to_string())));
    assert!(!roll.feed((3, "d".to_string())));
    assert!(!roll.feed((4, "e".to_string())));
    assert_eq!(roll.queue, vec![(2, "c".to_string()), (4, "e".to_string())]);
    // 同 key 覆盖排队中的条目（文本原位更新，不新增条目）
    assert!(!roll.feed((4, "e+".to_string())));
    assert_eq!(
        roll.queue,
        vec![(2, "c".to_string()), (4, "e+".to_string())]
    );
    // 滚动中的当前行同行号追加仍是原位刷新
    assert!(!roll.feed((1, "b+".to_string())));
    assert_eq!(roll.displayed, Some((1, "b+".to_string())));
}

#[test]
fn ticker_roll_fire_promotes_next_and_stops_when_drained() {
    let mut roll = TickerRoll::default();
    roll.feed((0, "a".to_string()));
    roll.feed((1, "b".to_string()));
    let generation = roll.generation;
    // 代次不符的旧定时器直接作废
    assert!(!roll.fire(generation + 1, Instant::now()));
    // 空队列：收尾（清退场行、退出停留期），不续期
    assert!(!roll.fire(generation, Instant::now()));
    assert!(!roll.rolling);
    assert!(roll.exiting.is_none());
    // 有新行排队时：滚入队首并续期
    assert!(roll.feed((2, "c".to_string())));
    roll.feed((3, "d".to_string()));
    let generation = roll.generation;
    assert!(roll.fire(generation, Instant::now()));
    assert_eq!(roll.displayed, Some((3, "d".to_string())));
    assert_eq!(roll.exiting, Some((2, "c".to_string())));
    assert!(roll.queue.is_empty());
}

#[test]
fn ticker_roll_fire_skips_stale_queue_on_timer_drift() {
    let mut roll = TickerRoll::default();
    roll.feed((0, "a".to_string()));
    roll.feed((1, "b".to_string()));
    roll.feed((2, "c".to_string()));
    roll.feed((3, "d".to_string()));
    // 模拟主线程繁忙：定时器晚到 >250ms（回填上次滚动时刻）
    roll.promoted_at = Some(Instant::now() - Duration::from_secs(2));
    let generation = roll.generation;
    assert!(roll.fire(generation, Instant::now()));
    // 跳过中间条 c，直接播最新 d
    assert_eq!(roll.displayed, Some((3, "d".to_string())));
    assert!(roll.queue.is_empty());
}

#[test]
fn ticker_roll_reset_invalidates_pending_timer() {
    let mut roll = TickerRoll::default();
    roll.feed((0, "a".to_string()));
    roll.feed((1, "b".to_string()));
    roll.feed((2, "c".to_string()));
    let generation = roll.generation;
    // 展开/收起：重置到最新行，无退场、无排队、无入场动画
    roll.reset_to(ticker_target_line("a\nb\nc"));
    assert_eq!(roll.displayed, Some((2, "c".to_string())));
    assert!(roll.exiting.is_none());
    assert!(roll.queue.is_empty());
    assert!(!roll.rolled_in);
    assert!(!roll.rolling);
    // reset 前起的定时器到点不动作（代次已作废）
    assert!(!roll.fire(generation, Instant::now()));
    assert_eq!(roll.displayed, Some((2, "c".to_string())));
}

/// 横向钉尾回归：纵滚容器的内容必须保持自然宽度溢出视口，ScrollHandle 才能感知
/// 横向可滚（钉尾 `set_offset(-max_offset)` 依赖它）。回归史：滚动容器内的文本
/// 宽度会被布局钳进可用空间（max_offset 恒 0 → 内容停在开头，2026-09-30 实测），
/// 修复 = 显式量宽设给容器（measure_ticker_width，sidebar 跑马灯同款）。
#[gpui_kit::test]
fn ticker_roll_content_overflows_viewport(cx: &mut gpui_kit::TestAppContext) {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AppContext as _, InteractiveElement as _, ParentElement as _,
        StatefulInteractiveElement as _, Styled as _,
    };
    cx.update(gpui_kit::init);

    struct Probe {
        scroll: gpui_kit::ScrollHandle,
    }
    impl gpui_kit::Render for Probe {
        fn render(
            &mut self,
            window: &mut gpui_kit::Window,
            cx: &mut gpui_kit::Context<Self>,
        ) -> impl gpui_kit::IntoElement {
            // 与 render_thinking 相同的嵌套：限宽外层 + 滚动 viewport + 纵滚容器
            let line = "纵滚回归探针".repeat(40);
            let width = measure_ticker_width(&line, window, cx);
            gpui_kit::div().size_full().child(
                gpui_kit::div().w(gpui_kit::px(200.)).child(
                    gpui_kit::div()
                        .id("ticker-viewport")
                        .w_full()
                        .overflow_x_scroll()
                        .track_scroll(&self.scroll)
                        .child(ticker_roll_content(
                            0,
                            0,
                            width,
                            line,
                            None,
                            false,
                            gpui_kit::hsla(0., 0., 0., 1.),
                        )),
                ),
            )
        }
    }

    let scroll = gpui_kit::ScrollHandle::new();
    let window = cx.open_window(gpui_kit::size(gpui_kit::px(800.), gpui_kit::px(600.)), {
        let scroll = scroll.clone();
        move |_window, _cx| Probe {
            scroll: scroll.clone(),
        }
    });
    cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
    let max = scroll.max_offset().x;
    assert!(
        max > gpui_kit::px(1.),
        "滚动行内容应溢出视口（钉尾依赖 max_offset），实际 {max:?}"
    );
}

#[test]
fn read_output_parsing() {
    use super::{
        is_read_code_output, parse_read_output, read_output_first_line, read_output_line_count,
    };
    // 标准分页输出：编号行 + 截断提示
    let output = "215\tlet base = base.strip_suffix(\".exe\");\n216\t}\n\n[已截断: 显示 215-216 行，共 300 行；用 offset 参数继续读取]";
    let parsed = parse_read_output(output).expect("应解析出内容");
    assert_eq!(parsed.lines.len(), 2);
    assert_eq!(parsed.lines[0].0, 215);
    assert_eq!(parsed.lines[0].1, "let base = base.strip_suffix(\".exe\");");
    assert_eq!(parsed.lines[1].1, "}");
    assert_eq!(
        parsed.notes,
        vec!["[已截断: 显示 215-216 行，共 300 行；用 offset 参数继续读取]"]
    );
    assert_eq!(read_output_line_count(output), 2);
    assert_eq!(read_output_first_line(output), Some(215));
    assert!(is_read_code_output(output));

    // 内容行自身以「数字+tab」开头：split_once 只切第一个 tab
    let parsed = parse_read_output("7\t100\t200").expect("应解析");
    assert_eq!(parsed.lines, vec![(7, "100\t200".to_string())]);

    // 空内容行（`{no}\t`）与 lossy 警告（无空行分隔）
    let parsed = parse_read_output("1\t\n2\tx\n[警告: 解码存在替换字符]").expect("应解析");
    assert_eq!(parsed.lines.len(), 2);
    assert_eq!(parsed.lines[0].1, "");
    assert_eq!(parsed.notes, vec!["[警告: 解码存在替换字符]"]);

    // 非内容输出：空文件/未变化/报错 → None（回落通用工具卡）
    assert!(parse_read_output("（空文件）").is_none());
    assert!(
        parse_read_output("（文件未变化：与上次 Read 参数相同且内容一致，无需重复读取）").is_none()
    );
    assert!(parse_read_output("文件不存在: foo.rs").is_none());
    assert!(!is_read_code_output("（空文件）"));
    assert_eq!(read_output_line_count("（空文件）"), 0);
    assert_eq!(read_output_first_line("（空文件）"), None);
}

/// 横向滚动回归：Read 卡不折行时，内容列用 measure_max_line_width 的显式宽度
/// （不显式给宽会被布局钳进可用空间，横向滚动失效）；这里验证「量宽 + 显式设宽
/// → ScrollHandle 感知横向溢出」整条链路（结构与 render_read_card 正文一致：
/// x 滚动容器 > v_flex（显式宽）> code_line_row（nowrap））。
#[gpui_kit::test]
fn read_card_nowrap_overflows_horizontally(cx: &mut gpui_kit::TestAppContext) {
    use crate::code_view::{code_line_row, highlight_code, measure_max_line_width};
    use gpui_kit::component::{ActiveTheme as _, v_flex};
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AppContext as _, InteractiveElement as _, ParentElement as _,
        StatefulInteractiveElement as _, Styled as _,
    };
    cx.update(gpui_kit::init);

    struct Probe {
        h_scroll: gpui_kit::ScrollHandle,
    }
    impl gpui_kit::Render for Probe {
        fn render(
            &mut self,
            window: &mut gpui_kit::Window,
            cx: &mut gpui_kit::Context<Self>,
        ) -> impl gpui_kit::IntoElement {
            let code = format!("短行\n{}", "let x = \"超长行\"; ".repeat(40));
            let theme = cx.theme().highlight_theme.clone();
            let highlighted = highlight_code(&code, "text", &theme);
            // 与卡同结构：gutter(28) + 代码格 padding(24) + 最大行宽
            let content_w = gpui_kit::px(28.)
                + gpui_kit::px(24.)
                + measure_max_line_width(&code, &highlighted, window, cx);
            gpui_kit::div().size_full().child(
                gpui_kit::div().w(gpui_kit::px(200.)).child(
                    gpui_kit::div()
                        .id("read-body-x")
                        .w_full()
                        .overflow_x_scroll()
                        .track_scroll(&self.h_scroll)
                        .child(v_flex().w(content_w).children(vec![
                            code_line_row(
                                1,
                                "短行",
                                vec![],
                                gpui_kit::px(28.),
                                gpui_kit::hsla(0., 0., 0., 1.),
                                false,
                            ),
                            code_line_row(
                                2,
                                highlighted.line_text(&code, 1),
                                highlighted.line_styles(1),
                                gpui_kit::px(28.),
                                gpui_kit::hsla(0., 0., 0., 1.),
                                false,
                            ),
                        ])),
                ),
            )
        }
    }

    let h_scroll = gpui_kit::ScrollHandle::new();
    let window = cx.open_window(gpui_kit::size(gpui_kit::px(800.), gpui_kit::px(600.)), {
        let h_scroll = h_scroll.clone();
        move |_window, _cx| Probe {
            h_scroll: h_scroll.clone(),
        }
    });
    cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
    let max = h_scroll.max_offset().x;
    assert!(
        max > gpui_kit::px(1.),
        "显式量宽后超长行应溢出视口产生横向滚动（max_offset.x），实际 {max:?}"
    );
}

/// 滚轮轴锁定回归：Read 卡（x/y 两层滚动容器）必须各自只响应自己轴的滚轮
/// delta。gpui 默认把纵向 delta 映射到仅 x 可滚容器（y→x）、横向 delta 映射到
/// 仅 y 可滚容器（x→y），不加 restrict_scroll_to_axis 时滚轮一动两轴同滚
///（2026-10-05 用户实测反馈）。结构与 render_read_card 正文一致。
/// 注：window.scroll(id) 依赖观测注册表（仅收 test_support 包裹的元素），
/// 这里用 dispatch_event 往已知布局位置直接派发滚轮事件。
#[gpui_kit::test]
fn read_card_scroll_wheel_is_axis_locked(cx: &mut gpui_kit::TestAppContext) {
    use crate::code_view::code_line_row;
    use gpui_kit::component::v_flex;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AppContext as _, InputEvent as _, InteractiveElement as _, ParentElement as _,
        StatefulInteractiveElement as _, Styled as _,
    };
    cx.update(gpui_kit::init);

    struct Probe {
        y_scroll: gpui_kit::ScrollHandle,
        h_scroll: gpui_kit::ScrollHandle,
    }
    impl gpui_kit::Render for Probe {
        fn render(
            &mut self,
            _window: &mut gpui_kit::Window,
            _cx: &mut gpui_kit::Context<Self>,
        ) -> impl gpui_kit::IntoElement {
            let line = "let x = \"超长行\"; ".repeat(40);
            gpui_kit::div().size_full().child(
                gpui_kit::div()
                    .w(gpui_kit::px(200.))
                    .h(gpui_kit::px(100.))
                    .child(
                        gpui_kit::div()
                            .id("read-body")
                            .w_full()
                            .max_h(gpui_kit::px(100.))
                            .overflow_y_scroll()
                            .restrict_scroll_to_axis()
                            .track_scroll(&self.y_scroll)
                            .child(
                                gpui_kit::div()
                                    .id("read-body-x")
                                    .overflow_x_scroll()
                                    .restrict_scroll_to_axis()
                                    .track_scroll(&self.h_scroll)
                                    .child(v_flex().w(gpui_kit::px(1200.)).children((0..40).map(
                                        |ix| {
                                            code_line_row(
                                                ix + 1,
                                                &line,
                                                vec![],
                                                gpui_kit::px(28.),
                                                gpui_kit::hsla(0., 0., 0., 1.),
                                                false,
                                            )
                                        },
                                    ))),
                            ),
                    ),
            )
        }
    }

    let y_scroll = gpui_kit::ScrollHandle::new();
    let h_scroll = gpui_kit::ScrollHandle::new();
    let window = cx.open_window(gpui_kit::size(gpui_kit::px(800.), gpui_kit::px(600.)), {
        let (y_scroll, h_scroll) = (y_scroll.clone(), h_scroll.clone());
        move |_window, _cx| Probe {
            y_scroll: y_scroll.clone(),
            h_scroll: h_scroll.clone(),
        }
    });
    // 卡片固定在窗口左上角的 200x100 区域；往其中心派滚轮事件
    let wheel = |dx: f32, dy: f32| {
        gpui_kit::ScrollWheelEvent {
            position: gpui_kit::point(gpui_kit::px(100.), gpui_kit::px(50.)),
            delta: gpui_kit::ScrollDelta::Lines(gpui_kit::point(dx, dy)),
            ..Default::default()
        }
        .to_platform_input()
    };
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        // 纵向滚轮（鼠标滚轮 = Lines，非 precise）：只能滚纵向，横向纹丝不动
        window.dispatch_event(wheel(0., -3.), cx);
        window.render_frame(cx);
    })
    .unwrap();
    assert_eq!(
        h_scroll.offset().x,
        gpui_kit::px(0.),
        "纵向滚轮不得带动横向滚动"
    );
    assert!(y_scroll.offset().y < gpui_kit::px(0.), "纵向滚轮应滚纵向");
    // 横向 delta（Shift+滚轮/触控板横滑）：只能滚横向，纵向保持原位
    //（滚动偏移与 y 轴同号约定：向右滚 = delta.x 为负、offset.x 变负）
    let y_before = y_scroll.offset().y;
    cx.update_window(window.into(), |_, window, cx| {
        window.dispatch_event(wheel(-4., 0.), cx);
        window.render_frame(cx);
    })
    .unwrap();
    assert!(
        h_scroll.offset().x != gpui_kit::px(0.),
        "横向 delta 应滚横向"
    );
    assert_eq!(y_scroll.offset().y, y_before, "横向 delta 不得带动纵向滚动");
}
/// 滚动链回归：Bash 卡的子卡用独立滚动句柄（不走 cards.rs 共享的
/// consume_scroll(body_scroll) 兜底），内容可滚时必须各自吞掉滚轮——
/// 否则穿透到外层消息列表双滚（2026-10-05 用户实测反馈）；内容不可滚
///（短命令）时必须穿透给列表（与其他工具卡行为一致）。
///
/// 注意点：①滚轮命中间接看 mouse_position（dispatch_event 不给滚轮更新它），
/// 派发前先派 MouseMove；②开合动画按真实墙钟播放，期间 max_h 裁切会挡住
/// 命中——展开后先睡过动画时长再测。
#[gpui_kit::test]
fn bash_card_scroll_traps_and_chains(cx: &mut gpui_kit::TestAppContext) {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AppContext as _, InputEvent as _};
    cx.update(gpui_kit::init);

    struct Probe {
        thread: gpui_kit::Entity<super::ThreadView>,
    }
    impl gpui_kit::Render for Probe {
        fn render(
            &mut self,
            _window: &mut gpui_kit::Window,
            _cx: &mut gpui_kit::Context<Self>,
        ) -> impl gpui_kit::IntoElement {
            use gpui_kit::IntoElement as _;
            self.thread.clone().into_any_element()
        }
    }

    let window = cx.open_window(
        gpui_kit::size(gpui_kit::px(600.), gpui_kit::px(400.)),
        |_, cx| {
            let thread = cx.new(super::ThreadView::new);
            Probe { thread }
        },
    );
    // 场景：Bash 卡在前（顶部，避免滚动后的几何换算）+ 30 条用户消息撑出
    // 外层列表滚动（命令 1 行 → 命令卡不可滚；输出 40 行 → 输出卡可滚）
    window
        .update(cx, |probe, _, cx| {
            probe.thread.update(cx, |view, cx| {
                view.append_user_message("先跑个命令".to_string(), vec![], cx);
                view.reduce_event(
                    pig_protocol::Event::ToolCallBegin {
                        session_id: "s".into(),
                        seq: 0,
                        item_id: "b1".into(),
                        tool: "Bash".into(),
                        input_summary: "echo hi".into(),
                        detail: String::new(),
                    },
                    cx,
                );
                view.reduce_event(
                    pig_protocol::Event::ToolCallEnd {
                        session_id: "s".into(),
                        seq: 1,
                        item_id: "b1".into(),
                        output: (1..=40)
                            .map(|i| format!("输出行 {i}"))
                            .collect::<Vec<_>>()
                            .join("\n"),
                        is_error: false,
                        edit: None,
                    },
                    cx,
                );
                for ix in 0..30 {
                    view.append_user_message(format!("消息 {ix}"), vec![], cx);
                }
                assert!(view.debug_expand_tool("Bash", cx), "应有 Bash 卡可展开");
            });
        })
        .unwrap();
    // 列表回顶（卡片在顶部）：append 期间每条消息都强制跟随贴底（含 deferred
    // 滚动标记），先渲一帧消费掉标记，再显式归零
    cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
    window
        .update(cx, |probe, _, cx| {
            probe.thread.update(cx, |view, _| {
                view.follow_bottom = false;
                view.scroll_handle
                    .set_offset(gpui_kit::point(gpui_kit::px(0.), gpui_kit::px(0.)));
            });
        })
        .unwrap();
    // 渲染一帧（bash_ui 创建 + 内容量高），再等动画播完（开合动画按真实墙钟
    // 走；不播完 max_h 裁切会把命中区收没）
    cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
    // 滚轮命中间接看 mouse_position：先把指针移过去（dispatch_event 只给
    // MouseMove/Down/Up 更新 mouse_position，滚轮事件不更新）
    let mouse_move = |position| {
        gpui_kit::MouseMoveEvent {
            position,
            ..Default::default()
        }
        .to_platform_input()
    };
    // dy: 负 = 向下滚（offset 变负），正 = 向上滚
    let wheel = |position, dy: f32| {
        gpui_kit::ScrollWheelEvent {
            position,
            delta: gpui_kit::ScrollDelta::Lines(gpui_kit::point(0., dy)),
            ..Default::default()
        }
        .to_platform_input()
    };
    let read_state = |probe: &Probe, cx: &gpui_kit::App| {
        let view = probe.thread.read(cx);
        let out = view
            .messages
            .iter()
            .flat_map(|m| &m.segments)
            .find_map(|s| match s {
                super::Segment::ToolCall {
                    bash_ui: Some(ui), ..
                } => Some((ui.out_scroll.offset().y, ui.cmd_scroll.bounds().center())),
                _ => None,
            });
        (
            view.scroll_handle.offset().y,
            view.scroll_handle.max_offset().y,
            out,
        )
    };
    let (outer_before, outer_max, out_state) = window
        .update(cx, |probe, _, cx| read_state(probe, cx))
        .unwrap();
    let (_, cmd_center) = out_state.expect("Bash 卡 UI 态应已创建");
    assert!(outer_max > gpui_kit::px(0.), "外层消息列表应可滚动");

    // 输出卡中心（此刻应在视口内）：用卡 bounds 的中心
    let out_center = window
        .update(cx, |probe, _, cx| {
            let view = probe.thread.read(cx);
            view.messages
                .iter()
                .flat_map(|m| &m.segments)
                .find_map(|s| match s {
                    super::Segment::ToolCall {
                        bash_ui: Some(ui), ..
                    } => Some(ui.out_scroll.bounds().center()),
                    _ => None,
                })
                .expect("Bash 卡 UI 态应已创建")
        })
        .unwrap();
    let list_bounds = window
        .update(cx, |probe, _, cx| {
            probe.thread.read(cx).scroll_handle.bounds()
        })
        .unwrap();
    assert!(
        out_center.y > list_bounds.top() && out_center.y < list_bounds.bottom(),
        "输出卡应在视口内: {out_center:?} vs {list_bounds:?}"
    );

    // 滚输出卡（可滚）：卡内滚动 + 外层列表纹丝不动（不穿透）
    cx.update_window(window.into(), |_, window, cx| {
        window.dispatch_event(mouse_move(out_center), cx);
        window.dispatch_event(wheel(out_center, -3.), cx);
        window.render_frame(cx);
    })
    .unwrap();
    let (outer_after, _, out_after) = window
        .update(cx, |probe, _, cx| read_state(probe, cx))
        .unwrap();
    let out_after = out_after.unwrap().0;
    assert!(out_after < gpui_kit::px(0.), "输出卡应滚动: {out_after:?}");
    assert_eq!(
        outer_after, outer_before,
        "可滚卡片不得把滚轮穿透给外层消息列表"
    );

    // 滚命令卡（1 行不可滚）：穿透给外层列表（与其他工具卡一致）
    cx.update_window(window.into(), |_, window, cx| {
        window.dispatch_event(mouse_move(cmd_center), cx);
        // 外层列表在顶部，向下滚（dy 为负）才有位移可观
        window.dispatch_event(wheel(cmd_center, -3.), cx);
        window.render_frame(cx);
    })
    .unwrap();
    let outer_chained = window
        .update(cx, |probe, _, cx| {
            probe.thread.read(cx).scroll_handle.offset().y
        })
        .unwrap();
    assert!(
        outer_chained < outer_after,
        "不可滚的卡片应把滚轮穿透给外层列表（{outer_after:?} → 下滚 {outer_chained:?}）"
    );
}

/// 压缩分隔条：进行中「正在压缩上下文」→ 完成「上下文已压缩」，摘要全文留 text。
#[gpui_kit::test]
fn compact_divider_progress_then_done(cx: &mut gpui_kit::TestAppContext) {
    use gpui_kit::AppContext as _;
    use gpui_kit::test::TestWindowExt as _;
    cx.update(gpui_kit::init);

    struct Probe {
        thread: gpui_kit::Entity<super::ThreadView>,
    }
    impl gpui_kit::Render for Probe {
        fn render(
            &mut self,
            _window: &mut gpui_kit::Window,
            _cx: &mut gpui_kit::Context<Self>,
        ) -> impl gpui_kit::IntoElement {
            use gpui_kit::IntoElement as _;
            self.thread.clone().into_any_element()
        }
    }

    let window = cx.open_window(
        gpui_kit::size(gpui_kit::px(800.), gpui_kit::px(600.)),
        |_, cx| {
            let thread = cx.new(super::ThreadView::new);
            Probe { thread }
        },
    );

    // 进行中：set_compacting(true) → 进行条出现且铺满内容列（分隔线 flex_grow 生效）
    window
        .update(cx, |probe, _, cx| {
            probe.thread.update(cx, |view, cx| {
                view.append_user_message("整理一下这个文件".to_string(), vec![], cx);
                view.set_compacting(true, cx);
                assert!(view.debug_compacting());
            });
        })
        .unwrap();
    cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
    cx.update_window(window.into(), |_, window, _| {
        let snap = window.find("compacting-divider");
        assert!(snap.visible(), "进行中分隔条应可见");
        assert!(
            snap.bounds().size.width > gpui_kit::px(600.),
            "分隔条应铺满内容列（分隔线 grow）: {:?}",
            snap.bounds()
        );
    })
    .unwrap();

    // 完成：进行条消失，「上下文已压缩」分隔条出现；摘要全文留在消息 text 供断言
    window
        .update(cx, |probe, _, cx| {
            probe.thread.update(cx, |view, cx| {
                view.set_compacting(false, cx);
                view.add_compact_note("[前文已压缩·模型摘要] 省略 9 条消息。\n\n摘要正文", cx);
                assert!(!view.debug_compacting());
            });
        })
        .unwrap();
    cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
    cx.update_window(window.into(), |_, window, _| {
        assert!(
            window.try_find("compacting-divider").is_none(),
            "完成后进行条应消失"
        );
        // 用户消息占 index 0，压缩条是 index 1
        let snap = window
            .try_find(("compact-note", 1usize))
            .expect("「上下文已压缩」分隔条应出现");
        assert!(snap.visible());
    })
    .unwrap();
    window
        .update(cx, |probe, _, cx| {
            probe.thread.update(cx, |view, _| {
                let notes = view.debug_system_notes();
                assert!(
                    notes
                        .iter()
                        .any(|n| n.contains("模型摘要") && n.contains("摘要正文")),
                    "摘要全文应保留在系统条 text: {notes:?}"
                );
            });
        })
        .unwrap();

    // 中止兜底：压缩中被打断（TurnAborted）标记必须清除，不残留进行条
    window
        .update(cx, |probe, _, cx| {
            probe.thread.update(cx, |view, cx| {
                view.set_compacting(true, cx);
                view.reduce_event(
                    pig_protocol::Event::TurnAborted {
                        session_id: "s".into(),
                        seq: 2,
                    },
                    cx,
                );
                assert!(!view.debug_compacting(), "TurnAborted 应清压缩标记");
            });
        })
        .unwrap();
}
