//! Expand/collapse animation: content slides open from 0 height plus fades in;
//! collapse keeps it mounted while playing the slide-shut fade-out (unmounting
//! the content is the caller's timer's job, see thread_view `drive_expand_anim`
//! and the sidebar workspace row click handling). The height target is the
//! content's measured natural height (the inner on_prepaint keeps measuring; the
//! clip/height cap only applies to the outer layer, the inner layer always lays
//! out at natural height; if not yet measured, stay mounted invisibly for one
//! frame to measure). The animation's final frame (delta=1) removes the max_h
//! cap so extra-tall content is not limited by the leftover value.

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::base::ElementExt as _;
use gpui_kit::*;

/// Expand/collapse animation duration
pub(crate) const EXPAND_ANIM_DUR: std::time::Duration = std::time::Duration::from_millis(200);

/// Expand/collapse animation state: generation increments by 1 on each
/// open/close (part of the animation element id, driving the replay);
/// collapsing = a collapse animation is in flight (content stays mounted, the
/// timer unmounts it when it expires);
/// measured_h = the content's natural height (continuously measured by
/// on_prepaint during render, used as the animation target height; the height is
/// decided by the content with no fixed cap, and the final frame delta=1 removes
/// the max_h cap)
#[derive(Default)]
pub struct ExpandAnim {
    pub generation: u64,
    pub collapsing: bool,
    pub measured_h: Rc<Cell<f32>>,
}

/// Animation wrapper for expandable/collapsible content: expand = content slides
/// open from 0 height plus fades in; collapse = stays mounted while sliding shut
/// and fading out. The id includes generation, replaying on each open/close
pub(crate) fn expand_anim_wrap(id: String, anim: &ExpandAnim, content: AnyElement) -> AnyElement {
    let measured = anim.measured_h.clone();
    let measured_inner = anim.measured_h.clone();
    let collapsing = anim.collapsing;
    div()
        .overflow_hidden()
        .with_animation(
            id,
            // delta stays linear; the easing is picked per direction inside the
            // closure: expand = ease-out (fast open, gentle settle); collapse =
            // ease-in (slow start, accelerating to fully shut; easing-out the
            // collapse would cram the motion into the first half and creep
            // nearly still through the tail, which reads as a final stutter when
            // the unmount timer expires)
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
