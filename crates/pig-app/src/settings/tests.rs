// Explicit imports only: `use super::*` would pull in gpui's `test` macro,
// which shadows the built-in #[test] (same trap as dock.rs / sidebar tests)
use crate::settings::SettingsView;
use gpui_kit::base::ScrollbarHandle as _;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AppContext as _, Context, Entity, ParentElement as _, Render, Styled as _, Window, div,
};

/// Probe view: the window root holds the settings entity (same shape as the
/// dock.rs / sidebar tests)
struct SettingsProbe {
    view: Entity<SettingsView>,
}

impl Render for SettingsProbe {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui_kit::IntoElement {
        div().size_full().child(self.view.clone())
    }
}

/// Model dialog layout invariants with the advanced section expanded
/// (user-reported regression 2026-10-10): the shell only carries a max-height,
/// so the form body must be capped on the scroll element itself — otherwise the
/// body grows unbounded, the content spills past the dialog borders, and
/// nothing scrolls. The footer must also stay inside the dialog (it is a shell
/// child, not scroll content).
#[test]
fn model_dialog_advanced_open_caps_body_and_keeps_footer_inside() {
    let cx = &mut gpui_kit::TestAppContext::single();
    cx.update(gpui_kit::init);
    let window = cx.open_window(
        gpui_kit::size(gpui_kit::px(1000.), gpui_kit::px(800.)),
        |window, cx| {
            let view = cx.new(|cx| SettingsView::new(window, cx));
            view.update(cx, |v, cx| {
                v.open_model_dialog(None, window, cx);
                if let Some(d) = v.model_dialog.as_mut() {
                    d.advanced_open = true;
                }
            });
            SettingsProbe { view }
        },
    );

    // Two frames: the first lays out, the second picks up measured sizes
    for _ in 0..2 {
        cx.update_window(*window, |_, window, cx| {
            window.render_frame(cx);
        })
        .expect("window alive");
    }

    // Stateful div ids (model-dialog/body) only register as path prefixes, not
    // findable leaves — the scroll handle carries the body's measured viewport
    // instead, and the save button leaf stands in for the footer
    let (body, max_y, save) = window
        .update(cx, |probe, window, cx| {
            let save = window.find("save-dialog").bounds();
            let d = probe.view.read(cx).model_dialog.as_ref().unwrap();
            (
                d.body_scroll.viewport_bounds(),
                f32::from(d.body_scroll.max_offset().y),
                save,
            )
        })
        .expect("window alive");

    assert!(
        f32::from(body.size.height) <= 529.,
        "form body must stay within its 528px cap: {}",
        body.size.height
    );
    assert!(
        max_y > 0.,
        "advanced-expanded content overflows the cap, so it must be scrollable"
    );
    assert!(
        save.origin.y >= body.bottom() - gpui_kit::px(1.),
        "the fixed footer sits below the capped body: body={} save={}",
        body.bottom(),
        save.origin.y
    );
    assert!(
        save.bottom() <= gpui_kit::px(800.),
        "the fixed footer must stay inside the window: {}",
        save.bottom()
    );
}

/// MCP and skill dialogs share the model dialog's layout (fixed header with a
/// top-right close + capped scrollable body + fixed footer). JSON mode's
/// 12-row textarea and the skill body's 10-row textarea guarantee both bodies
/// overflow their caps, so scrollability is asserted unconditionally; the
/// header X click must close the dialog.
#[test]
fn mcp_and_skills_dialogs_scroll_and_close_button_works() {
    let cx = &mut gpui_kit::TestAppContext::single();
    cx.update(gpui_kit::init);
    let window = cx.open_window(
        gpui_kit::size(gpui_kit::px(1000.), gpui_kit::px(800.)),
        |window, cx| {
            let view = cx.new(|cx| SettingsView::new(window, cx));
            view.update(cx, |v, cx| {
                v.open_mcp_dialog(None, window, cx);
                // Advanced section open: the form fields + env textarea push
                // the body past its cap
                if let Some(d) = v.mcp_dialog.as_mut() {
                    d.advanced_open = true;
                }
            });
            SettingsProbe { view }
        },
    );

    let render = |cx: &mut gpui_kit::TestAppContext| {
        // Several frames: auto-grow textareas measure and resize over frames
        for _ in 0..5 {
            cx.update_window(*window, |_, window, cx| {
                window.render_frame(cx);
            })
            .expect("window alive");
        }
    };
    render(cx);

    // MCP dialog: body capped, scrollable, footer inside the window
    let (body_h, max_y, save_bottom) = window
        .update(cx, |probe, window, cx| {
            let d = probe.view.read(cx).mcp_dialog.as_ref().unwrap();
            let save = window.find("mcp-dialog-save").bounds();
            (
                f32::from(d.body_scroll.viewport_bounds().size.height),
                f32::from(d.body_scroll.max_offset().y),
                f32::from(save.bottom()),
            )
        })
        .expect("window alive");
    assert!(
        body_h <= 529.,
        "MCP form body must stay within its 528px cap: {body_h}"
    );
    assert!(max_y > 0., "MCP form body must be scrollable");
    assert!(
        save_bottom <= 800.,
        "MCP fixed footer must stay inside the window: {save_bottom}"
    );

    // The top-right X closes the dialog
    cx.update_window(*window, |_, window, cx| {
        window.click("mcp-dialog-close", cx);
    })
    .expect("window alive");
    let closed = window
        .update(cx, |probe, _, cx| probe.view.read(cx).mcp_dialog.is_none())
        .expect("window alive");
    assert!(closed, "the header close button must close the MCP dialog");

    // Skill dialog: same invariants (body textarea alone guarantees overflow)
    window
        .update(cx, |probe, window, cx| {
            probe
                .view
                .update(cx, |v, cx| v.open_skills_dialog(None, window, cx));
        })
        .expect("window alive");
    render(cx);
    let (body_h, max_y, save_bottom) = window
        .update(cx, |probe, window, cx| {
            let d = probe.view.read(cx).skills_dialog.as_ref().unwrap();
            let save = window.find("skills-dialog-save").bounds();
            (
                f32::from(d.body_scroll.viewport_bounds().size.height),
                f32::from(d.body_scroll.max_offset().y),
                f32::from(save.bottom()),
            )
        })
        .expect("window alive");
    assert!(
        body_h <= 569.,
        "skill form body must stay within its 568px cap: {body_h}"
    );
    assert!(max_y > 0., "skill body must be scrollable");
    assert!(
        save_bottom <= 800.,
        "skill fixed footer must stay inside the window: {save_bottom}"
    );
}
