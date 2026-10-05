// 显式导入：super 链会把 gpui 的 `test` 宏导进来遮蔽内置 #[test]（dock.rs 同款坑）
use crate::sidebar::{Sidebar, SidebarSession};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AppContext as _, Context, Entity, ParentElement as _, Render, Styled as _, Window, div,
};
use std::collections::HashMap;
use std::path::PathBuf;

/// 侧栏探针视图（dock.rs 测试同款结构：窗口根视图持有实体）
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
            title: format!("会话 {i}"),
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

/// 分页「展开更多/收起」：容器高度应渐变，而非瞬变（用户实测回归：
/// 收起时内容已变少，max_h 上限帽压不住容器）
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
    // 12 个会话都在同一工作区
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

    // 展开工作区（与点击文件夹行同路径：入组 + gen+1）
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
    // 睡过展开动画落终态（5 条 + 分页控制条）
    std::thread::sleep(std::time::Duration::from_millis(300));
    render(cx, &window);
    let h5 = block_height(cx, &window);

    // 展开更多：+5 条，容器应从 h5 渐变上去
    click(cx, &window, ("workspace-more", 0));
    render(cx, &window);
    let h_grow_start = block_height(cx, &window);
    assert!(
        h_grow_start < h5 + 60.,
        "展开更多首帧容器应仍接近旧高 {h5}，实测 {h_grow_start}"
    );
    std::thread::sleep(std::time::Duration::from_millis(300));
    render(cx, &window);
    let h10 = block_height(cx, &window);
    assert!(h10 > h5 + 80., "展开更多终态应明显更高（{h5} → {h10}）");

    // 收起（分页）：容器应从 h10 渐变回 h5，而不是瞬塌
    click(cx, &window, ("workspace-collapse", 0));
    render(cx, &window);
    let h_shrink_start = block_height(cx, &window);
    assert!(
        h_shrink_start > h10 - 60.,
        "分页收起首帧容器应仍接近旧高 {h10}，实测 {h_shrink_start}"
    );
    // 「到期删行」计时器是真实墙钟：先睡过它（250ms），再泵前台续体
    //（run_until_parked 对挂起定时器视为 parked 会立即返回，不能替代等待）
    // 「到期删行」计时器走测试调度器的假时钟（真 sleep 不推进）：
    // 推进假时钟过 250ms，再泵前台续体应用变更
    cx.dispatcher
        .advance_clock(std::time::Duration::from_millis(300));
    cx.run_until_parked();
    render(cx, &window);
    let h_back = block_height(cx, &window);
    assert!(
        (h_back - h5).abs() < 2.,
        "分页收起终态应回到 {h5}，实测 {h_back}"
    );
}
