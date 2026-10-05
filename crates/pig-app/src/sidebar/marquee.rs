use super::*;

impl Sidebar {
    /// 悬停超宽标题：启动跑马灯，把文字缓慢滚到末尾再折返
    pub(crate) fn begin_title_marquee(&mut self, session_id: &str, cx: &mut Context<Self>) {
        let Some(handle) = self.title_scrolls.borrow().get(session_id).cloned() else {
            return;
        };
        // 未超宽（无横向溢出）不必滚动
        if f32::from(handle.max_offset().x) <= 1.0 {
            return;
        }
        if self
            .marquee
            .as_ref()
            .is_some_and(|m| m.session_id == session_id)
        {
            return;
        }
        self.marquee = Some(TitleMarquee {
            session_id: session_id.to_string(),
            position: 0.0,
            forward: true,
            hold_ticks: MARQUEE_START_TICKS,
        });
        let session_id = session_id.to_string();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(16))
                    .await;
                match this.update(cx, |this, cx| this.tick_title_marquee(&session_id, cx)) {
                    Ok(true) => {}
                    _ => break,
                }
            }
        })
        .detach();
    }

    pub(crate) fn end_title_marquee(&mut self, session_id: &str, cx: &mut Context<Self>) {
        if self
            .marquee
            .as_ref()
            .is_some_and(|m| m.session_id == session_id)
        {
            self.marquee = None;
        }
        if let Some(handle) = self.title_scrolls.borrow().get(session_id)
            && handle.offset().x != px(0.)
        {
            handle.set_offset(point(px(0.), px(0.)));
        }
        cx.notify();
    }

    /// 跑马灯推进一步；返回 false 表示循环该停了
    pub(crate) fn tick_title_marquee(&mut self, session_id: &str, cx: &mut Context<Self>) -> bool {
        if self
            .marquee
            .as_ref()
            .is_none_or(|m| m.session_id != session_id)
        {
            return false;
        }
        let Some(handle) = self.title_scrolls.borrow().get(session_id).cloned() else {
            self.marquee = None;
            return false;
        };
        let max = f32::from(handle.max_offset().x);
        if max <= 1.0 {
            handle.set_offset(point(px(0.), px(0.)));
            self.marquee = None;
            cx.notify();
            return false;
        }
        let marquee = self.marquee.as_mut().expect("checked above");
        if marquee.hold_ticks > 0 {
            marquee.hold_ticks -= 1;
            return true;
        }
        if marquee.forward {
            marquee.position += 1.5;
            if marquee.position >= max {
                marquee.position = max;
                marquee.forward = false;
                marquee.hold_ticks = MARQUEE_HOLD_TICKS;
            }
        } else {
            marquee.position -= 3.0;
            if marquee.position <= 0.0 {
                marquee.position = 0.0;
                marquee.forward = true;
                marquee.hold_ticks = MARQUEE_HOLD_TICKS;
            }
        }
        handle.set_offset(point(px(-marquee.position), px(0.)));
        cx.notify();
        true
    }

    /// 用文本系统量出标题单行渲染宽度。
    ///
    /// gpui 的文本测量会把宽度钳制进可用空间，导致 ScrollHandle 感知不到
    /// 溢出（实测 max_offset 恒为 0）；给内容显式真实宽度后滚动机制才生效。
    pub(crate) fn measure_title_width(title: &str, window: &Window, cx: &App) -> Pixels {
        let font_size = rems(0.875).to_pixels(window.rem_size());
        let font = Font {
            family: cx.theme().font_family.clone(),
            ..Font::default()
        };
        window
            .text_system()
            .shape_line(
                SharedString::from(title.to_string()),
                font_size,
                &[TextRun {
                    len: title.len(),
                    font,
                    color: black(),
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                }],
                None,
            )
            .width
    }

    /// 会话标题端部的渐隐条：base 为行背景实色（常态 sidebar / 选中 accent），
    /// tint 为悬停叠加层（accent 60%）；叠加后与行背景合成一致，尾端无色差
    pub(crate) fn title_fade(leading: bool, base: Hsla, tint: Option<Hsla>) -> Div {
        let (from, to) = if leading {
            (base, base.opacity(0.))
        } else {
            (base.opacity(0.), base)
        };
        let fade = div()
            .absolute()
            .top_0()
            .bottom_0()
            .when(leading, |this| this.left_0())
            .when(!leading, |this| this.right_0())
            .w(px(20.))
            .bg(linear_gradient(
                90.,
                linear_color_stop(from, 0.),
                linear_color_stop(to, 1.),
            ));
        match tint {
            Some(tint) => {
                let (from, to) = if leading {
                    (tint, tint.opacity(0.))
                } else {
                    (tint.opacity(0.), tint)
                };
                fade.child(div().size_full().bg(linear_gradient(
                    90.,
                    linear_color_stop(from, 0.),
                    linear_color_stop(to, 1.),
                )))
            }
            None => fade,
        }
    }

    /// 会话标题区：横向滚动（悬停跑马灯）+ 两端渐隐。key 为 title_scrolls
    /// 的键：单行会话行用会话 id，双行详情行用 "pinned-{id}"（同一会话在两
    /// 种行的滚动状态互不干扰）
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_title_scroll(
        &self,
        key: &str,
        element_id: impl Into<ElementId>,
        title: &str,
        fade_base: Hsla,
        fade_tint: Option<Hsla>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let title_handle = self
            .title_scrolls
            .borrow_mut()
            .entry(key.to_string())
            .or_default()
            .clone();
        let hover_key = key.to_string();
        // 显式真实宽度（+2px 余量防字宽取整误差），把溢出撑给 ScrollHandle
        let title_width = Self::measure_title_width(title, window, cx) + px(2.);
        // 渐隐显隐跟滚动位置（thread_view 思考滚动行同款）：右端还有未露出
        // 的文字才渐隐，跑马灯滚出开头后左端也渐隐
        let max = title_handle.max_offset().x;
        let offset = title_handle.offset().x;
        let hides_leading = max > px(1.) && offset < px(-1.);
        let hides_trailing = max > px(1.) && offset > px(1.) - max;
        div()
            .relative()
            .flex_1()
            .min_w_0()
            .child(
                div()
                    .id(element_id)
                    .text_sm()
                    .w_full()
                    // 双轴滚动而非 overflow_x_scroll：gpui 对单轴滚动容器会把另一轴的
                    // 滚轮增量折进来，双轴下纵向滚轮原样冒泡给会话列表，互不影响
                    .overflow_scroll()
                    .whitespace_nowrap()
                    .track_scroll(&title_handle)
                    .on_hover(cx.listener(move |this, hovered, _, cx| {
                        if *hovered {
                            this.begin_title_marquee(&hover_key, cx);
                        } else {
                            this.end_title_marquee(&hover_key, cx);
                        }
                    }))
                    .child(div().w(title_width).child(title.to_string())),
            )
            .when(hides_leading, |this| {
                this.child(Self::title_fade(true, fade_base, fade_tint))
            })
            .when(hides_trailing, |this| {
                this.child(Self::title_fade(false, fade_base, fade_tint))
            })
    }
}
