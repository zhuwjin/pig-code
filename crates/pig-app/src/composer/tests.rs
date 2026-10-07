//! Headless tests for keyboard navigation and staged commands in the / and @ popups.
//! Key presses go through the real dispatch chain: keystroke → app bindings registered
//! on the Input context (overtaking the composer's native bindings) → actions bubble
//! to the Composer root's on_action.
//! Note: this file must import explicitly, as `use super::*` pulls in gpui's `test`
//! macro, shadowing the built-in #[test] and expanding into infinite recursion (the
//! same trap hit in dock.rs).

use super::{Composer, ComposerEvent, Popup};
use crate::{ComposerNavDown, ComposerNavNext, ComposerNavPrev, ComposerNavUp, ComposerPopupClose};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AppContext as _, KeyBinding};
use std::cell::RefCell;
use std::rc::Rc;

/// The same 5 Input context bindings as main.rs (registered after gpui_kit::init in
/// tests; at equal depth the later registration wins, replicating production wiring)
fn bind_popup_keys(cx: &mut gpui_kit::TestAppContext) {
    cx.update(|cx| {
        cx.bind_keys([
            KeyBinding::new("up", ComposerNavUp, Some("Input")),
            KeyBinding::new("down", ComposerNavDown, Some("Input")),
            KeyBinding::new("tab", ComposerNavNext, Some("Input")),
            KeyBinding::new("shift-tab", ComposerNavPrev, Some("Input")),
            KeyBinding::new("escape", ComposerPopupClose, Some("Input")),
        ]);
    });
}

struct Probe {
    composer: gpui_kit::Entity<Composer>,
}

impl gpui_kit::Render for Probe {
    fn render(
        &mut self,
        _window: &mut gpui_kit::Window,
        _cx: &mut gpui_kit::Context<Self>,
    ) -> impl gpui_kit::IntoElement {
        use gpui_kit::IntoElement as _;
        self.composer.clone().into_any_element()
    }
}

fn open_composer(cx: &mut gpui_kit::TestAppContext) -> gpui_kit::WindowHandle<Probe> {
    cx.open_window(
        gpui_kit::size(gpui_kit::px(800.), gpui_kit::px(300.)),
        |window, cx| {
            let composer = cx.new(|cx| Composer::new(window, cx));
            Probe { composer }
        },
    )
}

/// Captures Composer events (asserting "staging sends nothing / dispatches by
/// command when sent") and puts focus into the composer.
/// The returned Subscription must live until the test ends (dropping unsubscribes)
fn capture_events(
    window: &gpui_kit::WindowHandle<Probe>,
    cx: &mut gpui_kit::TestAppContext,
) -> (Rc<RefCell<Vec<ComposerEvent>>>, gpui_kit::Subscription) {
    let captured = Rc::new(RefCell::new(Vec::new()));
    let sink = captured.clone();
    let mut subscription = None;
    window
        .update(cx, |probe, window, cx| {
            probe.composer.update(cx, |_, cx| {
                let entity = cx.entity();
                subscription = Some(cx.subscribe_in(
                    &entity,
                    window,
                    move |_, _, event: &ComposerEvent, _, _| {
                        sink.borrow_mut().push(event.clone());
                    },
                ));
            });
            probe.composer.update(cx, |this, cx| {
                this.input.update(cx, |input, cx| input.focus(window, cx));
            });
        })
        .unwrap();
    (captured, subscription.expect("subscription"))
}

/// TestWindowExt implements this for Window: get &mut Window via cx.update_window, then dispatch
fn press(cx: &mut gpui_kit::TestAppContext, window: &gpui_kit::WindowHandle<Probe>, key: &str) {
    cx.update_window((*window).into(), |_, window, cx| window.press(key, cx))
        .expect("window alive");
}

fn type_text(
    cx: &mut gpui_kit::TestAppContext,
    window: &gpui_kit::WindowHandle<Probe>,
    text: &str,
) {
    cx.update_window((*window).into(), |_, window, cx| window.input(text, cx))
        .expect("window alive");
}

fn read(
    cx: &mut gpui_kit::TestAppContext,
    window: &gpui_kit::WindowHandle<Probe>,
) -> (Option<(Popup, usize)>, usize, String, usize) {
    window
        .update(cx, |probe, _, cx| {
            let input = probe.composer.read(cx).input.read(cx);
            (
                probe.composer.read(cx).popup,
                probe.composer.read(cx).popup_sel,
                input.value().to_string(),
                input.tokens().len(),
            )
        })
        .unwrap()
}

/// Slash popup: Tab/arrow keys cycle → Enter stages as a chip (nothing sent) → keep
/// typing → Enter dispatches by command
#[gpui_kit::test]
fn slash_popup_nav_stages_then_dispatches(cx: &mut gpui_kit::TestAppContext) {
    cx.update(gpui_kit::init);
    bind_popup_keys(cx);
    let window = open_composer(cx);
    let (events, _events_sub) = capture_events(&window, cx);

    // Type / to open the popup (first item /clear selected by default)
    type_text(cx, &window, "/");
    let (popup, sel, _, _) = read(cx, &window);
    assert!(
        matches!(popup, Some((Popup::Slash, 0))),
        "popup should be open: {popup:?}"
    );
    assert_eq!(sel, 0);

    // Tab cycles: 0 → 1 → 0; down adds 1, up subtracts 1 (wraps around)
    press(cx, &window, "tab");
    let (_, sel, _, _) = read(cx, &window);
    assert_eq!(sel, 1, "Tab should move selection to /compact");
    press(cx, &window, "tab");
    let (_, sel, _, _) = read(cx, &window);
    assert_eq!(sel, 0, "Tab should wrap around");
    press(cx, &window, "down");
    let (_, sel, _, _) = read(cx, &window);
    assert_eq!(sel, 1, "down should advance selection by 1");
    press(cx, &window, "up");
    let (_, sel, _, _) = read(cx, &window);
    assert_eq!(sel, 0, "up should move selection back by 1");

    // Back on /compact, press Enter: staged as a token chip, no events sent
    press(cx, &window, "down");
    press(cx, &window, "enter");
    let (popup, _, value, tokens) = read(cx, &window);
    assert!(
        popup.is_none(),
        "popup should close after confirm: {popup:?}"
    );
    assert!(
        value.starts_with("/compact "),
        "command should be staged as a chip: {value:?}"
    );
    assert_eq!(tokens, 1, "command should be an atomic token");
    assert!(
        events.borrow().is_empty(),
        "staging must not send: {:?}",
        events.borrow().len()
    );

    // After the chip, type extra instructions then press Enter: dispatched by
    // command with the appended text
    type_text(cx, &window, "focus on the README");
    press(cx, &window, "enter");
    let (_, _, value, _) = read(cx, &window);
    assert_eq!(value, "", "input should be cleared after send");
    let captured = events.borrow();
    assert_eq!(captured.len(), 1, "should emit exactly one Compact event");
    match &captured[0] {
        ComposerEvent::Compact { instruction } => {
            assert_eq!(instruction.as_deref(), Some("focus on the README"))
        }
        _ => panic!("expected a Compact event"),
    }
}

/// Mention popup: arrow keys navigate → Enter inserts a file token (nothing sent);
/// Esc closes the popup
#[gpui_kit::test]
fn mention_popup_nav_inserts_and_escape_closes(cx: &mut gpui_kit::TestAppContext) {
    cx.update(gpui_kit::init);
    bind_popup_keys(cx);
    let window = open_composer(cx);
    let (events, _events_sub) = capture_events(&window, cx);

    window
        .update(cx, |probe, _, cx| {
            probe.composer.update(cx, |this, cx| {
                this.set_mention_results(vec!["src/a.rs".into(), "src/b.rs".into()], cx);
            });
        })
        .unwrap();
    type_text(cx, &window, "@");
    let (popup, sel, _, _) = read(cx, &window);
    assert!(
        matches!(popup, Some((Popup::Mention, _))),
        "popup should be open: {popup:?}"
    );
    assert_eq!(sel, 0);
    // The popup must actually render visible (not just a state flag)
    cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
    cx.update_window(window.into(), |_, window, _| {
        let snap = window.find("composer-popup");
        assert!(snap.visible(), "popup should be visible");
        assert!(
            snap.bounds().size.width > gpui_kit::px(200.),
            "popup should span the input width: {:?}",
            snap.bounds()
        );
    })
    .unwrap();

    // Down selects the second item; Enter inserts the file token (nothing sent)
    press(cx, &window, "down");
    press(cx, &window, "enter");
    let (popup, _, value, tokens) = read(cx, &window);
    assert!(popup.is_none());
    assert!(
        value.contains("@src/b.rs"),
        "the second item should be inserted: {value:?}"
    );
    assert_eq!(tokens, 1);
    // No message/command events may be emitted (SearchFiles triggered by @ is a
    // normal search request)
    assert!(
        events
            .borrow()
            .iter()
            .all(|e| matches!(e, ComposerEvent::SearchFiles(_))),
        "inserting a token must not send: {:?}",
        events.borrow().len()
    );

    // Reopen the popup; Esc closes it and the typed text is preserved
    type_text(cx, &window, "@");
    let (popup, _, _, _) = read(cx, &window);
    assert!(matches!(popup, Some((Popup::Mention, _))));
    press(cx, &window, "escape");
    let (popup, _, value, _) = read(cx, &window);
    assert!(popup.is_none(), "Esc should close the popup");
    assert!(
        value.ends_with('@'),
        "Esc should only close the popup, leaving text untouched: {value:?}"
    );
}

/// Regression: after deleting the trailing space of a chip/@-token, the popup must
/// not "revive"; a trigger position falling inside the token range does not count as
/// a real trigger (user-reported issue)
#[gpui_kit::test]
fn token_tail_backspace_does_not_reopen_popup(cx: &mut gpui_kit::TestAppContext) {
    cx.update(gpui_kit::init);
    bind_popup_keys(cx);
    let window = open_composer(cx);
    let _events = capture_events(&window, cx);

    // Command chip: / → Enter stages (/clear plus a trailing space) → backspace
    // deletes the space
    type_text(cx, &window, "/");
    press(cx, &window, "enter");
    let (popup, _, value, tokens) = read(cx, &window);
    assert!(popup.is_none() && value.starts_with("/clear ") && tokens == 1);
    press(cx, &window, "backspace");
    let (popup, _, value, _) = read(cx, &window);
    assert_eq!(value, "/clear", "the space should be deleted: {value:?}");
    assert!(
        popup.is_none(),
        "popup must not reappear after deleting the space: {popup:?}"
    );

    // Same for the @-token: clear input → insert file → backspace deletes the
    // trailing space
    window
        .update(cx, |probe, window, cx| {
            probe.composer.update(cx, |this, cx| {
                this.input.update(cx, |input, cx| {
                    input.set_value("", window, cx);
                });
                this.set_mention_results(vec!["src/a.rs".into()], cx);
            });
        })
        .unwrap();
    type_text(cx, &window, "@");
    press(cx, &window, "enter");
    let (popup, _, value, tokens) = read(cx, &window);
    assert!(popup.is_none() && value.starts_with("@src/a.rs ") && tokens == 1);
    press(cx, &window, "backspace");
    let (popup, _, value, _) = read(cx, &window);
    assert_eq!(value, "@src/a.rs");
    assert!(
        popup.is_none(),
        "popup must not reappear after deleting an @token's space: {popup:?}"
    );
}

/// Plan toggle (orthogonal to the permission level): checking "Plan" at the top of
/// the mode popup → SetPlanMode(true) and a plan chip appears in the bar; clicking
/// the chip's X → SetPlanMode(false) and the chip disappears
#[gpui_kit::test]
fn plan_mode_toggle_and_chip(cx: &mut gpui_kit::TestAppContext) {
    cx.update(gpui_kit::init);
    bind_popup_keys(cx);

    // The mode popup expands upward from the bar: use a bottom-pinned layout to pin
    // the composer to the window bottom, so the popup lands inside the viewport and
    // is hittable (with top alignment the popup's y goes negative and gets clipped)
    struct BottomProbe {
        composer: gpui_kit::Entity<Composer>,
    }
    impl gpui_kit::Render for BottomProbe {
        fn render(
            &mut self,
            _window: &mut gpui_kit::Window,
            _cx: &mut gpui_kit::Context<Self>,
        ) -> impl gpui_kit::IntoElement {
            use gpui_kit::{IntoElement as _, ParentElement as _, Styled as _};
            gpui_kit::base::v_flex()
                .size_full()
                .child(gpui_kit::base::v_flex().flex_1())
                .child(self.composer.clone().into_any_element())
        }
    }
    let window = cx.open_window(
        gpui_kit::size(gpui_kit::px(800.), gpui_kit::px(700.)),
        |window, cx| {
            let composer = cx.new(|cx| Composer::new(window, cx));
            BottomProbe { composer }
        },
    );
    // Event capture and focus (BottomProbe and Probe share field names but differ in
    // type; subscribe separately)
    let captured: Rc<RefCell<Vec<ComposerEvent>>> = Rc::new(RefCell::new(Vec::new()));
    let sink = captured.clone();
    let mut subscription = None;
    window
        .update(cx, |probe, window, cx| {
            probe.composer.update(cx, |_, cx| {
                let entity = cx.entity();
                subscription = Some(cx.subscribe_in(
                    &entity,
                    window,
                    move |_, _, event: &ComposerEvent, _, _| {
                        sink.borrow_mut().push(event.clone());
                    },
                ));
            });
            probe.composer.update(cx, |this, cx| {
                this.input.update(cx, |input, cx| input.focus(window, cx));
            });
        })
        .unwrap();
    let _sub = subscription.expect("subscription");

    // Open the mode popup (the bar chip has no test_support observation point, so
    // open it programmatically, same render path as clicking the chip)
    window
        .update(cx, |probe, _, cx| {
            probe.composer.update(cx, |this, cx| {
                this.popup = Some((Popup::ExecMode, 0));
                cx.notify();
            });
        })
        .unwrap();
    cx.update_window(*window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    // The popup enter animation runs on the real wall clock: sleep past it and render
    // another frame so the hit region takes effect
    std::thread::sleep(std::time::Duration::from_millis(250));
    cx.update_window(*window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    cx.update_window(*window, |_, window, _| {
        // The plan row and list row highlight blocks share width and margins
        // (regression: the header slot is full width, the row must carry its own mx_1)
        let plan = window.find("plan-mode-toggle");
        let row0 = window
            .try_find(gpui_kit::base::IndexPath::new(0))
            .expect("first list row");
        assert_eq!(
            plan.bounds().origin.x,
            row0.bounds().origin.x,
            "highlight left edges should align: {:?} vs {:?}",
            plan.bounds(),
            row0.bounds()
        );
        assert_eq!(
            plan.bounds().size.width,
            row0.bounds().size.width,
            "highlight blocks should have equal width"
        );
        assert!(
            window.try_find("plan-mode-toggle").is_some(),
            "popup should have the plan toggle row"
        );
        assert!(
            window.try_find("plan-chip").is_none(),
            "no plan chip when plan is not enabled"
        );
    })
    .unwrap();

    // Check "Plan": event emitted and chip appears (popup stays open, same trade-off
    // as the fs toggles)
    cx.update_window(*window, |_, window, cx| {
        window.click("plan-mode-toggle", cx);
        window.render_frame(cx);
    })
    .unwrap();
    window
        .update(cx, |probe, _, cx| {
            assert!(
                probe.composer.read(cx).plan_enabled(),
                "plan_enabled after checking"
            );
        })
        .unwrap();
    cx.update_window(*window, |_, window, _| {
        assert!(
            window.try_find("plan-chip").is_some(),
            "plan chip should appear after enabling"
        );
    })
    .unwrap();

    // The popup is still open; close it first, then click the chip's X
    cx.update_window(*window, |_, window, cx| {
        window.click("plan-mode-toggle", cx); // clicking again = off (also verifies toggling back and forth)
        window.render_frame(cx);
    })
    .unwrap();
    window
        .update(cx, |probe, _, cx| {
            assert!(!probe.composer.read(cx).plan_enabled());
        })
        .unwrap();
    // Reopen, enable plan again, then exercise the chip X close path
    cx.update_window(*window, |_, window, cx| {
        window.click("plan-mode-toggle", cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.update_window(*window, |_, window, cx| {
        window.click("plan-chip-close", cx);
        window.render_frame(cx);
    })
    .unwrap();
    window
        .update(cx, |probe, _, cx| {
            assert!(
                !probe.composer.read(cx).plan_enabled(),
                "should reset after closing via X"
            );
        })
        .unwrap();
    cx.update_window(*window, |_, window, _| {
        assert!(
            window.try_find("plan-chip").is_none(),
            "chip disappears after close"
        );
    })
    .unwrap();

    let events = captured.borrow();
    let plan_events: Vec<bool> = events
        .iter()
        .filter_map(|e| match e {
            ComposerEvent::SetPlanMode(on) => Some(*on),
            _ => None,
        })
        .collect();
    assert_eq!(
        plan_events,
        vec![true, false, true, false],
        "one event per on/off/on/off toggle: {plan_events:?}"
    );
}

/// Plan approval panel test helper: inject an ExitPlanMode approval into the
/// composer and render one frame
fn set_plan_approval(
    cx: &mut gpui_kit::TestAppContext,
    window: &gpui_kit::WindowHandle<Probe>,
    request_id: &str,
) {
    let approval = super::PendingApproval {
        request_id: request_id.to_string(),
        session_id: "s1".to_string(),
        tool: "ExitPlanMode".to_string(),
        detail: "# Implementation plan\n\n1. Step one: change the protocol\n2. Step two: change the engine".to_string(),
        danger_key: None,
        cwd: "/tmp/proj".to_string(),
    };
    window
        .update(cx, |probe, _, cx| {
            probe.composer.update(cx, |this, cx| {
                this.set_approval(Some(approval), cx);
            });
        })
        .unwrap();
    cx.update_window((*window).into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}

/// Plan approval panel (same as kimi-code): the ExitPlanMode approval renders the
/// full markdown plan plus three buttons: revise / reject and exit / approve plan;
/// approve=Allow, revise/reject=Reject; after the decision, plan_state and the
/// approval state are cleared
#[gpui_kit::test]
fn plan_approval_panel_decides(cx: &mut gpui_kit::TestAppContext) {
    use gpui_kit::AppContext as _;
    cx.update(gpui_kit::init);
    bind_popup_keys(cx);
    // The approval bar replaces the whole input area: make the window taller so the
    // buttons land inside the viewport and are hittable
    let window = cx.open_window(
        gpui_kit::size(gpui_kit::px(800.), gpui_kit::px(700.)),
        |window, cx| {
            let composer = cx.new(|cx| Composer::new(window, cx));
            Probe { composer }
        },
    );
    let (captured, _sub) = capture_events(&window, cx);

    set_plan_approval(cx, &window, "req-1");
    cx.update_window(*window, |_, window, _| {
        assert!(
            window.try_find("plan-approval-detail").is_some(),
            "plan panel detail should render"
        );
        assert!(window.try_find("plan-approve").is_some(), "approve button");
        assert!(window.try_find("plan-revise").is_some(), "revise button");
        assert!(
            window.try_find("plan-reject").is_some(),
            "reject-and-exit button"
        );
        assert!(
            window.try_find("plan-path").is_some(),
            "plan file path link"
        );
        // The generic approval bar's "always allow in this session" must not appear
        // in the plan panel
        assert!(window.try_find("approval-always").is_none());
    })
    .unwrap();

    // Path link click → ComposerEvent::OpenFile (.pigcode/plans/plan-<sid>.md)
    cx.update_window(*window, |_, window, cx| {
        window.click("plan-path", cx);
    })
    .unwrap();
    {
        let events = captured.borrow();
        let opened: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                ComposerEvent::OpenFile { path } => Some(path.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            opened,
            vec!["/tmp/proj/.pigcode/plans/plan-s1.md"],
            "path link should open the plan file: {opened:?}"
        );
    }

    // Approve plan → Allow
    cx.update_window(*window, |_, window, cx| {
        window.click("plan-approve", cx);
    })
    .unwrap();
    window
        .update(cx, |probe, _, cx| {
            assert!(
                probe.composer.read(cx).plan_state.is_none(),
                "plan_state cleared after decision"
            );
        })
        .unwrap();

    // Revise → input mode (cancel once to return to the three buttons, then enter
    // input mode again, type, and submit)
    set_plan_approval(cx, &window, "req-2");
    cx.update_window(*window, |_, window, cx| {
        window.click("plan-revise", cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.update_window(*window, |_, window, _| {
        assert!(
            window.try_find("plan-revise-submit").is_some(),
            "revise mode should show the submit-and-reject button"
        );
        assert!(
            window.try_find("plan-approve").is_none(),
            "input mode hides the three buttons"
        );
    })
    .unwrap();
    // Cancel: back to the three-button state
    cx.update_window(*window, |_, window, cx| {
        window.click("plan-revise-cancel", cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.update_window(*window, |_, window, _| {
        assert!(
            window.try_find("plan-approve").is_some(),
            "back to the three buttons after cancel"
        );
    })
    .unwrap();
    // Enter input mode again: type → submit and reject (with feedback)
    cx.update_window(*window, |_, window, cx| {
        window.click("plan-revise", cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.update_window(*window, |_, window, cx| {
        window.input("step three is wrong", cx);
    })
    .unwrap();
    cx.update_window(*window, |_, window, cx| {
        window.click("plan-revise-submit", cx);
    })
    .unwrap();

    // Reject and exit → Reject (no feedback)
    set_plan_approval(cx, &window, "req-3");
    cx.update_window(*window, |_, window, cx| {
        window.click("plan-reject", cx);
    })
    .unwrap();

    let events = captured.borrow();
    let decisions: Vec<(&str, pig_protocol::ApprovalDecision, Option<String>)> = events
        .iter()
        .filter_map(|e| match e {
            ComposerEvent::DecideApproval {
                request_id,
                decision,
                feedback,
            } => Some((request_id.as_str(), *decision, feedback.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        decisions,
        vec![
            ("req-1", pig_protocol::ApprovalDecision::Allow, None),
            (
                "req-2",
                pig_protocol::ApprovalDecision::Reject,
                Some("step three is wrong".to_string())
            ),
            ("req-3", pig_protocol::ApprovalDecision::Reject, None),
        ],
        "approve / revise-with-feedback reject / bare reject each in place: {decisions:?}"
    );
}

/// Guard test for the danger_key static mapping: all six reason keys core already
/// emits have localized text (zh via danger_reason_text, en via direct registry
/// lookup of the dynamic key; both non-empty with no unreplaced placeholders);
/// unknown keys (added by core in the future) return None and no warning line is
/// shown. When core adds a new DangerReason, danger_reason_text's match arms and
/// this table must be updated in sync.
#[test]
fn danger_reason_text_covers_all_known_keys() {
    const KEYS: [&str; 6] = [
        "fork_bomb",
        "rm_rf_root",
        "disk_format",
        "dd_block",
        "shutdown",
        "chmod_root",
    ];
    for key in KEYS {
        // zh: go through the production function (ctor already pins zh-CN)
        let zh = super::danger_reason_text(key)
            .unwrap_or_else(|| panic!("danger_key {key} should have a static mapping"));
        assert!(
            !zh.is_empty() && !zh.contains("%{"),
            "{key} zh text is abnormal: {zh}"
        );
        // en: look the dynamic key up directly in the registry (do not flip the
        // process-global locale, to avoid cross-contamination with concurrent tests)
        let en_key = format!("approval.danger.{key}");
        let en = rust_i18n::t!(en_key.as_str(), locale = "en");
        assert!(
            !en.is_empty() && !en.contains("%{") && !en.starts_with("approval.danger."),
            "{key} en text is abnormal (t! echoes the key name when missing): {en}"
        );
    }
    assert_eq!(super::danger_reason_text("future_new_reason"), None);
    // The title key exists in both locales (used as the warning line prefix)
    assert!(!rust_i18n::t!("approval.danger.high_risk").is_empty());
    assert!(!rust_i18n::t!("approval.danger.high_risk", locale = "en").is_empty());
}
