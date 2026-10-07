// Explicit imports: the super chain pulls in gpui's `test` macro, which shadows the
// built-in #[test] (same trap as dock.rs)
use crate::sidebar::{Sidebar, SidebarSession};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AppContext as _, Context, Entity, ParentElement as _, Render, Styled as _, Window, div,
};
use std::collections::HashMap;
use std::path::PathBuf;

/// Sidebar probe view (same structure as dock.rs tests: the window root view holds
/// the entity)
struct SidebarProbe {
    sidebar: Entity<Sidebar>,
}

impl Render for SidebarProbe {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui_kit::IntoElement {
        div().size_full().child(self.sidebar.clone())
    }
}

fn make_sessions(workspace: &str, count: usize) -> Vec<SidebarSession> {
    (0..count)
        .map(|i| SidebarSession {
            id: format!("s{i}"),
            title: format!("session {i}"),
            cwd: PathBuf::from(workspace),
            updated_at: 1000 + i as u64,
            pinned: false,
            archived: false,
            running: false,
            waiting_approval: false,
        })
        .collect()
}

fn render(cx: &mut gpui_kit::TestAppContext, window: &gpui_kit::WindowHandle<SidebarProbe>) {
    cx.update_window((*window).into(), |_, window, cx| {
        window.render_frame(cx);
    })
    .expect("window alive");
}

fn block_height(
    cx: &mut gpui_kit::TestAppContext,
    window: &gpui_kit::WindowHandle<SidebarProbe>,
) -> f32 {
    window
        .update(cx, |_, window, _| {
            f32::from(window.find(("ws-block", 0usize)).bounds().size.height)
        })
        .expect("window alive")
}

fn click(
    cx: &mut gpui_kit::TestAppContext,
    window: &gpui_kit::WindowHandle<SidebarProbe>,
    id: (&'static str, usize),
) {
    cx.update_window((*window).into(), |_, window, cx| {
        window.click(id, cx);
    })
    .expect("window alive");
}

/// Pagination "show more/collapse": the container height should animate, not jump
/// (user-reported regression: after collapsing the content is already reduced, and
/// the max_h cap cannot hold the container down)
#[test]
fn workspace_pagination_resize_animates() {
    let cx = &mut gpui_kit::TestAppContext::single();
    cx.update(gpui_kit::init);
    let window = cx.open_window(gpui_kit::size(gpui_kit::px(500.), gpui_kit::px(900.)), {
        |window, cx| {
            let sidebar = cx.new(|cx| Sidebar::new(window, cx));
            SidebarProbe { sidebar }
        }
    });
    // All 12 sessions in one workspace
    window
        .update(cx, |probe, _, cx| {
            probe.sidebar.update(cx, |sidebar, cx| {
                sidebar.set_state(
                    make_sessions("/tmp/ws", 12),
                    vec!["/tmp/ws".to_string()],
                    HashMap::new(),
                    None,
                    cx,
                );
            });
        })
        .expect("window alive");
    render(cx, &window);

    // Expand the workspace (same path as clicking the folder row: insert + gen+1)
    window
        .update(cx, |probe, _, cx| {
            probe.sidebar.update(cx, |sidebar, _cx| {
                sidebar.expanded.insert("/tmp/ws".to_string());
                let anim = sidebar
                    .expand_anims
                    .entry("/tmp/ws".to_string())
                    .or_default();
                anim.generation += 1;
            });
        })
        .expect("window alive");
    render(cx, &window);
    // Sleep past the expand animation to reach the end state (5 rows + pagination
    // controls)
    std::thread::sleep(std::time::Duration::from_millis(300));
    render(cx, &window);
    let h5 = block_height(cx, &window);

    // Show more: 5 extra rows; the container should grow gradually from h5
    click(cx, &window, ("workspace-more", 0));
    render(cx, &window);
    let h_grow_start = block_height(cx, &window);
    assert!(
        h_grow_start < h5 + 60.,
        "show-more first frame should stay near the old height {h5}, got {h_grow_start}"
    );
    std::thread::sleep(std::time::Duration::from_millis(300));
    render(cx, &window);
    let h10 = block_height(cx, &window);
    assert!(
        h10 > h5 + 80.,
        "show-more end state should be clearly taller ({h5} -> {h10})"
    );

    // Collapse (pagination): the container should shrink from h10 back to h5
    // gradually, not collapse instantly
    click(cx, &window, ("workspace-collapse", 0));
    render(cx, &window);
    let h_shrink_start = block_height(cx, &window);
    assert!(
        h_shrink_start > h10 - 60.,
        "collapse first frame should stay near the old height {h10}, got {h_shrink_start}"
    );
    // The "delete rows on expiry" timer follows the real wall clock: sleep past it
    // (250ms) first, then pump the foreground continuations
    // (run_until_parked treats pending timers as parked and returns immediately, it
    // cannot replace the wait)
    // The "delete rows on expiry" timer runs on the test scheduler's fake clock (a
    // real sleep does not advance it): advance the fake clock past 250ms, then pump
    // the foreground continuations to apply the change
    cx.dispatcher
        .advance_clock(std::time::Duration::from_millis(300));
    cx.run_until_parked();
    render(cx, &window);
    let h_back = block_height(cx, &window);
    assert!(
        (h_back - h5).abs() < 2.,
        "collapse end state should return to {h5}, got {h_back}"
    );
}
