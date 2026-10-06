//! / 和 @ 弹层的键盘导航与命令分阶（staged command）headless 测试。
//! 按键走真实派发链：keystroke → Input context 上后注册的应用绑定（抢过
//! 输入框原生绑定）→ 动作冒泡到 Composer 根的 on_action。
//! 注意：本文件必须显式导入——`use super::*` 会把 gpui 的 `test` 宏导进来
//! 遮蔽内置 #[test] 并展开无限递归（dock.rs 踩过的同款坑）。

use super::{Composer, ComposerEvent, Popup};
use crate::{ComposerNavDown, ComposerNavNext, ComposerNavPrev, ComposerNavUp, ComposerPopupClose};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AppContext as _, KeyBinding};
use std::cell::RefCell;
use std::rc::Rc;

/// 与 main.rs 相同的 5 条 Input context 绑定（测试里 gpui_kit::init 之后注册，
/// 同深度后注册者优先——复刻生产接线）
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

/// 捕获 Composer 事件（断言「分阶未发送/发送时按命令分派」），并把焦点放进输入框。
/// 返回的 Subscription 必须活到测试结束（drop 即退订）
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

/// TestWindowExt 为 Window 实现：经 cx.update_window 拿 &mut Window 再派发
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

/// / 弹层：Tab/↑↓ 循环切换 → Enter 分阶为 chip（不发送）→ 续写 → Enter 按命令分派
#[gpui_kit::test]
fn slash_popup_nav_stages_then_dispatches(cx: &mut gpui_kit::TestAppContext) {
    cx.update(gpui_kit::init);
    bind_popup_keys(cx);
    let window = open_composer(cx);
    let (events, _events_sub) = capture_events(&window, cx);

    // 输入 / 打开弹层（默认选中首项 /clear）
    type_text(cx, &window, "/");
    let (popup, sel, _, _) = read(cx, &window);
    assert!(
        matches!(popup, Some((Popup::Slash, 0))),
        "弹层应打开: {popup:?}"
    );
    assert_eq!(sel, 0);

    // Tab 循环：0 → 1 → 0；↓ 再 +1；↑ -1（回绕）
    press(cx, &window, "tab");
    let (_, sel, _, _) = read(cx, &window);
    assert_eq!(sel, 1, "Tab 应切到 /compact");
    press(cx, &window, "tab");
    let (_, sel, _, _) = read(cx, &window);
    assert_eq!(sel, 0, "Tab 应回绕");
    press(cx, &window, "down");
    let (_, sel, _, _) = read(cx, &window);
    assert_eq!(sel, 1, "↓ 应 +1");
    press(cx, &window, "up");
    let (_, sel, _, _) = read(cx, &window);
    assert_eq!(sel, 0, "↑ 应 -1");

    // 回到 /compact 按 Enter：分阶为 token chip，不发送任何事件
    press(cx, &window, "down");
    press(cx, &window, "enter");
    let (popup, _, value, tokens) = read(cx, &window);
    assert!(popup.is_none(), "确认后弹层应关闭: {popup:?}");
    assert!(
        value.starts_with("/compact "),
        "命令应分阶为 chip: {value:?}"
    );
    assert_eq!(tokens, 1, "命令应是原子 token");
    assert!(
        events.borrow().is_empty(),
        "分阶不得发送: {:?}",
        events.borrow().len()
    );

    // chip 后续写重点说明，再按 Enter：按命令分派，附续写文本
    type_text(cx, &window, "重点关注 README");
    press(cx, &window, "enter");
    let (_, _, value, _) = read(cx, &window);
    assert_eq!(value, "", "发送后输入框应清空");
    let captured = events.borrow();
    assert_eq!(captured.len(), 1, "应只发一次 Compact 事件");
    match &captured[0] {
        ComposerEvent::Compact { instruction } => {
            assert_eq!(instruction.as_deref(), Some("重点关注 README"))
        }
        _ => panic!("应为 Compact 事件"),
    }
}

/// @ 弹层：↑↓ 切换 → Enter 插文件 token（不发送）；Esc 关闭弹层
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
        "弹层应打开: {popup:?}"
    );
    assert_eq!(sel, 0);
    // 弹层必须真实渲染可见（不只是状态位）
    cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
    cx.update_window(window.into(), |_, window, _| {
        let snap = window.find("composer-popup");
        assert!(snap.visible(), "弹层应可见");
        assert!(
            snap.bounds().size.width > gpui_kit::px(200.),
            "弹层应铺满输入框宽度: {:?}",
            snap.bounds()
        );
    })
    .unwrap();

    // ↓ 选中第二项，Enter 插入文件 token（不发送）
    press(cx, &window, "down");
    press(cx, &window, "enter");
    let (popup, _, value, tokens) = read(cx, &window);
    assert!(popup.is_none());
    assert!(value.contains("@src/b.rs"), "应插入第二项: {value:?}");
    assert_eq!(tokens, 1);
    // 不得发出消息/命令类事件（@ 触发的 SearchFiles 是正常搜索请求）
    assert!(
        events
            .borrow()
            .iter()
            .all(|e| matches!(e, ComposerEvent::SearchFiles(_))),
        "插入 token 不得发送: {:?}",
        events.borrow().len()
    );

    // 再次打开弹层，Esc 关闭且输入内容保留
    type_text(cx, &window, "@");
    let (popup, _, _, _) = read(cx, &window);
    assert!(matches!(popup, Some((Popup::Mention, _))));
    press(cx, &window, "escape");
    let (popup, _, value, _) = read(cx, &window);
    assert!(popup.is_none(), "Esc 应关闭弹层");
    assert!(value.ends_with('@'), "Esc 只关弹层不动文本: {value:?}");
}

/// 回归：删掉 chip/@token 的尾随空格后，弹层不得「复活」——
/// 触发位置落在 token 范围内不算真触发符（用户实测反馈）
#[gpui_kit::test]
fn token_tail_backspace_does_not_reopen_popup(cx: &mut gpui_kit::TestAppContext) {
    cx.update(gpui_kit::init);
    bind_popup_keys(cx);
    let window = open_composer(cx);
    let _events = capture_events(&window, cx);

    // 命令 chip：/ → Enter 分阶（/clear + 尾随空格）→ 退格删空格
    type_text(cx, &window, "/");
    press(cx, &window, "enter");
    let (popup, _, value, tokens) = read(cx, &window);
    assert!(popup.is_none() && value.starts_with("/clear ") && tokens == 1);
    press(cx, &window, "backspace");
    let (popup, _, value, _) = read(cx, &window);
    assert_eq!(value, "/clear", "空格应被删掉: {value:?}");
    assert!(popup.is_none(), "删空格后弹层不得复活: {popup:?}");

    // @token 同款：清空输入 → 插文件 → 退格删尾随空格
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
    assert!(popup.is_none(), "@token 删空格后弹层不得复活: {popup:?}");
}

/// 计划开关（与权限档正交）：模式弹层顶部勾选「计划」→ SetPlanMode(true) +
/// bar 出计划 chip；chip 点 X 关闭 → SetPlanMode(false) + chip 消失
#[gpui_kit::test]
fn plan_mode_toggle_and_chip(cx: &mut gpui_kit::TestAppContext) {
    cx.update(gpui_kit::init);
    bind_popup_keys(cx);

    // 模式弹层从 bar 向上展开：用压底布局把 composer 钉到窗口底部，
    // 弹层才落在视口内可命中（顶对齐时弹层 y 为负被裁）
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
    // 事件捕获与聚焦（BottomProbe 与 Probe 结构同名不同型，单独订阅）
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

    // 打开模式弹层（bar chip 无 test_support 观测点，程序化开启——与点 chip 同渲染路径）
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
    // 弹层入场动画走真实墙钟：睡过它再渲一帧，命中区才生效
    std::thread::sleep(std::time::Duration::from_millis(250));
    cx.update_window(*window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    cx.update_window(*window, |_, window, _| {
        // 计划行与列表行的高亮块同宽同边距（回归：header 槽全宽，行要自带 mx_1）
        let plan = window.find("plan-mode-toggle");
        let row0 = window
            .try_find(gpui_kit::base::IndexPath::new(0))
            .expect("列表首行");
        assert_eq!(
            plan.bounds().origin.x,
            row0.bounds().origin.x,
            "高亮块左缘应对齐: {:?} vs {:?}",
            plan.bounds(),
            row0.bounds()
        );
        assert_eq!(
            plan.bounds().size.width,
            row0.bounds().size.width,
            "高亮块应同宽"
        );
        assert!(
            window.try_find("plan-mode-toggle").is_some(),
            "弹层应有计划勾选行"
        );
        assert!(
            window.try_find("plan-chip").is_none(),
            "未开启时无计划 chip"
        );
    })
    .unwrap();

    // 勾选「计划」：事件 + chip 出现（弹层保持打开，与 fs 开关同款取舍）
    cx.update_window(*window, |_, window, cx| {
        window.click("plan-mode-toggle", cx);
        window.render_frame(cx);
    })
    .unwrap();
    window
        .update(cx, |probe, _, cx| {
            assert!(
                probe.composer.read(cx).plan_enabled(),
                "勾选后 plan_enabled"
            );
        })
        .unwrap();
    cx.update_window(*window, |_, window, _| {
        assert!(
            window.try_find("plan-chip").is_some(),
            "开启后应出计划 chip"
        );
    })
    .unwrap();

    // 弹层还开着，先关掉再点 chip 的 X
    cx.update_window(*window, |_, window, cx| {
        window.click("plan-mode-toggle", cx); // 再点一次 = 关（同时验证来回切换）
        window.render_frame(cx);
    })
    .unwrap();
    window
        .update(cx, |probe, _, cx| {
            assert!(!probe.composer.read(cx).plan_enabled());
        })
        .unwrap();
    // 重新打开再开计划，走 chip X 关闭路径
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
            assert!(!probe.composer.read(cx).plan_enabled(), "X 关闭后应复位");
        })
        .unwrap();
    cx.update_window(*window, |_, window, _| {
        assert!(window.try_find("plan-chip").is_none(), "关闭后 chip 消失");
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
        "开/关/开/关各发一次: {plan_events:?}"
    );
}

/// 计划审批面板测试辅助：给 composer 注入一笔 ExitPlanMode 审批并渲一帧
fn set_plan_approval(
    cx: &mut gpui_kit::TestAppContext,
    window: &gpui_kit::WindowHandle<Probe>,
    request_id: &str,
) {
    let approval = super::PendingApproval {
        request_id: request_id.to_string(),
        session_id: "s1".to_string(),
        tool: "ExitPlanMode".to_string(),
        detail: "# 实施计划\n\n1. 第一步：改协议\n2. 第二步：改引擎".to_string(),
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

/// 计划审批面板（kimi-code 同款）：ExitPlanMode 审批渲染 markdown 计划全文 +
/// 修改/拒绝并退出/批准 plan 三按钮；批准=Allow，修改/拒绝=Reject；决议后
/// plan_state 与审批态清空
#[gpui_kit::test]
fn plan_approval_panel_decides(cx: &mut gpui_kit::TestAppContext) {
    use gpui_kit::AppContext as _;
    cx.update(gpui_kit::init);
    bind_popup_keys(cx);
    // 审批条整组替换输入区：窗口高一点，按钮才落在视口内可命中
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
            "计划面板正文应渲染"
        );
        assert!(window.try_find("plan-approve").is_some(), "批准按钮");
        assert!(window.try_find("plan-revise").is_some(), "修改按钮");
        assert!(window.try_find("plan-reject").is_some(), "拒绝并退出按钮");
        assert!(window.try_find("plan-path").is_some(), "计划文件路径链接");
        // 通用审批条的「本会话内批准」不应出现在计划面板
        assert!(window.try_find("approval-always").is_none());
    })
    .unwrap();

    // 路径链接点击 → ComposerEvent::OpenFile（.pigcode/plans/plan-<sid>.md）
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
            "路径链接应打开计划文件: {opened:?}"
        );
    }

    // 批准 plan → Allow
    cx.update_window(*window, |_, window, cx| {
        window.click("plan-approve", cx);
    })
    .unwrap();
    window
        .update(cx, |probe, _, cx| {
            assert!(
                probe.composer.read(cx).plan_state.is_none(),
                "决议后 plan_state 清空"
            );
        })
        .unwrap();

    // 修改 → 输入态（取消一次回三按钮，再进输入态打字提交）
    set_plan_approval(cx, &window, "req-2");
    cx.update_window(*window, |_, window, cx| {
        window.click("plan-revise", cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.update_window(*window, |_, window, _| {
        assert!(
            window.try_find("plan-revise-submit").is_some(),
            "修改后应出「提交并拒绝」"
        );
        assert!(
            window.try_find("plan-approve").is_none(),
            "输入态隐藏三按钮"
        );
    })
    .unwrap();
    // 取消：回三按钮态
    cx.update_window(*window, |_, window, cx| {
        window.click("plan-revise-cancel", cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.update_window(*window, |_, window, _| {
        assert!(window.try_find("plan-approve").is_some(), "取消后回三按钮");
    })
    .unwrap();
    // 再进输入态：打字 → 提交并拒绝（带反馈）
    cx.update_window(*window, |_, window, cx| {
        window.click("plan-revise", cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.update_window(*window, |_, window, cx| {
        window.input("第三步方案不对", cx);
    })
    .unwrap();
    cx.update_window(*window, |_, window, cx| {
        window.click("plan-revise-submit", cx);
    })
    .unwrap();

    // 拒绝并退出 → Reject（无反馈）
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
                Some("第三步方案不对".to_string())
            ),
            ("req-3", pig_protocol::ApprovalDecision::Reject, None),
        ],
        "批准/修改带反馈拒绝/裸拒绝各就各位: {decisions:?}"
    );
}
