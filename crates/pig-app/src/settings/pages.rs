use super::*;

impl SettingsView {
    pub(crate) fn render_nav(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut nav = v_flex()
            .w(px(220.))
            .h_full()
            .gap_1()
            .p_3()
            .border_r_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .id("back-to-workspace")
                    .gap_2()
                    .px_2()
                    .py_1()
                    .mb_2()
                    .rounded(cx.theme().radius)
                    .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
                    .on_click(cx.listener(|_, _, _, cx| {
                        cx.emit(SettingsEvent::Close);
                    }))
                    .child(Icon::new(IconName::ArrowLeft).size_4())
                    .child(div().text_sm().child("返回工作区")),
            );
        for (group, items) in NAV {
            nav = nav
                .child(
                    div()
                        .px_2()
                        .py_1()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(*group),
                )
                .children(items.iter().map(|(page, icon, label)| {
                    let selected = self.page == *page;
                    h_flex()
                        .id(gpui_kit::SharedString::from(label.to_string()))
                        .gap_2()
                        .px_2()
                        .py_1()
                        .rounded(cx.theme().radius)
                        .cursor_pointer()
                        .when(selected, |this| this.bg(cx.theme().accent))
                        .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.page = *page;
                            // 进入 MCP 页即刷新：重读配置 + 重新查询连接状态
                            if *page == SettingsPage::Mcp {
                                this.refresh_mcp(cx);
                            }
                            cx.notify();
                        }))
                        .child(
                            Icon::new(icon.clone())
                                .size_4()
                                .text_color(cx.theme().muted_foreground),
                        )
                        .child(div().text_sm().child(*label))
                }));
        }
        nav.into_any_element()
    }

    pub(crate) fn render_appearance(&self, cx: &mut Context<Self>) -> AnyElement {
        let follow_system = cx
            .try_global::<crate::ThemeFollowSystem>()
            .is_some_and(|flag| flag.0);
        let current_dark = cx.theme().mode.is_dark();
        h_flex()
            .gap_4()
            .children(
                [
                    (ThemeMode::Light, "亮色", IconName::Sun),
                    (ThemeMode::Dark, "暗色", IconName::Moon),
                ]
                .map(|(mode, label, icon)| {
                    let selected = !follow_system && mode.is_dark() == current_dark;
                    v_flex()
                        .id(gpui_kit::SharedString::from(label.to_string()))
                        .gap_2()
                        .w(px(180.))
                        .p_4()
                        .items_center()
                        .rounded(cx.theme().radius_lg)
                        .border_2()
                        .border_color(if selected {
                            cx.theme().primary
                        } else {
                            cx.theme().border
                        })
                        .cursor_pointer()
                        .hover(|this| this.bg(cx.theme().accent.opacity(0.4)))
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.set_global(crate::ThemeFollowSystem(false));
                            gpui_kit::component::Theme::change(mode, None, cx);
                        }))
                        .child(Icon::new(icon).size_8())
                        .child(div().text_sm().child(label))
                }),
            )
            .child(
                v_flex()
                    .id("跟随系统")
                    .gap_2()
                    .w(px(180.))
                    .p_4()
                    .items_center()
                    .rounded(cx.theme().radius_lg)
                    .border_2()
                    .border_color(if follow_system {
                        cx.theme().primary
                    } else {
                        cx.theme().border
                    })
                    .cursor_pointer()
                    .hover(|this| this.bg(cx.theme().accent.opacity(0.4)))
                    .on_click(cx.listener(|_, _, window, cx| {
                        cx.set_global(crate::ThemeFollowSystem(true));
                        gpui_kit::component::Theme::sync_system_appearance(Some(window), cx);
                    }))
                    .child(
                        h_flex()
                            .h_8()
                            .items_center()
                            .gap_1()
                            .child(Icon::new(IconName::Sun).size_6())
                            .child(Icon::new(IconName::Moon).size_6()),
                    )
                    .child(div().text_sm().child("跟随系统")),
            )
            .into_any_element()
    }

    pub(crate) fn render_placeholder(&self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .text_color(cx.theme().muted_foreground)
            .child(Icon::new(IconName::Inbox).size_8())
            .child(div().text_sm().child("即将推出"))
            .into_any_element()
    }
}
