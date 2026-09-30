use super::*;

impl AppView {
    /// 安装 dock 布局（构造后由 main 调用，此时 AppView 实体已就位，面板可持
    /// weak 引用）：左 dock = 侧栏，center = 会话区，右 dock = 改动面板。
    /// 锁定布局防拖拽重排、只保留调宽；右 dock 默认收起（toggle 一次）。
    pub(crate) fn install_dock(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use gpui_kit::component::dock::DockLayout;
        let app = cx.weak_entity();
        let center = cx.new(|cx| DockCenterPanel {
            app: app.clone(),
            focus_handle: cx.focus_handle(),
            _app_observer: observe_app_notify(&app, cx).expect("AppView 实体已就位"),
        });
        let right = cx.new(|cx| DockRightPanel {
            app: app.clone(),
            focus_handle: cx.focus_handle(),
            _app_observer: observe_app_notify(&app, cx).expect("AppView 实体已就位"),
        });
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
            // 右侧面板默认收起：进会话不自动显示改动
            dock.toggle_dock(DockPlacement::Right, window, cx);
        });
    }

    /// dock 开合补间的渲染侧：开合标志位（sidebar_collapsed / right_open）是
    /// 唯一事实源，翻转后登记一段宽度补间（见 [`DockSizeAnim`]），逐帧步进由
    /// [`Self::schedule_dock_anim_frames`] 的 on_next_frame 链驱动——动画帧只
    /// notify dock，与拖宽路径一致；若 notify AppView，dock 面板观察器会把
    /// cached 的中心区/右面板连带标脏，整棵树每帧重建，纯浪费。补间期间 dock
    /// 保持 open，dock_frame 自带的 overflow_hidden 裁掉出界内容，面板内容按
    /// 目标宽锚定在分隔线一侧（Sidebar::render / render_right_dock_content），
    /// 视觉上是滑出/滑入而非压缩重排，中心区宽度与面板同步连续变化（与拖宽
    /// 同观感）。动画中途反向开关：从当前实际宽重新出发。reduce_motion 直接
    /// 落终态。
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
            _ => return None,
        };
        // 本侧边缘段进行中：方向一致等待其定时器收尾（展开段收尾会开 dock
        // 并交接补间），方向相反撤段硬切。注意本函数每 render 对两侧各跑一
        // 遍，动作必须限定在本侧确实要转换的分支里，否则会把刚起步的动画
        // 在下一帧杀掉
        if let Some(edge) = self.dock_edge {
            if edge.placement == placement {
                if edge.opening == flag_open {
                    return None;
                }
                self.dock_edge = None;
                self.apply_dock_flags(placement, window, cx);
                return None;
            }
        }
        let (dock_open, current) = {
            let dock = self.dock.read(cx);
            (
                dock.is_dock_open(placement),
                dock.dock_size(placement).map(f32::from).unwrap_or(0.),
            )
        };
        // 已有本侧补间在跑且目标仍一致：继续（中途的无关 render 不得重启或
        // 清掉它——清了会让 dock 卡在半宽、被补钳硬拉回）
        if let Some(a) = anim {
            let want_to = if flag_open { target_w } else { DOCK_ANIM_MIN_W };
            if a.to == want_to {
                self.schedule_dock_anim_frames(window, cx);
                return Some(a);
            }
        }
        // 稳态：无补间且 dock 开合已与标志位一致
        if anim.is_none() && dock_open == flag_open {
            return None;
        }
        if cx.reduce_motion() {
            self.apply_dock_flags(placement, window, cx);
            return None;
        }
        // 走到这里 = 本侧确实要转换：另侧的展开边缘段还挂着（dock 延迟未开）
        // 时先替它落终态，否则其标志位悬空
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
                // 中途反向回展开（dock 已开在中间宽度）：从当前宽补间回目标
                let anim = DockSizeAnim {
                    from: current,
                    to: target_w,
                    duration: share(current, target_w),
                    start: std::time::Instant::now(),
                };
                self.schedule_dock_anim_frames(window, cx);
                return Some(anim);
            }
            // 展开前段：dock 保持关闭、中心区保持原宽，面板内容从窗口边滑入
            // 下限位置（覆盖层），收尾定时器到点开 dock 并接中段补间
            self.mount_dock_edge(placement, true, target_w, window, cx);
            return None;
        }
        // 收起（新鲜收起或从展开补间反向）：中段补间 实际宽→下限，终态关
        // dock 并挂后段滑出；时长按路程比例分摊，与前段/后段全程匀速
        let anim = DockSizeAnim {
            from: current,
            to: DOCK_ANIM_MIN_W,
            duration: share(current, DOCK_ANIM_MIN_W),
            start: std::time::Instant::now(),
        };
        self.schedule_dock_anim_frames(window, cx);
        Some(anim)
    }

    /// 挂边缘段覆盖层并起收尾定时器（+20ms 余量保证 with_animation 先走完，
    /// 早收尾会在透明层下露出双重内容）：展开段到点开 dock（下限宽）、接中
    /// 段补间（from=下限读回实际值），收起段到点仅清层。定时器按代次与标志
    /// 位双重校验，被反向抢占时安全空过。
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
                        // 标志位中途被反向则就此打住（dock 保持关闭 = 稳态）
                        let flag_open_now = match edge.placement {
                            DockPlacement::Left => !this.sidebar_collapsed,
                            _ => this.right_open,
                        };
                        if !flag_open_now {
                            cx.notify();
                            return;
                        }
                        // 开 dock（下限宽）并接中段补间，交接帧与覆盖层内容
                        // 像素一致
                        this.dock.update(cx, |dock, cx| {
                            dock.set_dock_size(edge.placement, px(DOCK_ANIM_MIN_W), window, cx);
                            dock.toggle_dock(edge.placement, window, cx);
                        });
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

    /// 边缘段覆盖层：容器固定在下限宽（100px）、贴窗口边，内容按目标宽、
    /// 从「贴分隔线的交接位」向窗口外滑动（展开）/从窗外滑到交接位（收起），
    /// 与中段补间同匀速（时长按路程比例分摊）。滑动边带 1px 分隔线色。
    pub(crate) fn render_dock_edge(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let edge = self.dock_edge?;
        let placement = edge.placement;
        let width = edge.width;
        let opening = edge.opening;
        let w = px(width);
        // 内容实体只在覆盖层出现一次：边缘段两种时序下真实 dock 都不渲染它。
        // 左=侧栏（自带 sidebar 底色）；右=面板内容（dock 皮肤平时铺的
        // tab_bar 底色这里自铺）
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
                        // 滑动边 = 虚拟分隔线（边缘段期间 dock 未开，无把手线）
                        .map(|this| match placement {
                            DockPlacement::Left => {
                                this.border_r_1().border_color(cx.theme().border)
                            }
                            _ => this.border_l_1().border_color(cx.theme().border),
                        })
                        // 默认 linear 缓动：与中段补间同匀速
                        .with_animation(
                            ("dock-edge-slide", placement as usize),
                            Animation::new(duration),
                            move |el, delta| {
                                // d = 内容从「完全出窗」到「交接位」的进度
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

    /// 注册 dock 开合动画的下一帧回调（on_next_frame 链）：回调里步进补间、
    /// 按需自续。标记位防重复排队（左右两栏同帧起步 + 链自续都走这里）。
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
                // 回调已消费，先清标记再按需自续
                this.dock_anim_frames_scheduled = false;
                if this.step_dock_anims_frame(window, cx) {
                    this.schedule_dock_anim_frames(window, cx);
                }
            });
        });
    }

    /// 补间步进一帧（左右两栏一起）：到点落终态并 notify AppView 一次（动画
    /// 期间 AppView 树冻结，落定帧让树按终态重排）；否则匀速插值写 dock 宽——
    /// 目标宽超出单帧步长封顶时按封顶走（掉帧不追帧），并取整像素（小数宽
    /// 让分界线与内容抗锯齿发虚）。只 notify dock。返回是否还有活动补间
    ///（false = 链终止）。
    pub(crate) fn step_dock_anims_frame(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let mut active = false;
        for placement in [DockPlacement::Left, DockPlacement::Right] {
            let anim = match placement {
                DockPlacement::Left => self.left_dock_anim,
                DockPlacement::Right => self.right_dock_anim,
                _ => continue,
            };
            let Some(anim) = anim else {
                continue;
            };
            let t = anim.start.elapsed().as_secs_f32() / anim.duration.as_secs_f32();
            if cx.reduce_motion() || t >= 1. {
                let (flag_open, target_w) = match placement {
                    DockPlacement::Left => (!self.sidebar_collapsed, self.sidebar_w),
                    _ => (self.right_open, self.right_w),
                };
                self.apply_dock_flags(placement, window, cx);
                match placement {
                    DockPlacement::Left => self.left_dock_anim = None,
                    DockPlacement::Right => self.right_dock_anim = None,
                    _ => {}
                }
                // 收起后段：dock 已真正关闭（中心区瞬时补宽），剩余下限宽内
                // 容由覆盖层同匀速滑出
                if !flag_open && !cx.reduce_motion() {
                    self.mount_dock_edge(placement, false, target_w, window, cx);
                }
                // 动画期间 AppView 树冻结（不逐帧重渲染），落定补一次
                cx.notify();
                continue;
            }
            // 匀速线性插值：目标位置 = from→to 按时间。步长封顶朝「目标位
            // 置」走且绝不倒退（单调向 to）——若朝 to 本身走，current 意外
            // 超前（如 set_size 下限抬高了起点）会在终点附近来回修正抖动
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

    /// 把一侧 dock 直接落到开合标志位对应的终态（跳过动画）：补间收尾、
    /// reduce_motion 共用。
    pub(crate) fn apply_dock_flags(
        &mut self,
        placement: DockPlacement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (flag_open, target_w) = match placement {
            DockPlacement::Left => (!self.sidebar_collapsed, self.sidebar_w),
            DockPlacement::Right => (self.right_open, self.right_w),
            _ => return,
        };
        let dock_open = self.dock.read(cx).is_dock_open(placement);
        self.dock.update(cx, |dock, cx| {
            if flag_open {
                if !dock_open {
                    // 先归零再 open：避免以旧宽先闪一帧
                    dock.set_dock_size(placement, px(0.), window, cx);
                    dock.toggle_dock(placement, window, cx);
                }
                // 精确落目标宽：补间最后一步按时间采样可能差几像素
                dock.set_dock_size(placement, px(target_w), window, cx);
            } else if dock_open {
                dock.toggle_dock(placement, window, cx);
                // 关闭态宽度不参与布局，写回存储值供下次展开作目标
                dock.set_dock_size(placement, px(target_w), window, cx);
            }
        });
    }

    /// 右侧面板开关（标题栏面板按钮）：展开/收起，tab 状态保留。
    /// 展开后没有激活 tab 时内容区显示面板首页（菜单页）。
    pub(crate) fn toggle_right_panel(&mut self, cx: &mut Context<Self>) {
        self.right_open = !self.right_open;
        cx.notify();
    }
}

/// 三栏最小宽度（px）：侧栏 / 中心区 / 右面板。三者之和 = 窗口最小宽 960，
/// 保证钳制区间恒非空（gpui-base 的拖拽钳制只有 PANEL_MIN_SIZE(100) 一档，
/// 没有自定义区间 API，故在 render 里补钳）。
pub(crate) const SIDEBAR_MIN_W: f32 = 200.;
pub(crate) const CENTER_MIN_W: f32 = 480.;
pub(crate) const RIGHT_PANEL_MIN_W: f32 = 280.;

/// dock 开合宽度补间（中段）：dock 保持 open，宽度按时间从 from 匀速插值到
/// to，由 on_next_frame 链每帧步进、每帧只 notify dock——与拖宽把手同一渲染
/// 路径（中心区随之连续重排，拖宽实测流畅）。匀速 + 整数像素 + 单帧步长封
/// 顶是关键：文本重排成本随单帧宽度增量超线性增长，缓动的速度峰值会让中间
/// 某几帧突发重排（「中点顿一下」的根源），拖拽正是匀速小步长才流畅；掉帧
/// 时封顶阻止追帧大步长，宁可动画略微拉长。面板内容按目标宽锚定在分隔线一
/// 侧（Sidebar::render / render_right_dock_content），配合 dock_frame 的
/// overflow_hidden 得到滑动而非压缩重排。
#[derive(Clone, Copy)]
pub(crate) struct DockSizeAnim {
    from: f32,
    to: f32,
    /// 本段时长：与首尾边缘段（DockEdgePhase）按路程比例分摊总时长，全程
    /// 匀速衔接
    duration: std::time::Duration,
    start: std::time::Instant,
}

/// dock 开合动画总时长（与拖宽一段侧栏的典型用时相当），中段补间与首尾边
/// 缘段按路程比例分摊
pub(crate) const DOCK_ANIM_DURATION: std::time::Duration = std::time::Duration::from_millis(200);

/// dock 开合单帧宽度步长封顶（px）
pub(crate) const DOCK_ANIM_MAX_STEP: f32 = 32.;

/// 补间宽度下限：gpui-base 的 `Dock::set_size` 有 PANEL_MIN_SIZE(100) 下限，
/// 开着的 dock 无法更窄（拖宽同理，拖到 100 就到头）。首尾各 100px 因此走
/// 覆盖层边缘段（见 DockEdgePhase），中段才补间真实宽度。
pub(crate) const DOCK_ANIM_MIN_W: f32 = 100.;

/// dock 开合的边缘段覆盖层：补间宽度无法触达的 首/尾 100px 由绝对定位覆盖
/// 层滑动补足（with_animation 样式补间，不失效任何视图缓存；面板内容实体
/// 只在覆盖层出现一次——两种时序下真实 dock 都不渲染它）：
/// - 展开前段（opening）：dock 保持关闭、中心区保持原宽，面板内容从窗口边
///   滑入到下限位置；收尾定时器到点才开 dock（下限宽）并无缝交接给中段补
///   间（交接帧两侧内容像素一致）；
/// - 收起后段（!opening）：中段补间到下限时已真正关 dock（中心区瞬时补宽
///   是「变宽」型重排，换行结构几乎不变、几乎不可见），覆盖层把剩余内容滑
///   出去。
#[derive(Clone, Copy)]
pub(crate) struct DockEdgePhase {
    placement: DockPlacement,
    /// true = 展开前段（滑入，结束后开 dock 接补间）；false = 收起后段（滑出）
    opening: bool,
    /// 面板内容固定宽（展开目标宽）
    width: f32,
    /// 代次：收尾定时器只处理自己这一代（可能被反向开关替换）
    generation: u32,
}

/// 三栏宽度钳制：展开的栏不低于各自最小值，且为中心区保留 CENTER_MIN_W
///（封顶 = 区域宽 - 中心最小值 - 对侧栏当前占位）。收起的栏占位为 0 不参与
/// 预算，其存储宽度原样保留，重开后由后续 render 再钳。区域宽为 0（首帧
/// 未测量）时不动作。顺序钳制——左先按右的当前占位钳，右再按钳后的左钳：
/// 单侧越界只拉回单侧，两侧同时越界（窗口缩到最小）左栏先让位，一遍收敛。
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

/// dock 中心区面板：内容回读 AppView 构建（hero 或会话列）。
/// dock 持有 panel 实体，渲染时经 weak 引用调 AppView 的 render 辅助。
pub(crate) struct DockCenterPanel {
    app: WeakEntity<AppView>,
    pub(crate) focus_handle: FocusHandle,
    _app_observer: Subscription,
}

/// dock 右侧 dock 面板：tab 栏 + 改动/菜单页内容，同样回读 AppView。
pub(crate) struct DockRightPanel {
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
