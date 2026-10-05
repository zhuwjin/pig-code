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
