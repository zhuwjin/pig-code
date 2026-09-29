use super::*;

impl ThreadView {
    /// 打开图片灯箱（缩略图点击）；失效附件没有 thumb 不会走到这
    pub(crate) fn open_lightbox(
        &mut self,
        message_ix: usize,
        image_ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(image) = self
            .messages
            .get(message_ix)
            .and_then(|message| message.images.get(image_ix))
        else {
            return;
        };
        let Some(thumb) = &image.thumb else {
            return;
        };
        self.lightbox = Some(Lightbox {
            image: thumb.clone(),
            label: format!("图片 {}", message_image_number(image_ix)),
            dims: image.dims,
            position: (message_ix, image_ix),
            zoom: 1.0,
            pan: (0.0, 0.0),
            drag_start: None,
            drag_capture: false,
            drag_moved: false,
        });
        // Esc 关闭依赖焦点在灯箱上
        self.lightbox_focus.focus(window, cx);
        cx.notify();
    }


    pub(crate) fn close_lightbox(&mut self, cx: &mut Context<Self>) {
        self.lightbox = None;
        cx.notify();
    }


    pub(crate) fn lightbox_positions(&self) -> Vec<(usize, usize)> {
        let Some(message_ix) = self.lightbox.as_ref().map(|lightbox| lightbox.position.0) else {
            return Vec::new();
        };
        let Some(message) = self.messages.get(message_ix) else {
            return Vec::new();
        };
        collect_lightbox_positions(
            message_ix,
            message.images.iter().map(|image| image.thumb.is_some()),
        )
    }


    pub(crate) fn current_lightbox_position(&self) -> Option<(usize, usize)> {
        let position = self.lightbox.as_ref()?.position;
        let positions = self.lightbox_positions();
        let index = positions
            .iter()
            .position(|candidate| *candidate == position)?;
        Some((index, positions.len()))
    }


    pub(crate) fn navigate_lightbox(&mut self, direction: isize, cx: &mut Context<Self>) {
        let Some(current) = self.lightbox.as_ref().map(|lightbox| lightbox.position) else {
            return;
        };
        let positions = self.lightbox_positions();
        let Some(index) = positions.iter().position(|candidate| *candidate == current) else {
            return;
        };
        let Some(next_index) = adjacent_image_index(index, positions.len(), direction) else {
            return;
        };
        let next_position = positions[next_index];
        let Some(image) = self
            .messages
            .get(next_position.0)
            .and_then(|message| message.images.get(next_position.1))
            .and_then(|image| image.thumb.as_ref().map(|thumb| (image, thumb.clone())))
        else {
            return;
        };
        let lightbox = self.lightbox.as_mut().expect("lightbox");
        lightbox.image = image.1;
        lightbox.label = format!("图片 {}", message_image_number(next_position.1));
        lightbox.dims = image.0.dims;
        lightbox.position = next_position;
        lightbox.zoom = 1.0;
        lightbox.pan = (0.0, 0.0);
        lightbox.drag_start = None;
        lightbox.drag_capture = false;
        lightbox.drag_moved = false;
        cx.notify();
    }


    pub(crate) fn reset_lightbox_view(&mut self, cx: &mut Context<Self>) {
        if let Some(lightbox) = self.lightbox.as_mut() {
            lightbox.zoom = 1.0;
            lightbox.pan = (0.0, 0.0);
            lightbox.drag_start = None;
            lightbox.drag_capture = false;
            lightbox.drag_moved = false;
        }
        cx.notify();
    }


    pub(crate) fn adjust_lightbox_zoom(
        &mut self,
        factor: f32,
        anchor: Option<(f32, f32)>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let (origin, viewport) = lightbox_viewport(&self.scroll_handle, window);
        let Some(lightbox) = self.lightbox.as_mut() else {
            return;
        };
        let old_zoom = lightbox.zoom;
        let new_zoom = (old_zoom * factor).clamp(LIGHTBOX_MIN_ZOOM, LIGHTBOX_MAX_ZOOM);
        if (new_zoom - old_zoom).abs() < f32::EPSILON {
            return;
        }
        if let Some(anchor) = anchor {
            lightbox.pan = lightbox_pan_after_zoom(
                lightbox.pan,
                lightbox.dims,
                viewport,
                origin,
                anchor,
                old_zoom,
                new_zoom,
            );
        }
        lightbox.zoom = new_zoom;
        lightbox.pan = clamp_lightbox_pan(
            lightbox.pan,
            lightbox_frame_size(lightbox.dims, viewport, lightbox.zoom),
            viewport,
        );
        cx.notify();
    }


    pub(crate) fn move_lightbox(&mut self, pan: (f32, f32), window: &Window, cx: &mut Context<Self>) {
        let (_, viewport) = lightbox_viewport(&self.scroll_handle, window);
        let Some(lightbox) = self.lightbox.as_mut() else {
            return;
        };
        lightbox.pan = clamp_lightbox_pan(
            pan,
            lightbox_frame_size(lightbox.dims, viewport, lightbox.zoom),
            viewport,
        );
        cx.notify();
    }


    /// 图片灯箱覆盖消息区：支持拖动、滚轮/触控板缩放、双击适配和工具栏控制。
    pub(crate) fn render_lightbox(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let lightbox = self.lightbox.as_ref().expect("lightbox");
        let (_, viewport) = lightbox_viewport(&self.scroll_handle, window);
        let image_size = lightbox_display_size(lightbox.dims, viewport, lightbox.zoom);
        let frame_size = lightbox_frame_size(lightbox.dims, viewport, lightbox.zoom);
        let frame_left = (viewport.0 - frame_size.0) * 0.5 + lightbox.pan.0;
        let frame_top = (viewport.1 - frame_size.1) * 0.5 + lightbox.pan.1;
        let on_mask = gpui_kit::white();
        let zoom_label = format!(
            "{}%",
            (lightbox_fit_scale(lightbox.dims, viewport) * lightbox.zoom * 100.0).round() as u32
        );
        let current_position = self.current_lightbox_position();
        let has_navigation = current_position.is_some_and(|(_, count)| count > 1);
        let (position_index, position_count) = current_position.unwrap_or((0, 0));
        let previous_disabled = position_index == 0;
        let next_disabled = position_count == 0 || position_index + 1 >= position_count;
        let image = lightbox.image.clone();

        let previous = div()
            .id("image-lightbox-prev")
            .cursor_pointer()
            .when(previous_disabled, |this| this.opacity(0.4))
            .when(!previous_disabled, |this| {
                this.hover(|this| this.bg(on_mask.opacity(0.18)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.navigate_lightbox(-1, cx);
                        cx.stop_propagation();
                    }))
            })
            .px_1()
            .py_1()
            .rounded_sm()
            .child("‹");
        let next = div()
            .id("image-lightbox-next")
            .cursor_pointer()
            .when(next_disabled, |this| this.opacity(0.4))
            .when(!next_disabled, |this| {
                this.hover(|this| this.bg(on_mask.opacity(0.18)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.navigate_lightbox(1, cx);
                        cx.stop_propagation();
                    }))
            })
            .px_1()
            .py_1()
            .rounded_sm()
            .child("›");

        let image_frame = v_flex()
            .id("image-lightbox-frame")
            .absolute()
            .left(px(frame_left))
            .top(px(frame_top))
            .w(px(frame_size.0))
            .h(px(frame_size.1))
            .items_center()
            .justify_center()
            .cursor_grab()
            .border_1()
            .border_color(on_mask.opacity(0.72))
            .rounded_md()
            .bg(gpui_kit::black().opacity(0.72))
            .shadow_lg()
            .on_click(cx.listener(|this, event: &ClickEvent, _, cx| {
                if let Some(lightbox) = this.lightbox.as_mut() {
                    if event.click_count() == 2 && !lightbox.drag_moved {
                        lightbox.zoom = 1.0;
                        lightbox.pan = (0.0, 0.0);
                    }
                    lightbox.drag_moved = false;
                }
                cx.stop_propagation();
                cx.notify();
            }))
            .child(gpui_kit::img(image).w(px(image_size.0)).h(px(image_size.1)));

        let zoom_in = div()
            .id("image-lightbox-zoom-in")
            .cursor_pointer()
            .px_2()
            .py_1()
            .rounded_sm()
            .hover(|this| this.bg(on_mask.opacity(0.18)))
            .on_click(cx.listener(|this, _, window, cx| {
                this.adjust_lightbox_zoom(1.25, None, window, cx);
                cx.stop_propagation();
            }))
            .child("+");
        let zoom_out = div()
            .id("image-lightbox-zoom-out")
            .cursor_pointer()
            .px_2()
            .py_1()
            .rounded_sm()
            .hover(|this| this.bg(on_mask.opacity(0.18)))
            .on_click(cx.listener(|this, _, window, cx| {
                this.adjust_lightbox_zoom(0.8, None, window, cx);
                cx.stop_propagation();
            }))
            .child("−");
        let fit = div()
            .id("image-lightbox-fit")
            .cursor_pointer()
            .px_2()
            .py_1()
            .rounded_sm()
            .hover(|this| this.bg(on_mask.opacity(0.18)))
            .on_click(cx.listener(|this, _, _, cx| {
                this.reset_lightbox_view(cx);
                cx.stop_propagation();
            }))
            .child("适配");
        let close = div()
            .id("image-lightbox-close")
            .cursor_pointer()
            .px_2()
            .py_1()
            .rounded_sm()
            .hover(|this| this.bg(on_mask.opacity(0.18)))
            .on_click(cx.listener(|this, _, _, cx| {
                this.close_lightbox(cx);
                cx.stop_propagation();
            }))
            .child("×");
        let toolbar = h_flex()
            .absolute()
            .top_3()
            .right_3()
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(on_mask.opacity(0.24))
            .bg(gpui_kit::black().opacity(0.72))
            .text_sm()
            .text_color(on_mask.opacity(0.92))
            .child(div().px_2().child(lightbox.label.clone()))
            .when(has_navigation, |this| {
                this.child(previous)
                    .child(
                        div()
                            .px_1()
                            .text_color(on_mask.opacity(0.68))
                            .child(format!("{}/{}", position_index + 1, position_count)),
                    )
                    .child(next)
            })
            .child(
                div()
                    .px_1()
                    .text_color(on_mask.opacity(0.68))
                    .child(zoom_label),
            )
            .child(zoom_out)
            .child(zoom_in)
            .child(fit)
            .child(close);

        let on_mask_click = cx.listener(|this, _, _, cx| {
            let suppress_close = if let Some(lightbox) = this.lightbox.as_mut() {
                let suppress = lightbox.drag_moved;
                lightbox.drag_moved = false;
                suppress
            } else {
                false
            };
            if !suppress_close {
                this.close_lightbox(cx);
            }
        });
        let wheel_zoom = cx.listener(|this, event: &ScrollWheelEvent, window, cx| {
            let delta = event.delta.pixel_delta(window.line_height());
            if delta.y != px(0.) {
                let factor = (f32::from(delta.y) * 0.002).exp().clamp(0.8, 1.25);
                this.adjust_lightbox_zoom(
                    factor,
                    Some((f32::from(event.position.x), f32::from(event.position.y))),
                    window,
                    cx,
                );
            }
            cx.stop_propagation();
        });
        let pinch_zoom = cx.listener(|this, event: &PinchEvent, window, cx| {
            this.adjust_lightbox_zoom(
                (1.0 + event.delta).clamp(0.5, 2.0),
                Some((f32::from(event.position.x), f32::from(event.position.y))),
                window,
                cx,
            );
            cx.stop_propagation();
        });
        let content_width = (viewport.0 - 48.0).max(1.0);
        let content_height = (viewport.1 - 48.0).max(1.0);

        div()
            .id("image-lightbox-overlay")
            .absolute()
            .inset_0()
            .bg(gpui_kit::black().opacity(0.5))
            .size_full()
            .track_focus(&self.lightbox_focus)
            .on_click(on_mask_click)
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                let Some(lightbox) = this.lightbox.as_ref() else {
                    return;
                };
                let Some((start, base_pan)) = lightbox.drag_start else {
                    return;
                };
                if !lightbox.drag_capture || event.pressed_button != Some(MouseButton::Left) {
                    return;
                }
                let position = (f32::from(event.position.x), f32::from(event.position.y));
                let dx = position.0 - start.0;
                let dy = position.1 - start.1;
                if dx.abs() + dy.abs() > 3.0
                    && let Some(lightbox) = this.lightbox.as_mut()
                {
                    lightbox.drag_moved = true;
                }
                this.move_lightbox((base_pan.0 + dx, base_pan.1 + dy), window, cx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if let Some(lightbox) = this.lightbox.as_mut() {
                        lightbox.drag_start = None;
                        lightbox.drag_capture = false;
                    }
                    cx.notify();
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if let Some(lightbox) = this.lightbox.as_mut() {
                        lightbox.drag_start = None;
                        lightbox.drag_capture = false;
                    }
                    cx.notify();
                }),
            )
            .on_scroll_wheel(wheel_zoom)
            .on_pinch(pinch_zoom)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    this.close_lightbox(cx);
                } else if !event.is_held && !event.keystroke.modifiers.modified() {
                    match event.keystroke.key.as_str() {
                        "left" => {
                            this.navigate_lightbox(-1, cx);
                            cx.stop_propagation();
                        }
                        "right" => {
                            this.navigate_lightbox(1, cx);
                            cx.stop_propagation();
                        }
                        _ => {}
                    }
                }
            }))
            .child(
                div()
                    .id("image-lightbox")
                    .size_full()
                    .max_w(px(content_width))
                    .max_h(px(content_height))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, window, cx| {
                            let (origin, viewport) = lightbox_viewport(&this.scroll_handle, window);
                            let pointer =
                                (f32::from(event.position.x), f32::from(event.position.y));
                            if let Some(lightbox) = this.lightbox.as_mut() {
                                let frame =
                                    lightbox_frame_size(lightbox.dims, viewport, lightbox.zoom);
                                let center = (
                                    origin.0 + viewport.0 * 0.5 + lightbox.pan.0,
                                    origin.1 + viewport.1 * 0.5 + lightbox.pan.1,
                                );
                                if (pointer.0 - center.0).abs() <= frame.0 * 0.5
                                    && (pointer.1 - center.1).abs() <= frame.1 * 0.5
                                {
                                    lightbox.drag_start = Some((pointer, lightbox.pan));
                                    lightbox.drag_capture = true;
                                    lightbox.drag_moved = false;
                                }
                            }
                            cx.stop_propagation();
                        }),
                    )
                    .child(image_frame)
                    .child(toolbar),
            )
            .into_any_element()
    }


}

pub(crate) fn message_image_number(image_ix: usize) -> usize {
    image_ix + 1
}

pub(crate) fn collect_lightbox_positions(
    message_ix: usize,
    images: impl IntoIterator<Item = bool>,
) -> Vec<(usize, usize)> {
    images
        .into_iter()
        .enumerate()
        .filter_map(|(image_ix, available)| available.then_some((message_ix, image_ix)))
        .collect()
}

pub(crate) fn adjacent_image_index(current: usize, count: usize, direction: isize) -> Option<usize> {
    let next = current.checked_add_signed(direction)?;
    (next < count).then_some(next)
}

const LIGHTBOX_MIN_ZOOM: f32 = 0.1;
const LIGHTBOX_MAX_ZOOM: f32 = 8.0;
const LIGHTBOX_FRAME_INSET: f32 = 5.0;

pub(crate) fn lightbox_viewport(handle: &ScrollHandle, window: &Window) -> ((f32, f32), (f32, f32)) {
    let bounds = handle.bounds();
    let size = (f32::from(bounds.size.width), f32::from(bounds.size.height));
    if size.0 > 0.0 && size.1 > 0.0 {
        ((f32::from(bounds.left()), f32::from(bounds.top())), size)
    } else {
        let viewport = window.viewport_size();
        (
            (0.0, 0.0),
            (f32::from(viewport.width), f32::from(viewport.height)),
        )
    }
}

pub(crate) fn lightbox_fit_scale(dims: (u32, u32), viewport: (f32, f32)) -> f32 {
    if dims.0 == 0 || dims.1 == 0 || viewport.0 <= 0.0 || viewport.1 <= 0.0 {
        return 1.0;
    }
    let available_width = (viewport.0 - 48.0).max(1.0);
    let available_height = (viewport.1 - 88.0).max(1.0);
    (available_width * 0.9 / dims.0 as f32).min(available_height * 0.9 / dims.1 as f32)
}

pub(crate) fn lightbox_display_size(dims: (u32, u32), viewport: (f32, f32), zoom: f32) -> (f32, f32) {
    let scale =
        lightbox_fit_scale(dims, viewport) * zoom.clamp(LIGHTBOX_MIN_ZOOM, LIGHTBOX_MAX_ZOOM);
    (
        (dims.0 as f32 * scale).max(1.0),
        (dims.1 as f32 * scale).max(1.0),
    )
}

pub(crate) fn lightbox_frame_size(dims: (u32, u32), viewport: (f32, f32), zoom: f32) -> (f32, f32) {
    let image_size = lightbox_display_size(dims, viewport, zoom);
    (
        image_size.0 + LIGHTBOX_FRAME_INSET * 2.0,
        image_size.1 + LIGHTBOX_FRAME_INSET * 2.0,
    )
}

pub(crate) fn lightbox_pan_after_zoom(
    pan: (f32, f32),
    dims: (u32, u32),
    viewport: (f32, f32),
    origin: (f32, f32),
    pointer: (f32, f32),
    old_zoom: f32,
    new_zoom: f32,
) -> (f32, f32) {
    let base_scale = lightbox_fit_scale(dims, viewport);
    let image_center = (viewport.0 * 0.5 + pan.0, viewport.1 * 0.5 + pan.1);
    let image_point = (
        (pointer.0 - origin.0 - image_center.0) / (base_scale * old_zoom),
        (pointer.1 - origin.1 - image_center.1) / (base_scale * old_zoom),
    );
    (
        pointer.0 - origin.0 - viewport.0 * 0.5 - image_point.0 * base_scale * new_zoom,
        pointer.1 - origin.1 - viewport.1 * 0.5 - image_point.1 * base_scale * new_zoom,
    )
}

pub(crate) fn clamp_lightbox_pan(pan: (f32, f32), frame: (f32, f32), viewport: (f32, f32)) -> (f32, f32) {
    let max_x = ((frame.0 - viewport.0) * 0.5).max(0.0);
    let max_y = ((frame.1 - viewport.1) * 0.5).max(0.0);
    (pan.0.clamp(-max_x, max_x), pan.1.clamp(-max_y, max_y))
}

/// 滚动穿透：有滚动条（max_offset > 0，内容超出视口）时吞掉滚轮事件，不穿透到外层
/// 消息列表（这版 gpui 的内置滚动监听不阻断冒泡，不吞的话外层会联动，到顶/到底也不放行）；
/// 没有可滚空间时放行，滚轮直接滚动外层。
pub(crate) fn consume_scroll(
    handle: &ScrollHandle,
) -> impl Fn(&ScrollWheelEvent, &mut Window, &mut App) + 'static {
    let handle = handle.clone();
    move |_, _, cx| {
        if handle.max_offset().y > px(0.) {
            cx.stop_propagation();
        }
    }
}
