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
    use gpui_kit::component::ActiveTheme as _;
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
