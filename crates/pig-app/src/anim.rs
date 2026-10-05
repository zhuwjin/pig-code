//! 展开/收起动画：内容从 0 高滑开 + 淡入；收起保持挂载播滑收淡出（内容卸载
//! 由调用方的计时器负责，见 thread_view `drive_expand_anim` / sidebar 工作区
//! 行点击处理）。高度目标用内容实测自然高（内层 on_prepaint 持续测量——clip/
//! 高度帽只作用在外层，内层始终按自然高布局；未测到先隐形挂一帧量高）；
//! 动画结束帧（delta=1）摘掉 max_h 帽，超高内容不受残留限制。

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::base::ElementExt as _;
use gpui_kit::*;

/// 展开/收起动画时长
pub(crate) const EXPAND_ANIM_DUR: std::time::Duration = std::time::Duration::from_millis(200);

/// 展开/收起动画状态：generation 每次开合 +1（作为动画元素 id 的一部分驱动重播）；
/// collapsing = 收起动画进行中（内容保持挂载，计时器到期后卸载）；
/// measured_h = 内容自然高度（render 时 on_prepaint 持续测量，作动画目标高——
/// 高度由内容决定、不设固定上限；动画结束帧 delta=1 摘掉 max_h 帽）
#[derive(Default)]
pub struct ExpandAnim {
    pub generation: u64,
    pub collapsing: bool,
    pub measured_h: Rc<Cell<f32>>,
}

/// 展开/收起内容的动画包装：展开 = 内容从 0 高滑开 + 淡入；收起 = 保持挂载
/// 滑收淡出。id 含 generation，每次开合重播
pub(crate) fn expand_anim_wrap(id: String, anim: &ExpandAnim, content: AnyElement) -> AnyElement {
    let measured = anim.measured_h.clone();
    let measured_inner = anim.measured_h.clone();
    let collapsing = anim.collapsing;
    div()
        .overflow_hidden()
        .with_animation(
            id,
            // delta 保持线性，缓动按方向在闭包内分别取：
            // 展开 = ease-out（快开缓落）；收起 = ease-in（慢起步加速收尽——
            // 用 ease-out 收会把动作压在前半段，剩余时间近静止地爬尾巴，
            // 卸载计时器到期时读作「最后顿一下」
            Animation::new(EXPAND_ANIM_DUR),
            move |el, delta| {
                let d = if collapsing {
                    1.0 - delta * delta
                } else {
                    1.0 - (1.0 - delta).powi(5)
                };
                let h = measured.get();
                if !collapsing && delta >= 1.0 {
                    el
                } else if h <= 0. {
                    el.opacity(0.)
                } else {
                    el.max_h(px(h * d)).opacity(d.max(0.))
                }
            },
        )
        .child(
            div()
                .on_prepaint(move |bounds, _, _| measured_inner.set(f32::from(bounds.size.height)))
                .child(content),
        )
        .into_any_element()
}
