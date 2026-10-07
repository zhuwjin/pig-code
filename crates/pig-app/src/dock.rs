use super::*;

impl AppView {
    /// Install the dock layout (called by main after construction, when the
    /// AppView entity is in place and panels can hold weak references): left
    /// dock = sidebar, center = session area, right dock = changes panel. The
    /// layout is locked against drag-rearranging, keeping only width
    /// adjustment; the right dock starts collapsed (one toggle).
    pub(crate) fn install_dock(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use gpui_kit::component::dock::DockLayout;
        let app = cx.weak_entity();
        let center = cx.new(|cx| DockCenterPanel {
            app: app.clone(),
            focus_handle: cx.focus_handle(),
            _app_observer: observe_app_notify(&app, cx).expect("AppView entity should be in place"),
        });
        let right = cx.new(|cx| DockRightPanel {
            app: app.clone(),
            focus_handle: cx.focus_handle(),
            _app_observer: observe_app_notify(&app, cx).expect("AppView entity should be in place"),
        });
        // The bottom dock (terminal) panel entity lives in AppView permanently:
        // the dock fully hidden = removed, and only remounted on expansion
        // (apply_dock_flags / mount_dock_edge)
        self.dock_bottom_panel = Some(cx.new(|cx| DockBottomPanel {
            app: app.clone(),
            focus_handle: cx.focus_handle(),
            _app_observer: observe_app_notify(&app, cx).expect("AppView entity should be in place"),
        }));
        self.dock.update(cx, |dock, cx| {
            dock.set_center(
                DockLayout::tabs().panel_view(panel_handle(center), cx),
                window,
                cx,
            );
            dock.set_dock(
                DockPlacement::Left,
                DockLayout::tabs().panel_view(panel_handle(self.sidebar.clone()), cx),
                window,
                cx,
            );
            dock.set_dock_size(DockPlacement::Left, px(self.sidebar_w), window, cx);
            dock.set_dock(
                DockPlacement::Right,
                DockLayout::tabs().panel_view(panel_handle(right), cx),
                window,
                cx,
            );
            dock.set_dock_size(DockPlacement::Right, px(self.right_w), window, cx);
            dock.set_locked(true, window, cx);
            // Right panel collapsed by default: entering a session does not
            // auto-show changes
            dock.toggle_dock(DockPlacement::Right, window, cx);
        });
    }

    /// Whether the terminal panel is in a visible context (session state only;
    /// hero/no-session/settings pages do not mount the bottom dock).
    /// The bottom dock's open flag = terminal_open && terminal_visible
    pub(crate) fn terminal_visible(&self, cx: &App) -> bool {
        !self.is_hero(cx) && self.current.is_some()
    }

    /// Render side of the dock open/close tween: the open/close flags
    /// (sidebar_collapsed / right_open) are the sole source of truth; after a
    /// flip a width tween is registered (see [`DockSizeAnim`]), stepped frame by
    /// frame by [`Self::schedule_dock_anim_frames`]'s on_next_frame chain.
    /// Animation frames notify only the dock, same as the width-drag path;
    /// notifying AppView would make the dock panel observers dirty the cached
    /// center/right panels too, rebuilding the whole tree every frame — pure
    /// waste. During the tween the dock stays open, dock_frame's built-in
    /// overflow_hidden clips out-of-bounds content, and panel content anchors to
    /// the divider side at the target width (Sidebar::render /
    /// render_right_dock_content), so visually it slides out/in rather than
    /// compressing and reflowing, and the center width changes continuously in
    /// sync with the panel (same look as width dragging). Reversing the toggle
    /// mid-animation restarts from the current actual width. reduce_motion lands
    /// on the end state directly.
    pub(crate) fn step_dock_anim(
        &mut self,
        placement: DockPlacement,
        anim: Option<DockSizeAnim>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<DockSizeAnim> {
        let (flag_open, target_w) = match placement {
            DockPlacement::Left => (!self.sidebar_collapsed, self.sidebar_w),
            DockPlacement::Right => (self.right_open, self.right_w),
            // Bottom dock = terminal panel: full-hide semantics (closed =
            // removed), and visible only in session state
            DockPlacement::Bottom => (
                self.terminal_open && self.terminal_visible(cx),
                self.terminal_h,
            ),
            _ => return None,
        };
        // An edge phase is running on this side: with matching direction, wait
        // for its timer to finish (the opening phase's finisher opens the dock
        // and hands over to the tween); with opposite direction, drop the phase
        // and hard-cut. Note this function runs once per side per render, so
        // actions must stay within the branch where this side truly transitions,
        // otherwise the just-started animation gets killed on the next frame
        if let Some(edge) = self.dock_edge
            && edge.placement == placement
        {
            if edge.opening == flag_open {
                return None;
            }
            self.dock_edge = None;
            self.apply_dock_flags(placement, window, cx);
            return None;
        }
        let (dock_open, current) = {
            let dock = self.dock.read(cx);
            (
                dock.is_dock_open(placement),
                dock.dock_size(placement).map(f32::from).unwrap_or(0.),
            )
        };
        // A tween is already running on this side with a matching target:
        // continue (unrelated mid-way renders must not restart or clear it —
        // clearing would leave the dock stuck at half width and yanked back by
        // the compensation clamp)
        if let Some(a) = anim {
            let want_to = if flag_open { target_w } else { DOCK_ANIM_MIN_W };
            if a.to == want_to {
                self.schedule_dock_anim_frames(window, cx);
                return Some(a);
            }
        }
        // Steady state: no tween and the dock's open state already matches the
        // flag
        if anim.is_none() && dock_open == flag_open {
            return None;
        }
        if cx.reduce_motion() {
            self.apply_dock_flags(placement, window, cx);
            return None;
        }
        // Reaching here means this side truly transitions: if the other side's
        // opening edge phase is still pending (dock opening deferred), land its
        // end state first, otherwise its flag is left dangling
        if let Some(old) = self.dock_edge {
            if old.opening {
                self.apply_dock_flags(old.placement, window, cx);
            }
            self.dock_edge = None;
        }
        let share = |from: f32, to: f32| {
            DOCK_ANIM_DURATION.mul_f32((to - from).abs().max(1.) / target_w.max(1.))
        };
        if flag_open {
            if dock_open {
                // Reversed back to expanding mid-way (dock already open at an
                // intermediate width): tween from the current width back to the
                // target
                let anim = DockSizeAnim {
                    from: current,
                    to: target_w,
                    duration: share(current, target_w),
                    start: std::time::Instant::now(),
                };
                self.schedule_dock_anim_frames(window, cx);
                return Some(anim);
            }
            // Opening edge phase: the dock stays closed and the center keeps its
            // width while panel content slides in from the window edge to the
            // lower-bound position (overlay); the finisher timer then opens the
            // dock and continues with the mid tween
            self.mount_dock_edge(placement, true, target_w, window, cx);
            return None;
        }
        // Collapsing (fresh collapse or reversal from an expansion tween): the
        // mid tween runs actual width → lower bound, and the end state closes
        // the dock and mounts the closing slide-out; duration is apportioned by
        // distance ratio, uniform with the leading/trailing phases across the
        // whole motion
        let anim = DockSizeAnim {
            from: current,
            to: DOCK_ANIM_MIN_W,
            duration: share(current, DOCK_ANIM_MIN_W),
            start: std::time::Instant::now(),
        };
        self.schedule_dock_anim_frames(window, cx);
        Some(anim)
    }

    /// Mount the edge phase overlay and start its finisher timer (+20ms
    /// headroom so with_animation finishes first; finishing early would expose
    /// doubled content under the transparent layer): when the opening phase
    /// expires it opens the dock (at lower-bound width) and continues with the
    /// mid tween (from = lower bound read back as the actual value); when the
    /// closing phase expires it only clears the layer. The timer is
    /// double-checked against generation and flag, safely turning into a no-op
    /// when preempted by a reversal.
    pub(crate) fn mount_dock_edge(
        &mut self,
        placement: DockPlacement,
        opening: bool,
        width: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let generation = self.dock_edge_generation;
        self.dock_edge_generation += 1;
        self.dock_edge = Some(DockEdgePhase {
            placement,
            opening,
            width,
            generation,
        });
        let expiry = DOCK_ANIM_DURATION.mul_f32(DOCK_ANIM_MIN_W / width.max(1.))
            + std::time::Duration::from_millis(20);
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(expiry).await;
            let _ = cx.update(|window, cx| {
                let _ = this.update(cx, |this, cx| {
                    let Some(edge) = this.dock_edge else {
                        return;
                    };
                    if edge.generation != generation {
                        return;
                    }
                    this.dock_edge = None;
                    if edge.opening {
                        // If the flag was reversed mid-way, stop right here
                        // (dock stays closed = steady state)
                        let flag_open_now = match edge.placement {
                            DockPlacement::Left => !this.sidebar_collapsed,
                            DockPlacement::Right => this.right_open,
                            DockPlacement::Bottom => {
                                this.terminal_open && this.terminal_visible(cx)
                            }
                            _ => unreachable!("edge only originates from Left/Right/Bottom"),
                        };
                        if !flag_open_now {
                            cx.notify();
                            return;
                        }
                        // Open the dock (at lower-bound width) and continue with
                        // the mid tween; the handover frame matches the overlay
                        // content pixel for pixel. The bottom dock is removed
                        // when closed, so remount it first
                        if edge.placement == DockPlacement::Bottom
                            && !this.dock.read(cx).has_dock(edge.placement)
                        {
                            use gpui_kit::component::dock::DockLayout;
                            if let Some(panel) = this.dock_bottom_panel.clone() {
                                this.dock.update(cx, |dock, cx| {
                                    dock.set_dock(
                                        edge.placement,
                                        DockLayout::tabs().panel_view(panel_handle(panel), cx),
                                        window,
                                        cx,
                                    );
                                    dock.set_dock_collapsible(edge.placement, false, window, cx);
                                    dock.set_dock_size(
                                        edge.placement,
                                        px(DOCK_ANIM_MIN_W),
                                        window,
                                        cx,
                                    );
                                });
                            }
                        } else {
                            this.dock.update(cx, |dock, cx| {
                                dock.set_dock_size(edge.placement, px(DOCK_ANIM_MIN_W), window, cx);
                                dock.toggle_dock(edge.placement, window, cx);
                            });
                        }
                        let anim = DockSizeAnim {
                            from: DOCK_ANIM_MIN_W,
                            to: edge.width,
                            duration: DOCK_ANIM_DURATION.mul_f32(
                                (edge.width - DOCK_ANIM_MIN_W).max(1.) / edge.width.max(1.),
                            ),
                            start: std::time::Instant::now(),
                        };
                        match edge.placement {
                            DockPlacement::Left => this.left_dock_anim = Some(anim),
                            DockPlacement::Right => this.right_dock_anim = Some(anim),
                            DockPlacement::Bottom => this.bottom_dock_anim = Some(anim),
                            _ => {}
                        }
                        this.schedule_dock_anim_frames(window, cx);
                    }
                    cx.notify();
                });
            });
        })
        .detach();
    }

    /// Edge phase overlay: the container is fixed at the lower-bound width
    /// (100px) and flush with the window edge; the content, sized to the target
    /// width, slides from the divider-aligned handover position out of the
    /// window (opening), or from outside the window in to the handover position
    /// (closing), at the same uniform speed as the mid tween (duration
    /// apportioned by distance ratio). The sliding edge carries a 1px divider
    /// color.
    pub(crate) fn render_dock_edge(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let edge = self.dock_edge?;
        let placement = edge.placement;
        let width = edge.width;
        let opening = edge.opening;
        // The bottom dock uses the vertical slide variant (the container is a
        // horizontal band at the bottom of the center column, not spanning the
        // left/right docks)
        if placement == DockPlacement::Bottom {
            return Some(self.render_bottom_dock_edge(width, opening, window, cx));
        }
        let w = px(width);
        // The content entity appears only once, in the overlay: under both edge
        // phase timings the real dock does not render it. Left = sidebar
        // (carries its own sidebar background); right = panel content (spreads
        // the tab_bar background itself, which the dock skin normally spreads)
        let (bg, content): (Hsla, AnyElement) = match placement {
            DockPlacement::Left => (cx.theme().sidebar, self.sidebar.clone().into_any_element()),
            _ => (
                *cx.theme().tokens.tab_bar,
                self.render_right_dock_content(window, cx),
            ),
        };
        let duration = DOCK_ANIM_DURATION.mul_f32(DOCK_ANIM_MIN_W / width.max(1.));
        let slide_left = placement == DockPlacement::Left;
        Some(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .map(|this| match placement {
                    DockPlacement::Left => this.left_0(),
                    _ => this.right_0(),
                })
                .w(px(DOCK_ANIM_MIN_W))
                .overflow_hidden()
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left_0()
                        .w(w)
                        .bg(bg)
                        // Sliding edge = virtual divider (during the edge phase
                        // the dock is not open, no handle line)
                        .map(|this| match placement {
                            DockPlacement::Left => {
                                this.border_r_1().border_color(cx.theme().border)
                            }
                            _ => this.border_l_1().border_color(cx.theme().border),
                        })
                        // Default linear easing: same uniform speed as the mid
                        // tween
                        .with_animation(
                            ("dock-edge-slide", placement as usize),
                            Animation::new(duration),
                            move |el, delta| {
                                // d = content progress from "fully outside the
                                // window" to the "handover position"
                                let d = if opening { delta } else { 1. - delta };
                                if slide_left {
                                    el.left(px(-width + DOCK_ANIM_MIN_W * d))
                                } else {
                                    el.left(px(DOCK_ANIM_MIN_W * (1. - d)))
                                }
                            },
                        )
                        .child(content),
                )
                .into_any_element(),
        )
    }

    /// Bottom dock panel content (terminal panel).
    /// During open/close tweens/edge phases: content holds the target height
    /// anchored to the top while the dock frame/overlay only clips (zero
    /// terminal grid resizes, avoiding per-frame SIGWINCH redraws); in the
    /// steady state (including official handle drags): fill the dock frame,
    /// with the terminal reflowing live (real terminal drag semantics)
    pub(crate) fn render_bottom_dock_content(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(panel) = self.terminal.clone() else {
            return div().into_any_element();
        };
        let animating = self.bottom_dock_anim.is_some()
            || matches!(self.dock_edge, Some(e) if e.placement == DockPlacement::Bottom);
        if animating {
            div()
                .size_full()
                .child(div().w_full().h(px(self.terminal_h)).child(panel))
                .into_any_element()
        } else {
            div().size_full().child(panel).into_any_element()
        }
    }

    /// Bottom dock edge phase overlay (vertical variant): the container is a
    /// 100px horizontal band at the bottom of the center column (yielding to
    /// the left/right docks' current extents); the content (terminal panel,
    /// fixed target height) slides vertically from the window bottom to the
    /// handover position, at the same uniform speed and duration apportionment
    /// as the horizontal variant. The top edge carries a 1px divider
    fn render_bottom_dock_edge(
        &mut self,
        target_h: f32,
        opening: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Left/right insets take the left/right docks' current actual extents
        // (transition values during their tweens/edge phases, an approximation
        // suffices)
        let (left_inset, right_inset) = {
            let dock = self.dock.read(cx);
            let inset = |p: DockPlacement| {
                if dock.is_dock_open(p) {
                    dock.dock_size(p).map(f32::from).unwrap_or(0.)
                } else {
                    0.
                }
            };
            (inset(DockPlacement::Left), inset(DockPlacement::Right))
        };
        let duration = DOCK_ANIM_DURATION.mul_f32(DOCK_ANIM_MIN_W / target_h.max(1.));
        let content = self.render_bottom_dock_content(window, cx);
        div()
            .absolute()
            .bottom_0()
            .left(px(left_inset))
            .right(px(right_inset))
            .h(px(DOCK_ANIM_MIN_W))
            .overflow_hidden()
            .child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .top_0()
                    .h(px(target_h))
                    .bg(cx.theme().background)
                    // Sliding edge = virtual divider (during the edge phase the
                    // dock is not mounted, no handle line)
                    .border_t_1()
                    .border_color(cx.theme().border)
                    // Default linear easing: same uniform speed as the mid tween
                    .with_animation(
                        ("dock-edge-slide", DockPlacement::Bottom as usize),
                        Animation::new(duration),
                        move |el, delta| {
                            // d = content progress from "fully outside the
                            // window" to the "handover position"
                            let d = if opening { delta } else { 1. - delta };
                            el.top(px(DOCK_ANIM_MIN_W * (1. - d)))
                        },
                    )
                    .child(content),
            )
            .into_any_element()
    }

    /// Register the next-frame callback for dock open/close animations (an
    /// on_next_frame chain): the callback steps the tweens and self-continues as
    /// needed. The flag prevents duplicate queuing (both panes starting on the
    /// same frame plus chain self-continuation go through here).
    pub(crate) fn schedule_dock_anim_frames(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.dock_anim_frames_scheduled {
            return;
        }
        self.dock_anim_frames_scheduled = true;
        let app = cx.weak_entity();
        window.on_next_frame(move |window, cx| {
            let Some(app) = app.upgrade() else { return };
            app.update(cx, |this, cx| {
                // The callback is consumed: clear the flag first, then
                // self-continue as needed
                this.dock_anim_frames_scheduled = false;
                if this.step_dock_anims_frame(window, cx) {
                    this.schedule_dock_anim_frames(window, cx);
                }
            });
        });
    }

    /// Step the tweens one frame (both panes together): on expiry, land the end
    /// state and notify AppView once (the AppView tree is frozen during the
    /// animation; the landing frame reflows the tree at the end state);
    /// otherwise interpolate at uniform speed and write the dock width — when
    /// the target exceeds the per-frame step cap, go by the cap (dropped frames
    /// are not chased), and round to whole pixels (fractional widths make the
    /// divider and content fuzzy with antialiasing). Notifies the dock only.
    /// Returns whether any tween is still active (false = chain terminates).
    pub(crate) fn step_dock_anims_frame(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let mut active = false;
        for placement in [
            DockPlacement::Left,
            DockPlacement::Right,
            DockPlacement::Bottom,
        ] {
            let anim = match placement {
                DockPlacement::Left => self.left_dock_anim,
                DockPlacement::Right => self.right_dock_anim,
                DockPlacement::Bottom => self.bottom_dock_anim,
                _ => continue,
            };
            let Some(anim) = anim else {
                continue;
            };
            let t = anim.start.elapsed().as_secs_f32() / anim.duration.as_secs_f32();
            if cx.reduce_motion() || t >= 1. {
                let (flag_open, target_w) = match placement {
                    DockPlacement::Left => (!self.sidebar_collapsed, self.sidebar_w),
                    DockPlacement::Right => (self.right_open, self.right_w),
                    DockPlacement::Bottom => (
                        self.terminal_open && self.terminal_visible(cx),
                        self.terminal_h,
                    ),
                    _ => continue,
                };
                self.apply_dock_flags(placement, window, cx);
                match placement {
                    DockPlacement::Left => self.left_dock_anim = None,
                    DockPlacement::Right => self.right_dock_anim = None,
                    DockPlacement::Bottom => self.bottom_dock_anim = None,
                    _ => {}
                }
                // Closing phase: the dock is already truly closed (the center
                // widens back instantly), and the remaining lower-bound-width
                // content slides out via the overlay at the same uniform speed
                if !flag_open && !cx.reduce_motion() {
                    self.mount_dock_edge(placement, false, target_w, window, cx);
                }
                // The AppView tree is frozen during the animation (no per-frame
                // re-render); re-render once on landing
                cx.notify();
                continue;
            }
            // Uniform linear interpolation: target position = from→to by time.
            // The step cap moves toward the "target position" and never
            // regresses (monotonic toward to) — if it moved toward to itself, a
            // current that unexpectedly runs ahead (e.g. set_size's lower bound
            // raised the start) would jitter back and forth with corrections
            // near the end
            let target = anim.from + (anim.to - anim.from) * t;
            let current = self
                .dock
                .read(cx)
                .dock_size(placement)
                .map(f32::from)
                .unwrap_or(0.);
            let dir = (anim.to - anim.from).signum();
            let proposed = if (target - current).abs() > DOCK_ANIM_MAX_STEP {
                current + dir * DOCK_ANIM_MAX_STEP
            } else {
                target
            };
            let size = if dir > 0. {
                proposed.max(current)
            } else {
                proposed.min(current)
            };
            self.dock.update(cx, |dock, cx| {
                dock.set_dock_size(placement, px(size.round()), window, cx);
            });
            active = true;
        }
        active
    }

    /// Land a dock side directly at the end state matching its open/close flag
    /// (skipping the animation): shared by tween finishing and reduce_motion.
    pub(crate) fn apply_dock_flags(
        &mut self,
        placement: DockPlacement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use gpui_kit::component::dock::DockLayout;
        let (flag_open, target_w) = match placement {
            DockPlacement::Left => (!self.sidebar_collapsed, self.sidebar_w),
            DockPlacement::Right => (self.right_open, self.right_w),
            DockPlacement::Bottom => (
                self.terminal_open && self.terminal_visible(cx),
                self.terminal_h,
            ),
            _ => return,
        };
        let dock_open = self.dock.read(cx).is_dock_open(placement);
        let bottom_panel = self.dock_bottom_panel.clone();
        self.dock.update(cx, |dock, cx| {
            if flag_open {
                if placement == DockPlacement::Bottom {
                    // Bottom dock full-hide semantics: closed = removed,
                    // expansion must remount first (the panel entity lives in
                    // AppView; set_dock + set_size within the same frame avoids
                    // flashing the old height)
                    if !dock.has_dock(placement) {
                        let Some(panel) = bottom_panel else {
                            return;
                        };
                        dock.set_dock(
                            placement,
                            DockLayout::tabs().panel_view(panel_handle(panel), cx),
                            window,
                            cx,
                        );
                        // The upstream "drag to minimum collapses into a 29px
                        // strip" gesture conflicts with the full-hide model;
                        // disable it
                        dock.set_dock_collapsible(placement, false, window, cx);
                    }
                    dock.set_dock_size(placement, px(target_w), window, cx);
                    return;
                }
                if !dock_open {
                    // Zero the size before opening: avoid one frame flashing at
                    // the old width
                    dock.set_dock_size(placement, px(0.), window, cx);
                    dock.toggle_dock(placement, window, cx);
                }
                // Land exactly on the target width: the tween's last
                // time-sampled step may be off by a few pixels
                dock.set_dock_size(placement, px(target_w), window, cx);
            } else if placement == DockPlacement::Bottom {
                if dock.has_dock(placement) {
                    // Full hide = remove (not toggle: a closed bottom dock
                    // leaves a 29px collapsed strip)
                    dock.remove_dock(placement, window, cx);
                }
            } else if dock_open {
                dock.toggle_dock(placement, window, cx);
                // The closed width does not participate in layout; write the
                // stored value back as the next expansion's target
                dock.set_dock_size(placement, px(target_w), window, cx);
            }
        });
    }

    /// Right panel toggle (title bar panel button): expand/collapse, tab state
    /// is preserved. When expanded with no active tab, the content area shows
    /// the panel home page (menu page).
    pub(crate) fn toggle_right_panel(&mut self, cx: &mut Context<Self>) {
        self.right_open = !self.right_open;
        cx.notify();
    }
}

/// Three-pane minimum widths (px): sidebar / center / right panel. Their sum
/// equals the 960 minimum window width, keeping the clamp interval never empty
/// (gpui-base's drag clamping has only the single PANEL_MIN_SIZE(100) tier and
/// no custom-interval API, hence the extra clamp in render).
pub(crate) const SIDEBAR_MIN_W: f32 = 200.;
pub(crate) const CENTER_MIN_W: f32 = 480.;
pub(crate) const RIGHT_PANEL_MIN_W: f32 = 280.;

/// Dock open/close width tween (mid phase): the dock stays open while the
/// width interpolates uniformly over time from from to to, stepped every frame
/// by the on_next_frame chain, notifying only the dock — the same render path
/// as the drag handle (the center reflows continuously along with it; width
/// dragging is measured smooth). Uniform speed + whole pixels + a per-frame
/// step cap are the key: text reflow cost grows superlinearly with the
/// per-frame width delta, and an eased speed peak would trigger bursts of
/// reflow on a few middle frames (the root of the "midpoint hiccup"); dragging
/// is smooth precisely because it is uniform with small steps. When frames
/// drop, the cap prevents large catch-up steps — better to let the animation
/// stretch slightly. Panel content anchors to the divider side at the target
/// width (Sidebar::render / render_right_dock_content), which together with
/// dock_frame's overflow_hidden yields a slide rather than a compressing
/// reflow.
#[derive(Clone, Copy)]
pub(crate) struct DockSizeAnim {
    from: f32,
    to: f32,
    /// Duration of this phase: the total duration is apportioned by distance
    /// ratio with the leading/trailing edge phases (DockEdgePhase), joining
    /// uniformly across the whole motion
    duration: std::time::Duration,
    start: std::time::Instant,
}

/// Total duration of dock open/close animations (comparable to typically
/// dragging a sidebar through one stretch); the mid tween and the
/// leading/trailing edge phases apportion it by distance ratio
pub(crate) const DOCK_ANIM_DURATION: std::time::Duration = std::time::Duration::from_millis(200);

/// Per-frame width step cap for dock open/close (px)
pub(crate) const DOCK_ANIM_MAX_STEP: f32 = 32.;

/// Lower bound of the tween width: gpui-base's `Dock::set_size` enforces a
/// PANEL_MIN_SIZE(100) floor, so an open dock cannot be narrower (same for
/// width drags — 100 is the end of the road). The first and last 100px
/// therefore go through the overlay edge phases (see DockEdgePhase), and only
/// the middle phase tweens the real width.
pub(crate) const DOCK_ANIM_MIN_W: f32 = 100.;

/// Dock open/close edge phase overlay: the first/last 100px that the width
/// tween cannot reach are covered by an absolutely positioned overlay slide (a
/// with_animation style tween that invalidates no view caches; the panel
/// content entity appears only once, in the overlay — under both timings the
/// real dock does not render it):
/// - Opening phase (opening): the dock stays closed and the center keeps its
///   width while panel content slides in from the window edge to the
///   lower-bound position; the finisher timer then opens the dock (at
///   lower-bound width) and hands over seamlessly to the mid tween (the
///   handover frame matches content on both sides pixel for pixel);
/// - Closing phase (!opening): when the mid tween reaches the lower bound the
///   dock is already truly closed (the center instantly widening back is a
///   "widen"-type reflow whose line structure barely changes, nearly
///   invisible), and the overlay slides the remaining content out.
#[derive(Clone, Copy)]
pub(crate) struct DockEdgePhase {
    placement: DockPlacement,
    /// true = opening phase (slides in; opens the dock afterwards to continue
    /// the tween); false = closing phase (slides out)
    opening: bool,
    /// Fixed panel content width (the expansion target width)
    width: f32,
    /// Generation: the finisher timer only handles its own generation (it may
    /// be replaced by a reversed toggle)
    generation: u32,
}

/// Three-pane width clamping: expanded panes stay above their minimums and
/// reserve CENTER_MIN_W for the center (cap = area width - center minimum -
/// the opposite pane's current extent). Collapsed panes have zero extent,
/// skip the budget, and keep their stored widths as-is, re-clamped by a later
/// render after reopening. No-op when the area width is 0 (unmeasured first
/// frame). Sequential clamping — the left clamps first against the right's
/// current extent, then the right clamps against the clamped left: a single
/// out-of-range side pulls back only itself, and when both are out of range
/// (window shrunk to minimum) the left pane yields first, converging in one
/// pass.
pub(crate) fn clamp_dock_widths(
    area: f32,
    left: f32,
    right: f32,
    left_open: bool,
    right_open: bool,
) -> (f32, f32) {
    if area <= 0. {
        return (left, right);
    }
    let cap = |min: f32, opposite_extent: f32| (area - CENTER_MIN_W - opposite_extent).max(min);
    let new_left = if left_open {
        left.clamp(
            SIDEBAR_MIN_W,
            cap(SIDEBAR_MIN_W, if right_open { right } else { 0. }),
        )
    } else {
        left
    };
    let new_right = if right_open {
        right.clamp(
            RIGHT_PANEL_MIN_W,
            cap(RIGHT_PANEL_MIN_W, if left_open { new_left } else { 0. }),
        )
    } else {
        right
    };
    (new_left, new_right)
}

/// Dock center panel: content is built by reading back AppView (hero or the
/// session column). The dock holds the panel entity; rendering calls AppView's
/// render helpers through a weak reference.
pub(crate) struct DockCenterPanel {
    app: WeakEntity<AppView>,
    pub(crate) focus_handle: FocusHandle,
    _app_observer: Subscription,
}

/// Dock right panel: tab bar + changes/menu page content, also reading back
/// AppView.
pub(crate) struct DockRightPanel {
    app: WeakEntity<AppView>,
    pub(crate) focus_handle: FocusHandle,
    _app_observer: Subscription,
}

/// Dock bottom panel: terminal panel container, also reading back AppView.
/// Unlike the left/right docks, the bottom dock uses "full hide" semantics —
/// closing means remove_dock (upstream toggle close would leave a 29px
/// collapsed strip, mismatching the title bar button's open/close model), so
/// the panel entity lives in AppView and is mounted into the dock only when
/// expanded
pub(crate) struct DockBottomPanel {
    app: WeakEntity<AppView>,
    pub(crate) focus_handle: FocusHandle,
    _app_observer: Subscription,
}

impl Render for DockCenterPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.app
            .update(cx, |app, cx| app.render_center(cx))
            .unwrap_or_else(|_| div().into_any_element())
    }
}

impl Render for DockRightPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.app
            .update(cx, |app, cx| app.render_right_dock_content(window, cx))
            .unwrap_or_else(|_| div().into_any_element())
    }
}

impl Render for DockBottomPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.app
            .update(cx, |app, cx| app.render_bottom_dock_content(window, cx))
            .unwrap_or_else(|_| div().into_any_element())
    }
}

#[cfg(test)]
mod tests {
    // Note: no `use super::*` — the super chain imports gpui's `test` macro,
    // which shadows the built-in #[test] and makes it expand recursively (same
    // story for the use super::* at the top of this file, hence this module
    // imports everything explicitly)
    use gpui_kit::component::dock::{
        BasePanel, DockArea, DockLayout, DockPlacement, DockSkin, Panel, PanelEvent, panel_handle,
    };
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
        ParentElement as _, Render, Styled as _, Window, div, px,
    };

    /// Minimal panel reproducing "bottom dock handle won't drag" (all chrome
    /// off, same as impl_dock_panel)
    struct DummyPanel {
        focus_handle: FocusHandle,
    }

    impl Render for DummyPanel {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child("dummy")
        }
    }

    impl Focusable for DummyPanel {
        fn focus_handle(&self, _: &App) -> FocusHandle {
            self.focus_handle.clone()
        }
    }

    impl EventEmitter<PanelEvent> for DummyPanel {}

    impl BasePanel for DummyPanel {
        fn panel_name(&self) -> &'static str {
            "dummy"
        }
    }

    impl Panel for DummyPanel {
        fn title_bar(&self, _: &App) -> bool {
            false
        }

        fn inner_padding(&self, _: &App) -> bool {
            false
        }
    }

    /// Bottom dock installation parameters exactly matching AppView (locked +
    /// collapsible(false))
    fn build_dock(window: &mut Window, cx: &mut App) -> Entity<DockArea> {
        let (dock, _skin) = DockSkin::dock_area("test-dock", None, window, cx);
        let center = cx.new(|cx| DummyPanel {
            focus_handle: cx.focus_handle(),
        });
        let left = cx.new(|cx| DummyPanel {
            focus_handle: cx.focus_handle(),
        });
        let bottom = cx.new(|cx| DummyPanel {
            focus_handle: cx.focus_handle(),
        });
        dock.update(cx, |dock, cx| {
            dock.set_center(
                DockLayout::tabs().panel_view(panel_handle(center), cx),
                window,
                cx,
            );
            // Left dock as the control (the sidebar handle is draggable in the
            // App)
            dock.set_dock(
                DockPlacement::Left,
                DockLayout::tabs().panel_view(panel_handle(left), cx),
                window,
                cx,
            );
            dock.set_dock_size(DockPlacement::Left, px(220.), window, cx);
            dock.set_dock(
                DockPlacement::Bottom,
                DockLayout::tabs().panel_view(panel_handle(bottom), cx),
                window,
                cx,
            );
            dock.set_dock_size(DockPlacement::Bottom, px(300.), window, cx);
            // Match AppView: lock the layout + disable "drag to minimum
            // collapses" for the bottom
            dock.set_locked(true, window, cx);
            dock.set_dock_collapsible(DockPlacement::Bottom, false, window, cx);
        });
        dock
    }

    /// Window root view: holds the dock entity (TestAppContext::open_window
    /// requires the closure to return a Render value; an Entity cannot be
    /// given directly)
    struct DockProbe {
        dock: Entity<DockArea>,
    }

    impl Render for DockProbe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            self.dock.clone()
        }
    }

    /// Close to the App's shape: the dock sits inside a "title bar + flex_1
    /// container" and the bottom dock is lazily mounted only after the first
    /// frame (simulating the terminal's first expansion); the handle should
    /// still drag
    #[test]
    fn bottom_dock_handle_drags_when_mounted_late() {
        let cx = &mut gpui_kit::TestAppContext::single();
        cx.update(gpui_kit::init);
        let window = cx.open_window(gpui_kit::size(px(800.), px(600.)), |window, cx| {
            let (dock, _skin) = DockSkin::dock_area("test-dock-late", None, window, cx);
            let center = cx.new(|cx| DummyPanel {
                focus_handle: cx.focus_handle(),
            });
            dock.update(cx, |dock, cx| {
                dock.set_center(
                    DockLayout::tabs().panel_view(panel_handle(center), cx),
                    window,
                    cx,
                );
                dock.set_locked(true, window, cx);
            });
            DockProbe { dock }
        });
        let dock = window
            .update(cx, |probe, _, _| probe.dock.clone())
            .expect("window alive");
        // First frame: no bottom dock
        cx.update_window(window.into(), |_, window, cx| {
            window.render_frame(cx);
        })
        .expect("window alive");
        // Lazily mount the bottom dock (same path as the App's first
        // expansion) + disable collapsibility
        cx.update_window(window.into(), |_, window, cx| {
            let panel = cx.new(|cx| DummyPanel {
                focus_handle: cx.focus_handle(),
            });
            dock.update(cx, |dock, cx| {
                dock.set_dock(
                    DockPlacement::Bottom,
                    DockLayout::tabs().panel_view(panel_handle(panel), cx),
                    window,
                    cx,
                );
                dock.set_dock_collapsible(DockPlacement::Bottom, false, window, cx);
                dock.set_dock_size(DockPlacement::Bottom, px(300.), window, cx);
            });
            window.render_frame(cx);
            let before = dock.read(cx).dock_size(DockPlacement::Bottom);
            assert!(before.is_some(), "bottom dock should be mounted");
            // Window is 600 tall, the dock area fills the window (no title
            // bar), the 300-tall bottom dock occupies [300,600], and the
            // handle's 5px snap-to-top band is at its top edge. Drag up 80px
            // from inside the band
            window.drag(
                gpui_kit::point(px(400.), px(302.)),
                gpui_kit::point(px(400.), px(222.)),
                cx,
            );
            let after = dock.read(cx).dock_size(DockPlacement::Bottom);
            assert!(
                after > before,
                "drag after lazy mount should still change height: {before:?} -> {after:?}"
            );
        })
        .expect("window alive");
    }

    /// Verify that the official handle drag changes the dock height
    /// (reproducing "bottom dock handle won't drag"). No #[gpui_kit::test]
    /// macro: importing gpui's test macro via glob in this module would shadow
    /// the built-in #[test] and make it expand recursively (see the module
    /// header note), so drive TestAppContext manually
    #[test]
    fn bottom_dock_handle_drags() {
        let cx = &mut gpui_kit::TestAppContext::single();
        cx.update(gpui_kit::init);
        let window = cx.open_window(gpui_kit::size(px(800.), px(600.)), |window, cx| DockProbe {
            dock: build_dock(window, cx),
        });
        let dock = window
            .update(cx, |probe, _, _| probe.dock.clone())
            .expect("window alive");
        cx.update_window(window.into(), |_, window, cx| {
            window.render_frame(cx);
            let before = dock.read(cx).dock_size(DockPlacement::Bottom);
            // The 300-tall bottom dock occupies window [300,600], and the
            // handle's 5px snap-to-top band is at its top edge. Drag up 80px
            // from inside the band at (400,302): the dock should get taller
            // (measured from the window bottom)
            window.drag(
                gpui_kit::point(px(400.), px(302.)),
                gpui_kit::point(px(400.), px(222.)),
                cx,
            );
            let after = dock.read(cx).dock_size(DockPlacement::Bottom);
            assert!(
                after > before,
                "dock height should increase after drag: {before:?} -> {after:?}"
            );
        })
        .expect("window alive");
    }
}
