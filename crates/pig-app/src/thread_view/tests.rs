use super::cards::{measure_ticker_width, ticker_roll_content};
use super::{MentionSegment, split_mention_segments};
use super::{
    TickerRoll, adjacent_image_index, as_task_notification, clamp_lightbox_pan,
    collect_lightbox_positions, elide_record_path, format_file_size, format_notification_duration,
    lightbox_display_size, lightbox_fit_scale, lightbox_pan_after_zoom, message_image_number,
    ticker_target_line,
};
use std::time::{Duration, Instant};

#[test]
fn split_mention_segments_matches_boundaries_and_longest_first() {
    // Basic: matches split into Mention, text keeps original spacing
    let files = vec!["src/a.rs".to_string()];
    assert_eq!(
        split_mention_segments("@src/a.rs take a look", &files),
        vec![
            MentionSegment::Mention("src/a.rs".into()),
            MentionSegment::Text(" take a look".into()),
        ]
    );
    // Boundary guard: @a.rs2 must not falsely match @a.rs
    let files = vec!["a.rs".to_string()];
    assert_eq!(
        split_mention_segments("@a.rs2 and @a.rs", &files),
        vec![
            MentionSegment::Text("@a.rs2 and ".into()),
            MentionSegment::Mention("a.rs".into()),
        ]
    );
    // Longest path first: src/a.rs matches whole, not hijacked by a.rs
    let files = vec!["a.rs".to_string(), "src/a.rs".to_string()];
    assert_eq!(
        split_mention_segments("@src/a.rs", &files),
        vec![MentionSegment::Mention("src/a.rs".into())]
    );
    // All occurrences split; files absent from the text produce no segment (left to the fallback chip row)
    let files = vec!["x.rs".to_string(), "y.rs".to_string()];
    assert_eq!(
        split_mention_segments("@x.rs and @x.rs", &files),
        vec![
            MentionSegment::Mention("x.rs".into()),
            MentionSegment::Text(" and ".into()),
            MentionSegment::Mention("x.rs".into()),
        ]
    );
}

#[test]
fn task_notification_strips_outer_tags() {
    // Minimal form without attributes (with newlines; attribute parsing tested separately): recognized as a notification
    assert!(
            as_task_notification(
                "<task-notification>\nBackground subagent a1 (explore) finished (3 turns).\n\nResult body\n</task-notification>"
            )
            .is_some()
        );
    // Tolerates surrounding whitespace (trimmed before checking)
    assert!(as_task_notification("  <task-notification>body</task-notification>\n").is_some());
}

#[test]
fn task_notification_rejects_plain_messages() {
    assert!(as_task_notification("plain user message").is_none());
    // Prefix-only or suffix-only does not count
    assert!(as_task_notification("<task-notification>unclosed").is_none());
    assert!(as_task_notification("no open tag</task-notification>").is_none());
    // Tags not spanning the whole text (body text in front) do not count
    assert!(as_task_notification("quote: <task-notification>x</task-notification>").is_none());
    // Similar tag names (non-'>'/whitespace after the prefix) do not count
    assert!(as_task_notification("<task-notification-foo>x</task-notification>").is_none());
}

#[test]
fn task_notification_keeps_nested_tags() {
    // Nested same-name tags not misjudged: strip_suffix only honors the outermost closing tag; the whole text is still recognized as a notification
    assert!(
        as_task_notification(
            "<task-notification>outer<task-notification>inner</task-notification>outer</task-notification>"
        )
        .is_some()
    );
}

#[test]
fn task_notification_parses_open_tag_attributes() {
    // Actual core output: the open tag carries agent_id/profile/status/turns/model/
    // description/duration_ms/record/result
    let note = as_task_notification(
            "<task-notification agent_id=\"a1-2\" profile=\"explore\" status=\"completed\" turns=\"3\" model=\"Mock · mock-model\" description=\"subagent selftest delegation\" duration_ms=\"12345\" record=\"/tmp/x/sessions/s1.agents/a1-2.jsonl\" result=\"/tmp/x/sessions/s1.agents/a1-2.result.md\">\nBackground subagent a1-2 (explore) finished (3 turns).\n\nResult\n</task-notification>",
        )
        .expect("notification with attributes should parse");
    assert_eq!(note.agent_id.as_deref(), Some("a1-2"));
    assert_eq!(note.status.as_deref(), Some("completed"));
    assert_eq!(note.turns.as_deref(), Some("3"));
    assert_eq!(
        note.description.as_deref(),
        Some("subagent selftest delegation")
    );
    assert_eq!(note.duration_ms, Some(12345));
    assert_eq!(
        note.record.as_deref(),
        Some("/tmp/x/sessions/s1.agents/a1-2.jsonl")
    );
    assert_eq!(
        note.result.as_deref(),
        Some("/tmp/x/sessions/s1.agents/a1-2.result.md")
    );
    // Failed variant
    let failed = as_task_notification(
            "<task-notification agent_id=\"a1-3\" profile=\"explore\" status=\"failed\" turns=\"20\" model=\"Mock · mock-model\" description=\"x\">\nFailure reason\n</task-notification>",
        )
        .expect("failed-status notification should parse");
    assert_eq!(failed.status.as_deref(), Some("failed"));
}

#[test]
fn task_notification_tolerates_missing_attributes() {
    // Attributes parsed independently: all missing → all None
    // (parse robustness; the render side defaults per field: title falls back to
    // "background subagent", no click wiring without agent_id)
    let note = as_task_notification("<task-notification>body</task-notification>")
        .expect("notification without attributes should parse");
    assert!(note.agent_id.is_none());
    assert!(note.status.is_none());
    assert!(note.turns.is_none());
    assert!(note.description.is_none());
    assert!(note.duration_ms.is_none());
    assert!(note.record.is_none());
    assert!(note.result.is_none());
    // Partially missing attributes: per-field None fallback; non-numeric duration → None
    let partial = as_task_notification(
        "<task-notification agent_id=\"a9-1\" duration_ms=\"abc\">x</task-notification>",
    )
    .expect("notification with partial attributes should parse");
    assert_eq!(partial.agent_id.as_deref(), Some("a9-1"));
    assert!(partial.status.is_none());
    assert!(partial.duration_ms.is_none());
}

#[test]
fn notification_display_formatters() {
    // Duration: <60s → X.X s; ≥60s → m min ss s
    assert_eq!(format_notification_duration(2345), "2.3 秒");
    assert_eq!(format_notification_duration(60_000), "1 分 00 秒");
    assert_eq!(format_notification_duration(61_500), "1 分 01 秒");
    // Record path middle elision: short paths as-is, long paths keep head and tail
    assert_eq!(elide_record_path("/tmp/a.jsonl"), "/tmp/a.jsonl");
    let long = "/var/folders/xx/yy/data/sessions/s1-2.agents/a123-1.jsonl";
    let elided = elide_record_path(long);
    assert!(elided.starts_with("/…/"), "{elided}");
    assert!(elided.ends_with("s1-2.agents/a123-1.jsonl"), "{elided}");
    // File size
    assert_eq!(format_file_size(512), "512 B");
    assert_eq!(format_file_size(2048), "2.0 KB");
    assert_eq!(format_file_size(3 * 1024 * 1024), "3.0 MB");
}

#[test]
fn task_notification_sanitized_description_parses() {
    // description sanitized by core: no quotes or newlines, ≤60 chars; string-search parsing unaffected
    let sanitized: String = "描述 with space 与 CJK".to_string();
    let text = format!(
        "<task-notification agent_id=\"a1-1\" status=\"completed\" turns=\"1\" model=\"m\" description=\"{sanitized}\">\nb\n</task-notification>"
    );
    let note = as_task_notification(&text).expect("sanitized description should parse");
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

/// User message image pipeline: each image_nums entry (media file number) occupies
/// one thumbnail slot; the body stays clean original text (the attachment-link
/// protocol is dead, no longer parsed from text).
/// With no media dir bound (test entity default) each image falls back to the
/// placeholder (the render layer shows an "unavailable" chip).
#[gpui_kit::test]
fn append_user_message_loads_images_by_nums(cx: &mut gpui_kit::TestAppContext) {
    use gpui_kit::AppContext as _;
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
    window
        .update(cx, |probe, _, cx| {
            probe.thread.update(cx, |view, cx| {
                view.append_user_message("describe the image".to_string(), vec![], vec![3, 7], cx);
                let message = view
                    .messages
                    .last()
                    .expect("user message should be appended");
                assert_eq!(
                    message.text, "describe the image",
                    "body should be the clean original text"
                );
                assert_eq!(message.images.len(), 2, "one slot per image_nums entry");
                assert!(
                    message.images.iter().all(|img| img.thumb.is_none()),
                    "no media dir: every image falls back to the placeholder (「已失效」 chip)"
                );
                // Message without images: images stays empty
                view.append_user_message("plain text".to_string(), vec![], vec![], cx);
                assert!(
                    view.messages
                        .last()
                        .expect("second message")
                        .images
                        .is_empty()
                );
            });
        })
        .unwrap();
}

#[test]
fn ticker_target_line_picks_last_non_empty_line() {
    assert_eq!(ticker_target_line(""), None);
    assert_eq!(ticker_target_line("  \n\t\n"), None);
    // Last non-empty line; the line number is the original text's line index (the vertical-scroll key)
    assert_eq!(
        ticker_target_line("line 1\n\nline 2\n\n"),
        Some((2, "line 2".to_string()))
    );
    // Bare \r and tabs collapse to single spaces (the render layer treats \r as a newline; the scrolling line must stay single-line)
    assert_eq!(
        ticker_target_line("line 1\na b\tc\rd"),
        Some((1, "a b c d".to_string()))
    );
    // Stable line number: appending to the same line keeps the key
    assert_eq!(ticker_target_line("abc"), Some((0, "abc".to_string())));
    assert_eq!(
        ticker_target_line("abc def"),
        Some((0, "abc def".to_string()))
    );
}

#[test]
fn ticker_roll_first_line_shows_without_animation() {
    let mut roll = TickerRoll::default();
    // First line shows directly: no scrolling (no timer), no exiting line, no enter animation
    assert!(!roll.feed((0, "line 1".to_string())));
    assert_eq!(roll.displayed, Some((0, "line 1".to_string())));
    assert!(!roll.rolled_in);
    assert!(roll.exiting.is_none());
}

#[test]
fn ticker_roll_refreshes_same_line_in_place() {
    let mut roll = TickerRoll::default();
    roll.feed((0, "think".to_string()));
    // Append to the same line number: in-place refresh, no scroll
    assert!(!roll.feed((0, "think more".to_string())));
    assert_eq!(roll.displayed, Some((0, "think more".to_string())));
    assert!(roll.exiting.is_none());
    assert!(!roll.rolling);
}

#[test]
fn ticker_roll_promotes_new_line_and_queues_during_hold() {
    let mut roll = TickerRoll::default();
    roll.feed((0, "a".to_string()));
    // Line number changed and outside the hold period: scroll immediately
    assert!(roll.feed((1, "b".to_string())));
    assert_eq!(roll.displayed, Some((1, "b".to_string())));
    assert_eq!(roll.exiting, Some((0, "a".to_string())));
    assert!(roll.rolled_in);
    assert!(roll.rolling);
    // Within the hold period: the first queued entry keeps its slot, the second takes the second slot, later ones replace the second slot
    assert!(!roll.feed((2, "c".to_string())));
    assert!(!roll.feed((3, "d".to_string())));
    assert!(!roll.feed((4, "e".to_string())));
    assert_eq!(roll.queue, vec![(2, "c".to_string()), (4, "e".to_string())]);
    // Same key overwrites the queued entry (text updated in place, no new entry)
    assert!(!roll.feed((4, "e+".to_string())));
    assert_eq!(
        roll.queue,
        vec![(2, "c".to_string()), (4, "e+".to_string())]
    );
    // Appending to the currently scrolling line's number is still an in-place refresh
    assert!(!roll.feed((1, "b+".to_string())));
    assert_eq!(roll.displayed, Some((1, "b+".to_string())));
}

#[test]
fn ticker_roll_fire_promotes_next_and_stops_when_drained() {
    let mut roll = TickerRoll::default();
    roll.feed((0, "a".to_string()));
    roll.feed((1, "b".to_string()));
    let generation = roll.generation;
    // A stale timer with mismatched generation is discarded outright
    assert!(!roll.fire(generation + 1, Instant::now()));
    // Empty queue: wrap up (clear the exiting line, leave the hold period), no re-arm
    assert!(!roll.fire(generation, Instant::now()));
    assert!(!roll.rolling);
    assert!(roll.exiting.is_none());
    // With new lines queued: roll in the queue head and re-arm
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
    // Simulate a busy main thread: the timer fires >250ms late (backfills the last scroll time)
    roll.promoted_at = Some(Instant::now() - Duration::from_secs(2));
    let generation = roll.generation;
    assert!(roll.fire(generation, Instant::now()));
    // Skips the middle entry c and plays the latest d directly
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
    // Expand/collapse: reset to the latest line, no exiting line, no queue, no enter animation
    roll.reset_to(ticker_target_line("a\nb\nc"));
    assert_eq!(roll.displayed, Some((2, "c".to_string())));
    assert!(roll.exiting.is_none());
    assert!(roll.queue.is_empty());
    assert!(!roll.rolled_in);
    assert!(!roll.rolling);
    // A timer started before reset does nothing when it fires (generation already invalidated)
    assert!(!roll.fire(generation, Instant::now()));
    assert_eq!(roll.displayed, Some((2, "c".to_string())));
}

/// Horizontal pin-to-tail regression: the vertical-scroll container's content must
/// keep its natural width overflowing the viewport so ScrollHandle can sense
/// horizontal scrollability (pin-to-tail `set_offset(-max_offset)` depends on it).
/// Regression history: text inside the scroll container got its width clamped by
/// layout into the available space (max_offset always 0 → content stuck at the
/// start, reproduced 2026-09-30); the fix is to measure the width explicitly and
/// set it on the container (measure_ticker_width, same as the sidebar marquee).
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
            // Same nesting as render_thinking: width-capped outer + scroll viewport + vertical-scroll container
            let line = "vertical-scroll regression probe".repeat(40);
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
        "ticker line content should overflow the viewport (pin-to-tail depends on max_offset), got {max:?}"
    );
}

#[test]
fn read_output_parsing() {
    use super::{
        is_read_code_output, parse_read_output, read_output_first_line, read_output_line_count,
    };
    // Standard paginated output: numbered lines + truncation note
    let output = "215\tlet base = base.strip_suffix(\".exe\");\n216\t}\n\n[Truncated: showing lines 215-216 of 300; pass an offset to keep reading]";
    let parsed = parse_read_output(output).expect("should parse content");
    assert_eq!(parsed.lines.len(), 2);
    assert_eq!(parsed.lines[0].0, 215);
    assert_eq!(parsed.lines[0].1, "let base = base.strip_suffix(\".exe\");");
    assert_eq!(parsed.lines[1].1, "}");
    assert_eq!(
        parsed.notes,
        vec!["[Truncated: showing lines 215-216 of 300; pass an offset to keep reading]"]
    );
    assert_eq!(read_output_line_count(output), 2);
    assert_eq!(read_output_first_line(output), Some(215));
    assert!(is_read_code_output(output));

    // A content line itself starting with "digits+tab": split_once splits only at the first tab
    let parsed = parse_read_output("7\t100\t200").expect("should parse");
    assert_eq!(parsed.lines, vec![(7, "100\t200".to_string())]);

    // Empty content line (`{no}\t`) and a lossy warning (no blank-line separator)
    let parsed =
        parse_read_output("1\t\n2\tx\n[Warning: decoded output contains replacement characters]")
            .expect("should parse");
    assert_eq!(parsed.lines.len(), 2);
    assert_eq!(parsed.lines[0].1, "");
    assert_eq!(
        parsed.notes,
        vec!["[Warning: decoded output contains replacement characters]"]
    );

    // Non-content output: empty file/unchanged/error → None (falls back to the generic tool card)
    assert!(parse_read_output("(empty file)").is_none());
    assert!(
        parse_read_output("(File unchanged: same Read parameters as last time and identical content, no need to read again)").is_none()
    );
    assert!(parse_read_output("File not found: foo.rs").is_none());
    assert!(!is_read_code_output("(empty file)"));
    assert_eq!(read_output_line_count("(empty file)"), 0);
    assert_eq!(read_output_first_line("(empty file)"), None);
}

/// Horizontal scroll regression: when the Read card is not wrapping, the content
/// column uses the explicit width from measure_max_line_width (without an
/// explicit width, layout clamps it into the available space and horizontal
/// scrolling breaks); this verifies the whole "measure width + set width
/// explicitly → ScrollHandle senses horizontal overflow" chain (structure
/// matches the render_read_card body: x-scroll container > v_flex (explicit
/// width) > code_line_row (nowrap)).
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
            let code = format!(
                "short line\n{}",
                "let x = \"a very long line\"; ".repeat(40)
            );
            let theme = cx.theme().highlight_theme.clone();
            let highlighted = highlight_code(&code, "text", &theme);
            // Same structure as the card: gutter(28) + code cell padding(24) + max line width
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
                                "short line",
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
        "with explicit width measurement the long line should overflow the viewport for horizontal scrolling (max_offset.x), got {max:?}"
    );
}

/// Scroll wheel axis-lock regression: the Read card (x/y two-layer scroll
/// containers) must each respond only to its own axis's wheel delta. gpui by
/// default maps vertical delta onto x-only scrollable containers (y→x) and
/// horizontal delta onto y-only scrollable containers (x→y); without
/// restrict_scroll_to_axis one wheel move scrolls both axes (user-reported
/// 2026-10-05). Structure matches the render_read_card body.
/// Note: window.scroll(id) relies on the observation registry (only elements
/// wrapped by test_support register), so wheel events are dispatched directly
/// at known layout positions via dispatch_event.
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
            let line = "let x = \"a very long line\"; ".repeat(40);
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
    // The card sits in the 200x100 area at the window's top-left; dispatch wheel events at its center
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
        // Vertical wheel (mouse wheel = Lines, not precise): scrolls vertically only; horizontal stays put
        window.dispatch_event(wheel(0., -3.), cx);
        window.render_frame(cx);
    })
    .unwrap();
    assert_eq!(
        h_scroll.offset().x,
        gpui_kit::px(0.),
        "vertical wheel must not drive horizontal scrolling"
    );
    assert!(
        y_scroll.offset().y < gpui_kit::px(0.),
        "vertical wheel should scroll vertically"
    );
    // Horizontal delta (Shift+wheel/trackpad swipe): scrolls horizontally only; vertical stays put
    // (scroll offsets share the y-axis sign convention: scrolling right = delta.x negative, offset.x negative)
    let y_before = y_scroll.offset().y;
    cx.update_window(window.into(), |_, window, cx| {
        window.dispatch_event(wheel(-4., 0.), cx);
        window.render_frame(cx);
    })
    .unwrap();
    assert!(
        h_scroll.offset().x != gpui_kit::px(0.),
        "horizontal delta should scroll horizontally"
    );
    assert_eq!(
        y_scroll.offset().y,
        y_before,
        "horizontal delta must not drive vertical scrolling"
    );
}
/// Scroll chaining regression: the Bash card's subcards use independent scroll
/// handles (they do not go through the shared consume_scroll(body_scroll)
/// fallback in cards.rs); when content is scrollable each must swallow the wheel
/// itself, otherwise it leaks through to the outer message list and both scroll
/// (user-reported 2026-10-05); when content is not scrollable (short command)
/// it must chain through to the list (consistent with other tool cards).
///
/// Notes: (1) wheel hit-testing indirectly reads mouse_position (dispatch_event
/// does not update it for wheels), so dispatch a MouseMove first; (2) the
/// expand/collapse animation plays on real wall-clock time and max_h clipping
/// during it blocks hit-testing — sleep past the animation duration after
/// expanding before testing.
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
    // Scenario: the Bash card first (top, avoiding post-scroll geometry math) plus
    // 30 user messages to stretch the outer list into scrollability (the command
    // is 1 line → command subcard not scrollable; the output is 40 lines → output
    // subcard scrollable)
    window
        .update(cx, |probe, _, cx| {
            probe.thread.update(cx, |view, cx| {
                view.append_user_message("run a command first".to_string(), vec![], vec![], cx);
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
                            .map(|i| format!("output line {i}"))
                            .collect::<Vec<_>>()
                            .join("\n"),
                        is_error: false,
                        edit: None,
                    },
                    cx,
                );
                for ix in 0..30 {
                    view.append_user_message(format!("message {ix}"), vec![], vec![], cx);
                }
                assert!(
                    view.debug_expand_tool("Bash", cx),
                    "a Bash card should exist to expand"
                );
            });
        })
        .unwrap();
    // Restore the list to top (card at top): during append every message forces
    // follow-bottom (including the deferred scroll flag), so render one frame to
    // consume the flag, then explicitly zero the offset
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
    // Render one frame (bash_ui created + heights measured), then wait out the
    // animation (the expand/collapse animation runs on real wall clock; if not
    // finished, max_h clipping removes the hit area)
    cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
    // Wheel hit-testing indirectly reads mouse_position: move the pointer there
    // first (dispatch_event updates mouse_position only for MouseMove/Down/Up,
    // not wheel events)
    let mouse_move = |position| {
        gpui_kit::MouseMoveEvent {
            position,
            ..Default::default()
        }
        .to_platform_input()
    };
    // dy: negative = scroll down (offset goes negative), positive = scroll up
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
                } => Some((
                    ui.out.v_scroll.offset().y,
                    ui.cmd.v_scroll.bounds().center(),
                )),
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
    let (_, cmd_center) = out_state.expect("Bash card UI state should be created");
    assert!(
        outer_max > gpui_kit::px(0.),
        "outer message list should be scrollable"
    );

    // Output subcard center (should be inside the viewport by now): use the center of the card bounds
    let out_center = window
        .update(cx, |probe, _, cx| {
            let view = probe.thread.read(cx);
            view.messages
                .iter()
                .flat_map(|m| &m.segments)
                .find_map(|s| match s {
                    super::Segment::ToolCall {
                        bash_ui: Some(ui), ..
                    } => Some(ui.out.v_scroll.bounds().center()),
                    _ => None,
                })
                .expect("Bash card UI state should be created")
        })
        .unwrap();
    let list_bounds = window
        .update(cx, |probe, _, cx| {
            probe.thread.read(cx).scroll_handle.bounds()
        })
        .unwrap();
    assert!(
        out_center.y > list_bounds.top() && out_center.y < list_bounds.bottom(),
        "output subcard should be inside the viewport: {out_center:?} vs {list_bounds:?}"
    );

    // Scroll the output subcard (scrollable): scrolls inside the card; the outer list stays put (no leak-through)
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
    assert!(
        out_after < gpui_kit::px(0.),
        "output subcard should have scrolled: {out_after:?}"
    );
    assert_eq!(
        outer_after, outer_before,
        "a scrollable card must not leak the wheel to the outer message list"
    );

    // Scroll the command subcard (1 line, not scrollable): chains through to the outer list (consistent with other tool cards)
    cx.update_window(window.into(), |_, window, cx| {
        window.dispatch_event(mouse_move(cmd_center), cx);
        // The outer list is at the top; only scrolling down (negative dy) yields observable motion
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
        "a non-scrollable card should chain the wheel to the outer list ({outer_after:?} → scrolled down {outer_chained:?})"
    );
}

/// Generic tool box: a tool without a dedicated card (TodoList) expands into a
/// plain rounded box (no header/buttons) showing the output — monospace,
/// height-capped at GENERIC_BOX_MAX_H and internally scrollable via the
/// segment's shared body_scroll
#[gpui_kit::test]
fn generic_box_renders_and_scrolls(cx: &mut gpui_kit::TestAppContext) {
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
        gpui_kit::size(gpui_kit::px(600.), gpui_kit::px(400.)),
        |_, cx| {
            let thread = cx.new(super::ThreadView::new);
            Probe { thread }
        },
    );
    window
        .update(cx, |probe, _, cx| {
            probe.thread.update(cx, |view, cx| {
                view.append_user_message("update the todos".to_string(), vec![], vec![], cx);
                view.reduce_event(
                    pig_protocol::Event::ToolCallBegin {
                        session_id: "s".into(),
                        seq: 0,
                        item_id: "g1".into(),
                        tool: "TodoList".into(),
                        input_summary: String::new(),
                        detail: String::new(),
                    },
                    cx,
                );
                view.reduce_event(
                    pig_protocol::Event::ToolCallEnd {
                        session_id: "s".into(),
                        seq: 1,
                        item_id: "g1".into(),
                        output: (1..=40)
                            .map(|i| format!("{i}. task item"))
                            .collect::<Vec<_>>()
                            .join("\n"),
                        is_error: false,
                        edit: None,
                    },
                    cx,
                );
                assert!(
                    view.debug_expand_tool("TodoList", cx),
                    "a TodoList card should exist to expand"
                );
            });
        })
        .unwrap();
    // Render one frame, then wait out the expand animation and render again
    // (same timing concern as the Bash card test: the animation runs on wall
    // clock and max_h clipping mid-animation shrinks the viewport)
    cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
    let (painted, max_offset) = window
        .update(cx, |probe, _, cx| {
            let view = probe.thread.read(cx);
            view.messages
                .iter()
                .flat_map(|m| &m.segments)
                .find_map(|s| match s {
                    super::Segment::ToolCall {
                        tool, body_scroll, ..
                    } if tool == "TodoList" => Some((
                        body_scroll.bounds().size.width > gpui_kit::px(0.),
                        body_scroll.max_offset().y,
                    )),
                    _ => None,
                })
                .expect("a TodoList tool call segment should exist")
        })
        .unwrap();
    assert!(painted, "the generic box body should have painted");
    assert!(
        max_offset > gpui_kit::px(0.),
        "the 40-line output should overflow the box cap and scroll internally"
    );
}

/// The full input is repeated inside the generic box only when the collapsed
/// summary row cannot show it: multi-line inputs (the row collapses them to
/// one line) or long ones (estimated width past the row; CJK counts double)
#[test]
fn full_input_repeated_only_when_row_cannot_show_it() {
    use super::cards::show_full_input;
    assert!(!show_full_input(""));
    assert!(!show_full_input("*/package.json"));
    assert!(!show_full_input("查找文件|终端"));
    // Multi-line input
    assert!(show_full_input("first line\nsecond line"));
    // Long single-line input (81 ASCII chars > 80 columns)
    assert!(show_full_input(&"a".repeat(81)));
    assert!(!show_full_input(&"a".repeat(80)));
    // CJK counts double: 41 CJK chars = 82 columns
    assert!(show_full_input(&"文".repeat(41)));
    assert!(!show_full_input(&"文".repeat(40)));
}

/// Glob output parsing (search_card.rs): path rows from the first
/// blank-line section, bracketed/parenthesized footers as notes; no result
/// row → None (the generic box takes over)
#[test]
fn glob_output_parses_rows_and_notes() {
    use super::search_card::parse_glob_output;
    let parsed =
        parse_glob_output("crates/a.rs\ncrates/b.rs\n\n[Showing 1-2; continue with offset=2]")
            .expect("two path rows");
    assert_eq!(parsed.rows.len(), 2);
    assert_eq!(parsed.rows[0].path, "crates/a.rs");
    assert_eq!(parsed.rows[0].line, None);
    assert_eq!(parsed.rows[0].content, None);
    assert_eq!(parsed.notes, vec!["[Showing 1-2; continue with offset=2]"]);
    assert!(parse_glob_output("(no matching files)").is_none());
}

/// Grep output parsing (search_card.rs): content rows split at the first
/// colon followed by digits + ": " (drive letters and colons inside the path
/// stay intact), `--` separators dropped, footers as notes; files_with_matches
/// bare paths and count rows become path-only rows
#[test]
fn grep_output_parses_rows_and_notes() {
    use super::search_card::parse_grep_output;
    let parsed = parse_grep_output(
        "src/a.rs:10: use super::*;\nsrc/a.rs:11: use std::io;\n--\nsrc/b.rs:3: let x: u32 = 1;\n[Skipped: binary/undecodable 1]",
    )
    .expect("three content rows");
    assert_eq!(parsed.rows.len(), 3);
    assert_eq!(parsed.rows[0].path, "src/a.rs");
    assert_eq!(parsed.rows[0].line, Some(10));
    assert_eq!(parsed.rows[0].content.as_deref(), Some("use super::*;"));
    // Content containing ": " with digits stays intact (first-match split)
    assert_eq!(parsed.rows[2].path, "src/b.rs");
    assert_eq!(parsed.rows[2].line, Some(3));
    assert_eq!(parsed.rows[2].content.as_deref(), Some("let x: u32 = 1;"));
    assert_eq!(parsed.notes, vec!["[Skipped: binary/undecodable 1]"]);
    // Windows absolute path: the drive-letter colon does not split
    let parsed = parse_grep_output("C:/work/a.rs:7: fn main() {}").expect("one row");
    assert_eq!(parsed.rows[0].path, "C:/work/a.rs");
    assert_eq!(parsed.rows[0].line, Some(7));
    // files_with_matches: bare paths
    let parsed = parse_grep_output("src/a.rs\nsrc/b.rs").expect("two path rows");
    assert_eq!(parsed.rows[1].path, "src/b.rs");
    assert_eq!(parsed.rows[1].line, None);
    // Count mode: `{path}:{count}` opens the file and shows the count
    let parsed = parse_grep_output("src/a.rs:3").expect("count row");
    assert_eq!(parsed.rows[0].path, "src/a.rs");
    assert_eq!(parsed.rows[0].content.as_deref(), Some("3"));
    assert!(parse_grep_output("(no matches)").is_none());
}

/// Grep result list: rows render and clicking one emits OpenFile with the
/// row's path and line number (the right-side file panel scrolls to and
/// highlights that line)
#[gpui_kit::test]
fn grep_result_row_click_opens_file_at_line(cx: &mut gpui_kit::TestAppContext) {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AppContext as _, IntoElement as _};
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
            self.thread.clone().into_any_element()
        }
    }

    let window = cx.open_window(
        gpui_kit::size(gpui_kit::px(800.), gpui_kit::px(400.)),
        |_, cx| {
            let thread = cx.new(super::ThreadView::new);
            Probe { thread }
        },
    );
    // Event capture (the subscription must live until the test ends)
    let captured = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let sink = captured.clone();
    let mut events_sub = None;
    window
        .update(cx, |probe, window, cx| {
            probe.thread.update(cx, |view, cx| {
                let entity = cx.entity();
                events_sub = Some(cx.subscribe_in(
                    &entity,
                    window,
                    move |_, _, event: &super::ThreadEvent, _, _| {
                        sink.borrow_mut().push(event.clone());
                    },
                ));
                view.append_user_message("find the callers".to_string(), vec![], vec![], cx);
                view.reduce_event(
                    pig_protocol::Event::ToolCallBegin {
                        session_id: "s".into(),
                        seq: 0,
                        item_id: "g1".into(),
                        tool: "Grep".into(),
                        input_summary: "render_title_scroll".into(),
                        detail: String::new(),
                    },
                    cx,
                );
                view.reduce_event(
                    pig_protocol::Event::ToolCallEnd {
                        session_id: "s".into(),
                        seq: 1,
                        item_id: "g1".into(),
                        output: "src/sidebar.rs:88: fn render_title_scroll(\nsrc/main.rs:166: render_title_scroll(x)".to_string(),
                        is_error: false,
                        edit: None,
                    },
                    cx,
                );
                assert!(view.debug_expand_tool("Grep", cx));
            });
        })
        .unwrap();
    let _events_sub = events_sub;
    // Wait out the expand animation (hit-testing while max_h animates is
    // unreliable, same as the Bash card test)
    cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();

    // Row 1 (second result): the segment sits at message 1 / segment 0, so the
    // row id is ("search-row", (1024 + 0) * 512 + 1)
    let row_id = ("search-row", 1024usize * 512 + 1);
    cx.update_window(window.into(), |_, window, _| {
        let snap = window.find(row_id);
        assert!(snap.visible(), "the second result row should be visible");
    })
    .unwrap();
    // Hover drives the blue text color via the search_row_hover state
    cx.update_window(window.into(), |_, window, cx| {
        window.hover(row_id, cx);
    })
    .unwrap();
    let hover_state = window
        .update(cx, |probe, _, cx| probe.thread.read(cx).search_row_hover)
        .unwrap();
    assert_eq!(
        hover_state,
        Some((1, 0, 1)),
        "hovering the row should record it in search_row_hover"
    );
    cx.update_window(window.into(), |_, window, cx| {
        window.click(row_id, cx);
    })
    .unwrap();
    let events = captured.borrow();
    assert_eq!(
        events.len(),
        1,
        "OpenFile should be emitted exactly once: {}",
        events.len()
    );
    match &events[0] {
        super::ThreadEvent::OpenFile { path, line } => {
            assert_eq!(path, "src/main.rs");
            assert_eq!(*line, Some(166));
        }
        other => panic!("expected an OpenFile event: {other:?}"),
    }
}

/// Compact divider: in-progress "compacting context" → done "context compacted (Nk → Mk tokens)"
/// with a "view summary" link (click → OpenCompactSummary carrying the bare summary);
/// the full summary stays in text.
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
    // Event capture (the subscription must live until the test ends)
    let captured = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let sink = captured.clone();
    let mut events_sub = None;
    // In progress: set_compacting(true) → the progress divider appears and fills the content column (the divider's flex_grow works)
    window
        .update(cx, |probe, window, cx| {
            probe.thread.update(cx, |view, cx| {
                let entity = cx.entity();
                events_sub = Some(cx.subscribe_in(
                    &entity,
                    window,
                    move |_, _, event: &super::ThreadEvent, _, _| {
                        sink.borrow_mut().push(event.clone());
                    },
                ));
                view.append_user_message("tidy up this file".to_string(), vec![], vec![], cx);
                view.set_compacting(true, cx);
                assert!(view.debug_compacting());
            });
        })
        .unwrap();
    let _events_sub = events_sub;
    cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
    cx.update_window(window.into(), |_, window, _| {
        let snap = window.find("compacting-divider");
        assert!(snap.visible(), "in-progress divider should be visible");
        assert!(
            snap.bounds().size.width > gpui_kit::px(600.),
            "divider should span the full content column (divider line grow): {:?}",
            snap.bounds()
        );
    })
    .unwrap();

    // Done: the progress divider disappears, the "context compacted" divider appears; the full summary stays in message text for assertions
    window
        .update(cx, |probe, _, cx| {
            probe.thread.update(cx, |view, cx| {
                view.set_compacting(false, cx);
                view.add_compact_note(
                    "[前文已压缩·模型摘要] 省略 9 条消息。\n\n摘要正文",
                    Some(391_000),
                    Some(41_600),
                    Some("摘要正文".to_string()),
                    cx,
                );
                assert!(!view.debug_compacting());
            });
        })
        .unwrap();
    cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
    cx.update_window(window.into(), |_, window, _| {
        assert!(
            window.try_find("compacting-divider").is_none(),
            "in-progress divider should disappear once compacting is done"
        );
        // The user message takes index 0; the compact divider is index 1
        let snap = window
            .try_find(("compact-note", 1usize))
            .expect("the 'context compacted' ('上下文已压缩') divider should appear");
        assert!(snap.visible());
        // The "view summary" link sits inside the divider
        let link = window
            .try_find(("compact-summary-link", 1usize))
            .expect("the 'view summary' link should appear inside the divider");
        assert!(link.visible());
    })
    .unwrap();
    // Clicking the link emits OpenCompactSummary with the bare summary (not the full note)
    cx.update_window(window.into(), |_, window, cx| {
        window.click(("compact-summary-link", 1usize), cx);
    })
    .unwrap();
    {
        let events = captured.borrow();
        assert_eq!(
            events.len(),
            1,
            "OpenCompactSummary should be emitted exactly once: {}",
            events.len()
        );
        match &events[0] {
            super::ThreadEvent::OpenCompactSummary { text } => {
                assert_eq!(text, "摘要正文");
            }
            other => panic!("expected an OpenCompactSummary event: {other:?}"),
        }
    }
    window
        .update(cx, |probe, _, cx| {
            probe.thread.update(cx, |view, _| {
                let notes = view.debug_system_notes();
                assert!(
                    notes
                        .iter()
                        .any(|n| n.contains("模型摘要") && n.contains("摘要正文")),
                    "full summary should stay in the system note text: {notes:?}"
                );
            });
        })
        .unwrap();

    // Abort fallback: when compacting is interrupted (TurnAborted) the flag must be cleared, leaving no progress divider behind
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
                assert!(
                    !view.debug_compacting(),
                    "TurnAborted should clear the compacting flag"
                );
            });
        })
        .unwrap();
}

/// Turn work row ("worked N s ›"): after TurnComplete, tool cards and thinking
/// blocks collapse into the row; clicking the whole row expands (content grows)
/// then collapses again; aborted turns land on Stopped. Duration formatting
/// (milliseconds → seconds, nearest rounding, minimum 1 s) lives in
/// render_work_row; the pure formatting part is pinned directly on
/// fmt_work_duration
#[gpui_kit::test]
fn work_row_collapses_on_turn_complete(cx: &mut gpui_kit::TestAppContext) {
    use gpui_kit::AppContext as _;
    use gpui_kit::test::TestWindowExt as _;
    cx.update(gpui_kit::init);

    assert_eq!(super::fmt_work_duration(10, "已工作"), "已工作 10 秒");
    assert_eq!(super::fmt_work_duration(102, "已工作"), "已工作 1 分 42 秒");
    assert_eq!(super::fmt_work_duration(0, "工作中"), "工作中 0 秒");

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

    // Full turn: user message ix 0, assistant message ix 1, tool card segment six 0
    window
        .update(cx, |probe, _, cx| {
            probe.thread.update(cx, |view, cx| {
                view.append_user_message("run a command".to_string(), vec![], vec![], cx);
                view.reduce_event(
                    pig_protocol::Event::TurnStarted {
                        session_id: "s".into(),
                        seq: 0,
                        turn_id: "t1".into(),
                    },
                    cx,
                );
                view.reduce_event(
                    pig_protocol::Event::ToolCallBegin {
                        session_id: "s".into(),
                        seq: 1,
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
                        seq: 2,
                        item_id: "b1".into(),
                        output: (1..=40)
                            .map(|i| format!("output line {i}"))
                            .collect::<Vec<_>>()
                            .join("\n"),
                        is_error: false,
                        edit: None,
                    },
                    cx,
                );
                view.reduce_event(
                    pig_protocol::Event::TurnComplete {
                        session_id: "s".into(),
                        seq: 3,
                        duration_ms: 10_400,
                        stats: None,
                    },
                    cx,
                );
                let message = &view.messages[1];
                assert!(
                    matches!(
                        message.work_state,
                        Some(super::WorkState::Completed {
                            duration: Some(d)
                        }) if d == std::time::Duration::from_millis(10_400)
                    ),
                    "turn completion should settle the work row (real duration)"
                );
                assert!(
                    !message.work_open,
                    "work row should be collapsed by default"
                );
            });
        })
        .unwrap();

    cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
    cx.update_window(window.into(), |_, window, _| {
        assert!(
            window.find(("work-row", 1usize)).visible(),
            "collapsed row should be visible"
        );
        assert!(
            window.try_find(("tool", 1024usize)).is_none(),
            "tool cards should not render while collapsed"
        );
    })
    .unwrap();

    // Click the collapsed row → expand: the tool card returns to the message flow
    cx.update_window(window.into(), |_, window, cx| {
        window.click(("work-row", 1usize), cx);
    })
    .unwrap();
    cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
    cx.update_window(window.into(), |_, window, _| {
        assert!(
            window.try_find(("tool", 1024usize)).is_some(),
            "after expanding, the tool card should return to the message flow"
        );
    })
    .unwrap();
    window
        .update(cx, |probe, _, cx| {
            assert!(
                probe.thread.read(cx).messages[1].work_open,
                "work row should expand after the click"
            );
        })
        .unwrap();

    // Click again → collapse; the tool card disappears again
    cx.update_window(window.into(), |_, window, cx| {
        window.click(("work-row", 1usize), cx);
    })
    .unwrap();
    cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
    cx.update_window(window.into(), |_, window, _| {
        assert!(
            window.try_find(("tool", 1024usize)).is_none(),
            "collapsing again should hide the tool card"
        );
    })
    .unwrap();

    // The aborted turn lands on Stopped ("stopped")
    window
        .update(cx, |probe, _, cx| {
            probe.thread.update(cx, |view, cx| {
                view.reduce_event(
                    pig_protocol::Event::TurnStarted {
                        session_id: "s".into(),
                        seq: 4,
                        turn_id: "t2".into(),
                    },
                    cx,
                );
                view.reduce_event(
                    pig_protocol::Event::ToolCallBegin {
                        session_id: "s".into(),
                        seq: 5,
                        item_id: "b2".into(),
                        tool: "Bash".into(),
                        input_summary: "sleep 99".into(),
                        detail: String::new(),
                    },
                    cx,
                );
                view.reduce_event(
                    pig_protocol::Event::TurnAborted {
                        session_id: "s".into(),
                        seq: 6,
                    },
                    cx,
                );
                let message = &view.messages[2];
                assert!(
                    matches!(message.work_state, Some(super::WorkState::Stopped)),
                    "aborted turn should land on Stopped"
                );
            });
        })
        .unwrap();
}

/// @mention inline chip: renders visibly (icon + underlined file name); click emits OpenFile to open the file
#[gpui_kit::test]
fn user_message_mention_chip_renders_and_opens_file(cx: &mut gpui_kit::TestAppContext) {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AppContext as _, IntoElement as _};
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
            self.thread.clone().into_any_element()
        }
    }

    let window = cx.open_window(
        gpui_kit::size(gpui_kit::px(800.), gpui_kit::px(400.)),
        |_, cx| {
            let thread = cx.new(super::ThreadView::new);
            Probe { thread }
        },
    );
    // Event capture (the subscription must live until the test ends)
    let captured = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let sink = captured.clone();
    let mut events_sub = None;
    window
        .update(cx, |probe, window, cx| {
            probe.thread.update(cx, |view, cx| {
                let entity = cx.entity();
                events_sub = Some(cx.subscribe_in(
                    &entity,
                    window,
                    move |_, _, event: &super::ThreadEvent, _, _| {
                        sink.borrow_mut().push(event.clone());
                    },
                ));
                view.append_user_message(
                    "@src/a.rs take a look at this file".to_string(),
                    vec!["src/a.rs".to_string()],
                    vec![],
                    cx,
                );
            });
        })
        .unwrap();
    let _events_sub = events_sub;
    cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();

    // The chip really renders visibly (message ix=0, Mention segment six=0)
    cx.update_window(window.into(), |_, window, _| {
        let snap = window.find(("user-mention", 0usize));
        assert!(snap.visible(), "@chip should be visible");
    })
    .unwrap();

    // Click the chip → OpenFile{path, line: None}
    cx.update_window(window.into(), |_, window, cx| {
        window.click(("user-mention", 0usize), cx);
    })
    .unwrap();
    let events = captured.borrow();
    assert_eq!(
        events.len(),
        1,
        "OpenFile should be emitted exactly once: {}",
        events.len()
    );
    match &events[0] {
        super::ThreadEvent::OpenFile { path, line } => {
            assert_eq!(path, "src/a.rs");
            assert_eq!(*line, None);
        }
        other => panic!("expected an OpenFile event: {other:?}"),
    }
}

/// Message actions row (same as ZCode's assistant actions row): rendered after
/// the turn ends (appears on hover); the fork event's turns = the target
/// message's turn ordinal; copy sets copied; a running turn renders no row at all
#[gpui_kit::test]
fn message_actions_copy_and_fork(cx: &mut gpui_kit::TestAppContext) {
    use gpui_kit::AppContext as _;
    use gpui_kit::test::TestWindowExt as _;
    use std::cell::RefCell;
    use std::rc::Rc;
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
    let captured: Rc<RefCell<Vec<super::ThreadEvent>>> = Rc::new(RefCell::new(vec![]));
    let sink = captured.clone();
    let mut events_sub = None;
    window
        .update(cx, |probe, window, cx| {
            probe.thread.update(cx, |view, cx| {
                let entity = cx.entity();
                events_sub = Some(cx.subscribe_in(
                    &entity,
                    window,
                    move |_, _, event: &super::ThreadEvent, _, _| {
                        sink.borrow_mut().push(event.clone());
                    },
                ));
                // Two complete turns: messages [U0, A1, U2, A3]
                for (turn, ask, answer) in [
                    ("t1", "question one", "answer one"),
                    ("t2", "question two", "answer two"),
                ] {
                    view.append_user_message(ask.to_string(), vec![], vec![], cx);
                    view.reduce_event(
                        pig_protocol::Event::TurnStarted {
                            session_id: "s".into(),
                            seq: 0,
                            turn_id: turn.into(),
                        },
                        cx,
                    );
                    view.reduce_event(
                        pig_protocol::Event::TextDone {
                            session_id: "s".into(),
                            seq: 1,
                            item_id: format!("{turn}-md"),
                            full_text: answer.to_string(),
                        },
                        cx,
                    );
                    view.reduce_event(
                        pig_protocol::Event::TurnComplete {
                            session_id: "s".into(),
                            seq: 2,
                            duration_ms: 1000,
                            stats: None,
                        },
                        cx,
                    );
                }
            });
        })
        .unwrap();
    let _events_sub = events_sub;
    cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();

    // Both turns' action rows exist (transparent but clickable)
    cx.update_window(window.into(), |_, window, _| {
        assert!(
            window.try_find(("msg-fork", 1usize)).is_some(),
            "the first turn should have a fork button"
        );
        assert!(
            window.try_find(("msg-copy", 1usize)).is_some(),
            "the first turn should have a copy button"
        );
        assert!(
            window.try_find(("msg-fork", 3usize)).is_some(),
            "the second turn should have a fork button"
        );
    })
    .unwrap();

    // Fork: turns = the target message's turn ordinal
    cx.update_window(window.into(), |_, window, cx| {
        window.click(("msg-fork", 1usize), cx);
    })
    .unwrap();
    cx.update_window(window.into(), |_, window, cx| {
        window.click(("msg-fork", 3usize), cx);
    })
    .unwrap();
    {
        let events = captured.borrow();
        let forks: Vec<usize> = events
            .iter()
            .filter_map(|e| match e {
                super::ThreadEvent::Fork { turns } => Some(*turns),
                _ => None,
            })
            .collect();
        assert_eq!(forks, vec![1, 2], "forked turn ordinals: {forks:?}");
    }

    // Copy: sets copied (checkmark feedback) and joins all Markdown segments
    cx.update_window(window.into(), |_, window, cx| {
        window.click(("msg-copy", 1usize), cx);
    })
    .unwrap();
    window
        .update(cx, |probe, _, cx| {
            assert!(
                probe.thread.read(cx).messages[1].copied,
                "copied should be set after copying"
            );
        })
        .unwrap();
    // The checkmark bounces back after 1.2s (the rebound timer runs on the test scheduler's fake clock; real sleep does not advance it)
    cx.dispatcher
        .advance_clock(std::time::Duration::from_millis(1300));
    cx.run_until_parked();
    window
        .update(cx, |probe, _, cx| {
            assert!(
                !probe.thread.read(cx).messages[1].copied,
                "checkmark should bounce back after 1.2s"
            );
        })
        .unwrap();

    // Running turn: the actions row is not rendered at all
    window
        .update(cx, |probe, _, cx| {
            probe.thread.update(cx, |view, cx| {
                view.reduce_event(
                    pig_protocol::Event::TurnStarted {
                        session_id: "s".into(),
                        seq: 3,
                        turn_id: "t3".into(),
                    },
                    cx,
                );
            });
        })
        .unwrap();
    cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
    cx.update_window(window.into(), |_, window, _| {
        assert!(
            window.try_find(("msg-actions", 4usize)).is_none(),
            "a running turn should not render the actions row"
        );
        // Finished turns are unaffected by the new turn
        assert!(window.try_find(("msg-fork", 3usize)).is_some());
    })
    .unwrap();
}

/// ExitPlanMode plan card (same as kimi's "plan pending/approved"): ToolCallBegin
/// creates the card showing "pending"; the chevron expands to the full plan text;
/// the decision plus ToolCallEnd settles the tri-state; the replay form
/// (Begin+End arriving back to back) shows the result directly
#[gpui_kit::test]
fn plan_row_states_and_expand(cx: &mut gpui_kit::TestAppContext) {
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

    // Live form: TurnStarted → ExitPlanMode ToolCallBegin (detail = args JSON) → ApprovalRequested
    window
        .update(cx, |probe, _, cx| {
            probe.thread.update(cx, |view, cx| {
                view.append_user_message("make a plan".to_string(), vec![], vec![], cx);
                view.reduce_event(
                    pig_protocol::Event::TurnStarted {
                        session_id: "s".into(),
                        seq: 0,
                        turn_id: "t1".into(),
                    },
                    cx,
                );
                view.reduce_event(
                    pig_protocol::Event::ToolCallBegin {
                        session_id: "s".into(),
                        seq: 1,
                        item_id: "pe1".into(),
                        tool: "ExitPlanMode".into(),
                        input_summary: "request to exit plan mode".into(),
                        detail: serde_json::json!({"plan": "# Implementation plan\n\n1. Step one\n2. Step two"})
                            .to_string(),
                    },
                    cx,
                );
                view.reduce_event(
                    pig_protocol::Event::ApprovalRequested {
                        session_id: "s".into(),
                        seq: 2,
                        request_id: "req-pe1".into(),
                        tool: "ExitPlanMode".into(),
                        detail: "# Implementation plan\n\n1. Step one\n2. Step two".into(),
                        danger_key: None,
                    },
                    cx,
                );
                let message = &view.messages[1];
                assert!(
                    matches!(
                        message.segments.first(),
                        Some(super::Segment::Plan { done: false, .. })
                    ),
                    "ExitPlanMode should create a Plan segment, not a tool-card segment"
                );
            });
        })
        .unwrap();
    cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();

    // The "plan · pending" row is visible; after the chevron expands, the full plan text is visible
    cx.update_window(window.into(), |_, window, _| {
        assert!(
            window.find(("plan-row", 1024usize)).visible(),
            "plan row should be visible"
        );
        assert!(
            window.try_find(("plan-row-body", 1024usize)).is_none(),
            "collapsed by default, no expanded body"
        );
    })
    .unwrap();
    cx.update_window(window.into(), |_, window, cx| {
        window.click(("plan-row", 1024usize), cx);
    })
    .unwrap();
    cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
    cx.update_window(window.into(), |_, window, _| {
        assert!(
            window.try_find(("plan-row-body", 1024usize)).is_some(),
            "clicking should expand the full plan text"
        );
    })
    .unwrap();

    // Approve + ToolCallEnd → "approved"
    window
        .update(cx, |probe, _, cx| {
            probe.thread.update(cx, |view, cx| {
                view.decide_approval_by_id(
                    "req-pe1",
                    pig_protocol::ApprovalDecision::Allow,
                    None,
                    cx,
                );
                view.reduce_event(
                    pig_protocol::Event::ToolCallEnd {
                        session_id: "s".into(),
                        seq: 3,
                        item_id: "pe1".into(),
                        output: "Plan approved; plan mode is now off. Start executing the plan."
                            .into(),
                        is_error: false,
                        edit: None,
                    },
                    cx,
                );
                let Some(super::Segment::Plan {
                    done,
                    approved,
                    open,
                    ..
                }) = view.messages[1].segments.first()
                else {
                    panic!("expected a Plan segment");
                };
                assert!(
                    *done && *approved,
                    "approval should settle on '已通过' (approved)"
                );
                assert!(*open, "expanded state should be preserved");
            });
        })
        .unwrap();

    // Replay form: Begin+End arriving back to back (output contains "user declined") → "declined" directly
    window
        .update(cx, |probe, _, cx| {
            probe.thread.update(cx, |view, cx| {
                view.reduce_event(
                    pig_protocol::Event::TurnStarted {
                        session_id: "s".into(),
                        seq: 4,
                        turn_id: "replay-2".into(),
                    },
                    cx,
                );
                view.reduce_event(
                    pig_protocol::Event::ToolCallBegin {
                        session_id: "s".into(),
                        seq: 5,
                        item_id: "replay-2-tool-1".into(),
                        tool: "ExitPlanMode".into(),
                        input_summary: "request to exit plan mode".into(),
                        detail: serde_json::json!({"plan": "# Old plan"}).to_string(),
                    },
                    cx,
                );
                view.reduce_event(
                    pig_protocol::Event::ToolCallEnd {
                        session_id: "s".into(),
                        seq: 6,
                        item_id: "replay-2-tool-1".into(),
                        output: "The user declined to exit plan mode. Continue refining the plan or answer open questions.".into(),
                        is_error: true,
                        edit: None,
                    },
                    cx,
                );
                let message = view.messages.last().expect("replay message");
                assert!(
                    matches!(
                        message.segments.first(),
                        Some(super::Segment::Plan {
                            done: true,
                            approved: false,
                            ..
                        })
                    ),
                    "replay should land directly on '已拒绝' (declined)"
                );
            });
        })
        .unwrap();
}
