use super::*;

impl AppView {
    pub(crate) fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_dark = cx.theme().mode.is_dark();
        let title = self
            .current
            .as_ref()
            .and_then(|id| self.metas.iter().find(|m| &m.id == id))
            .map(|m| m.title.clone())
            .unwrap_or_else(|| "pig-code".to_string());

        // Windows 上标题栏命中 HTCAPTION：左键按下仍会派发 MouseDownEvent，但抬起被
        // OS 的窗口移动模态循环吞掉，窗口级文本选择一旦开始手势就收不到结束，
        // 之后移动鼠标会变成拖选。按下标题栏时抑制选择，手势便永不开始。
        div()
            .id("title-bar-selection-guard")
            .w_full()
            .flex_shrink_0()
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                GlobalState::suppress_text_selection(cx);
            })
            .child(
                TitleBar::new()
                    .child(if self.settings_open {
                        // 设置模式：左区换「返回工作区」（与其他标题栏按钮同款样式，
                        // 替代在设置侧栏里叠加返回按钮的方案）
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("back-to-workspace")
                                    .ghost()
                                    .small()
                                    .occlude()
                                    .icon(IconName::ArrowLeft)
                                    .label("返回工作区")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.settings_open = false;
                                        cx.notify();
                                    })),
                            )
                            .child(div().text_sm().font_semibold().child("设置"))
                    } else {
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("toggle-sidebar")
                                    .ghost()
                                    .small()
                                    .occlude()
                                    .icon(IconName::PanelLeft)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.sidebar_collapsed = !this.sidebar_collapsed;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .font_semibold()
                                    .child(format!("pig-code · {title}")),
                            )
                            // 会话操作菜单（三个点）：有活动会话才显示
                            .when(self.current.is_some(), |this| {
                                this.child(
                                    div()
                                        .on_prepaint({
                                            let cell = self.session_menu_btn_bounds.clone();
                                            move |bounds, _, _| cell.set(bounds)
                                        })
                                        .child(
                                            Button::new("session-menu")
                                                .ghost()
                                                .small()
                                                .occlude()
                                                .icon(IconName::Ellipsis)
                                                .on_click(cx.listener(
                                                    |this, event: &ClickEvent, _, cx| {
                                                        // 与分支 chip 同款：菜单打开时点按钮，
                                                        // 按下先 outside-close（记位置），同一次
                                                        // 按压的 click 按位置吞掉防收起又弹开
                                                        let down_pos = match event {
                                                            ClickEvent::Mouse(e) => {
                                                                Some(e.down.position)
                                                            }
                                                            _ => None,
                                                        };
                                                        if this
                                                            .session_menu_outside_close
                                                            .take()
                                                            .is_some_and(|pos| {
                                                                Some(pos) == down_pos
                                                            })
                                                        {
                                                            return;
                                                        }
                                                        this.session_menu_open =
                                                            !this.session_menu_open;
                                                        cx.notify();
                                                    },
                                                )),
                                        ),
                                )
                            })
                    })
                    .child(
                        h_flex()
                            .gap_1()
                            .px_2()
                            // 「在访达/文件管理器中打开」split 按钮：主钮直接打开当前
                            // 工作区，chevron 出菜单（后续在终端/编辑器打开等挂同一菜单）
                            .when(self.current.is_some(), |this| {
                                let fm_label =
                                    format!("在{}中打开", pig_core::files::file_manager_name());
                                // 彩色图标不能走 Icon（svg 按文字色渲成单色）：
                                // macOS 用 NSWorkspace 取的真实访达图标（img 保色），
                                // 取到前/其余平台回退 Lucide 文件夹
                                let fm_icon_el: AnyElement = match &self.fm_icon {
                                    Some(icon) => img(icon.clone()).size_4().into_any_element(),
                                    None => Icon::new(AssetsIconName::FolderOpen)
                                        .size_4()
                                        .into_any_element(),
                                };
                                let view = cx.entity().downgrade();
                                let menu_icon = self.fm_icon.clone();
                                let menu_label = fm_label.clone();
                                this.child(
                                    DropdownButton::new("fm-split")
                                        .outline()
                                        .small()
                                        .button(
                                            Button::new("fm-open")
                                                .occlude()
                                                .tooltip(fm_label.clone())
                                                .child(fm_icon_el)
                                                .on_click(cx.listener(|this, _, _, _| {
                                                    this.open_current_in_file_manager();
                                                })),
                                        )
                                        .dropdown_menu(move |menu, _, _| {
                                            let view = view.clone();
                                            let label = menu_label.clone();
                                            // 有真图就用 ElementItem 自绘「图标+文字」行
                                            //（icon 槽只收单色 Icon，彩图得走 img）；
                                            // 无真图（非 mac/未取到）退化为纯文字项
                                            let item = match menu_icon.clone() {
                                                Some(icon) => {
                                                    PopupMenuItem::element(move |_, _| {
                                                        h_flex()
                                                            .gap_2()
                                                            .items_center()
                                                            .child(img(icon.clone()).size_4())
                                                            .child(label.clone())
                                                    })
                                                }
                                                None => PopupMenuItem::new(menu_label.clone()),
                                            };
                                            menu.item(item.on_click(move |_, _, cx| {
                                                let _ = view.update(cx, |this, _| {
                                                    this.open_current_in_file_manager();
                                                });
                                            }))
                                        }),
                                )
                            })
                            .when(self.git_branch.is_some(), |this| {
                                this.child(
                                    div()
                                        .on_prepaint({
                                            let cell = self.title_branch_btn_bounds.clone();
                                            move |bounds, _, _| cell.set(bounds)
                                        })
                                        .child(
                                            Button::new("title-branch")
                                                .ghost()
                                                .small()
                                                .occlude()
                                                .label(format!(
                                                    "⎇ {}",
                                                    self.git_branch.as_deref().unwrap_or_default()
                                                ))
                                                .on_click(cx.listener(
                                                    |this, event: &ClickEvent, _, cx| {
                                                        // 菜单打开时点 chip：按下先触发
                                                        // outside-close（记录位置），同一次按压
                                                        // 的 click 按位置吞掉，避免收起又弹开
                                                        let down_pos = match event {
                                                            ClickEvent::Mouse(e) => {
                                                                Some(e.down.position)
                                                            }
                                                            _ => None,
                                                        };
                                                        if this
                                                            .title_branch_outside_close
                                                            .take()
                                                            .is_some_and(|pos| {
                                                                Some(pos) == down_pos
                                                            })
                                                        {
                                                            return;
                                                        }
                                                        this.title_branch_menu_open =
                                                            !this.title_branch_menu_open;
                                                        cx.notify();
                                                    },
                                                )),
                                        ),
                                )
                            })
                            .child(
                                Button::new("toggle-theme")
                                    .ghost()
                                    .small()
                                    .occlude()
                                    .icon(if is_dark {
                                        IconName::Sun
                                    } else {
                                        IconName::Moon
                                    })
                                    .on_click(move |_, _, cx| {
                                        Theme::change(
                                            if is_dark {
                                                ThemeMode::Light
                                            } else {
                                                ThemeMode::Dark
                                            },
                                            None,
                                            cx,
                                        );
                                        cx.set_global(ThemeFollowSystem(false));
                                    }),
                            )
                            .child(
                                Button::new("open-settings")
                                    .ghost()
                                    .small()
                                    .occlude()
                                    .icon(IconName::Settings)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.open_settings(cx);
                                    })),
                            )
                            .child(
                                Button::new("toggle-terminal")
                                    .ghost()
                                    .small()
                                    .occlude()
                                    .icon(IconName::SquareTerminal)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.toggle_terminal_panel(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("right-panel-menu")
                                    .ghost()
                                    .small()
                                    .occlude()
                                    .icon(if self.right_open {
                                        IconName::PanelRightClose
                                    } else {
                                        IconName::PanelRight
                                    })
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.toggle_right_panel(cx);
                                    })),
                            ),
                    ),
            )
            .when(self.title_branch_menu_open, |this| {
                this.child(self.render_branch_menu(cx))
            })
            .when(self.session_menu_open, |this| {
                this.child(self.render_session_menu(cx))
            })
    }

    /// 标题栏会话菜单（三个点弹层）：锚定按钮下方、左对齐（菜单向右展开），点外部收起。
    /// 置顶/归档/重命名与侧栏右键菜单同链路；末项「查看调用轨迹」
    pub(crate) fn render_session_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let meta = self
            .current
            .as_ref()
            .and_then(|id| self.metas.iter().find(|m| &m.id == id));
        let pinned = meta.is_some_and(|m| m.pinned);
        let archived = meta.is_some_and(|m| m.archived);
        let pin_id = self.current.clone().unwrap_or_default();
        let archive_id = pin_id.clone();
        let rename_id = pin_id.clone();
        let rename_title = meta.map(|m| m.title.clone()).unwrap_or_default();
        deferred(
            Positioner::side(self.session_menu_btn_bounds.get())
                .placement(Placement::Bottom)
                .align(Align::Start)
                .offset(px(6.))
                .margin(px(8.))
                .occlude()
                .child(
                    v_flex()
                        .id("title-session-menu")
                        .w(px(220.))
                        .p_1()
                        .bg(cx.theme().popover)
                        .border_1()
                        .border_color(cx.theme().border)
                        .rounded_lg()
                        .shadow_lg()
                        .on_mouse_down_out(cx.listener(|this, event: &MouseDownEvent, _, cx| {
                            this.session_menu_open = false;
                            this.session_menu_outside_close = Some(event.position);
                            cx.notify();
                        }))
                        .child(
                            h_flex()
                                .id("menu-toggle-pin")
                                .gap_2()
                                .px_2()
                                .py_1()
                                .rounded(cx.theme().radius)
                                .text_sm()
                                .cursor_pointer()
                                .hover(|h| h.bg(cx.theme().accent.opacity(0.6)))
                                .child(
                                    Icon::new(if pinned {
                                        IconName::StarFill
                                    } else {
                                        IconName::Star
                                    })
                                    .size_4(),
                                )
                                .child(div().child(if pinned { "取消置顶" } else { "置顶" }))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.session_menu_open = false;
                                    this.agent.set_pinned(&pin_id, !pinned);
                                    cx.notify();
                                })),
                        )
                        .child(
                            h_flex()
                                .id("menu-toggle-archive")
                                .gap_2()
                                .px_2()
                                .py_1()
                                .rounded(cx.theme().radius)
                                .text_sm()
                                .cursor_pointer()
                                .hover(|h| h.bg(cx.theme().accent.opacity(0.6)))
                                .child(
                                    Icon::new(if archived {
                                        IconName::Undo2
                                    } else {
                                        IconName::Inbox
                                    })
                                    .size_4(),
                                )
                                .child(div().child(if archived {
                                    "取消归档"
                                } else {
                                    "归档"
                                }))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.session_menu_open = false;
                                    this.agent.set_archived(&archive_id, !archived);
                                    cx.notify();
                                })),
                        )
                        // 归档会话不在侧栏渲染（设置页管理），行内重命名无行可承载——隐藏该入口
                        .when(!archived, |this| {
                            this.child(
                                h_flex()
                                    .id("menu-rename-session")
                                    .gap_2()
                                    .px_2()
                                    .py_1()
                                    .rounded(cx.theme().radius)
                                    .text_sm()
                                    .cursor_pointer()
                                    .hover(|h| h.bg(cx.theme().accent.opacity(0.6)))
                                    .child(Icon::new(AssetsIconName::SquarePen).size_4())
                                    .child(div().child("重命名"))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.session_menu_open = false;
                                        // 行内重命名输入框画在侧栏会话行上：收起态先展开侧栏
                                        this.sidebar_collapsed = false;
                                        this.sidebar.update(cx, |sidebar, cx| {
                                            sidebar.start_session_rename(
                                                rename_id.clone(),
                                                rename_title.clone(),
                                                window,
                                                cx,
                                            );
                                        });
                                        cx.notify();
                                    })),
                            )
                        })
                        .child(div().h(px(1.)).my_1().bg(cx.theme().border))
                        .child(
                            h_flex()
                                .id("menu-view-trajectory")
                                .gap_2()
                                .px_2()
                                .py_1()
                                .rounded(cx.theme().radius)
                                .text_sm()
                                .cursor_pointer()
                                .hover(|h| h.bg(cx.theme().accent.opacity(0.6)))
                                .child(Icon::new(IconName::FileText).size_4())
                                .child(div().child("查看调用轨迹"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.session_menu_open = false;
                                    this.open_trajectory(cx);
                                })),
                        ),
                ),
        )
        .with_priority(1)
        .into_any_element()
    }

    /// 当前会话的工作区在系统文件管理器中打开（macOS 访达 / Windows 资源管理器 /
    /// Linux xdg-open；detached spawn，启动失败只记日志）
    pub(crate) fn open_current_in_file_manager(&self) {
        let Some(cwd) = self.current_cwd() else {
            return;
        };
        if let Err(err) = pig_core::files::open_in_file_manager(&cwd) {
            eprintln!(
                "[fm] 打开{}失败 {}: {err}",
                pig_core::files::file_manager_name(),
                cwd.display()
            );
        }
    }

    /// 标题栏分支切换菜单：deferred 到窗口层，锚定分支 chip 正下方
    /// （与标签页 "+" 菜单同一模式）。当前分支高亮，点击其他分支 checkout。
    pub(crate) fn render_branch_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let current = self.git_branch.clone().unwrap_or_default();
        deferred(
            Positioner::side(self.title_branch_btn_bounds.get())
                .placement(Placement::Bottom)
                .align(Align::End)
                .offset(px(6.))
                .margin(px(8.))
                .occlude()
                .child(
                    v_flex()
                        .id("title-branch-menu")
                        .w(px(240.))
                        .max_h(px(360.))
                        .overflow_y_scroll()
                        .p_1()
                        .bg(cx.theme().popover)
                        .border_1()
                        .border_color(cx.theme().border)
                        .rounded_lg()
                        .shadow_lg()
                        .on_mouse_down_out(cx.listener(|this, event: &MouseDownEvent, _, cx| {
                            this.title_branch_menu_open = false;
                            this.title_branch_outside_close = Some(event.position);
                            cx.notify();
                        }))
                        .child(
                            div()
                                .px_2()
                                .py_1()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("切换分支"),
                        )
                        .children(self.title_branches.iter().map(|branch| {
                            let branch = branch.clone();
                            let is_current = branch == current;
                            div()
                                .id(gpui_kit::SharedString::from(branch.clone()))
                                .px_2()
                                .py_1()
                                .rounded(cx.theme().radius)
                                .text_sm()
                                .cursor_pointer()
                                .when(is_current, |this| this.bg(cx.theme().accent))
                                .when(!is_current, |this| {
                                    this.hover(|h| h.bg(cx.theme().accent.opacity(0.6)))
                                })
                                .child(
                                    h_flex()
                                        .w_full()
                                        .justify_between()
                                        .gap_2()
                                        .child(
                                            div()
                                                .text_color(if is_current {
                                                    cx.theme().primary
                                                } else {
                                                    cx.theme().foreground
                                                })
                                                .child(branch.clone()),
                                        )
                                        .when(is_current, |this| {
                                            this.child(
                                                Icon::new(IconName::Check)
                                                    .size_3()
                                                    .text_color(cx.theme().primary),
                                            )
                                        }),
                                )
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.title_branch_menu_open = false;
                                    if let Some(cwd) = this.current_cwd() {
                                        this.agent.checkout_branch(cwd, branch.clone());
                                    }
                                    cx.notify();
                                }))
                        })),
                ),
        )
        .with_priority(1)
        .into_any_element()
    }
}
