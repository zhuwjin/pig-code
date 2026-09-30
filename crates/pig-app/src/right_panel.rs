use super::*;

/// 右侧面板菜单项：(名称, 图标, 快捷键 action, 占位禁用, 点击打开的 tab)
pub(crate) type RightMenuItem = (
    &'static str,
    AssetsIconName,
    Option<&'static dyn Action>,
    bool,
    Option<RightTab>,
);

impl AppView {
    /// 右侧面板 tab 开关（快捷键用）：已激活时再次触发 = 收起面板；否则打开并激活该 tab。
    pub(crate) fn toggle_right_tab(&mut self, tab: RightTab, cx: &mut Context<Self>) {
        if self.right_open && self.right_active.as_ref() == Some(&tab) {
            self.right_open = false;
        } else {
            if !self.right_tabs.contains(&tab) {
                self.right_tabs.push(tab.clone());
            }
            self.right_active = Some(tab);
            self.right_open = true;
        }
        cx.notify();
    }

    /// 打开并激活右侧 tab（菜单点击用，纯打开不带收起语义）
    pub(crate) fn open_right_tab(&mut self, tab: RightTab, cx: &mut Context<Self>) {
        if !self.right_tabs.contains(&tab) {
            self.right_tabs.push(tab.clone());
        }
        self.right_active = Some(tab);
        self.right_open = true;
        cx.notify();
    }

    /// 关闭右侧 tab：关掉激活 tab 时切到剩余最后一个；关掉的是最后一个 tab
    /// 时面板没有内容可显示，自动收起（经 step_dock_anim 走收起动画）。
    pub(crate) fn close_right_tab(&mut self, tab: RightTab, cx: &mut Context<Self>) {
        self.right_tabs.retain(|t| *t != tab);
        // 「子代理」tab 的内容面板随 tab 关闭释放
        if let RightTab::Subagent { agent_id } = &tab {
            self.subagent_tabs.remove(agent_id);
        }
        if self.right_active.as_ref() == Some(&tab) {
            self.right_active = self.right_tabs.last().cloned();
        }
        if self.right_tabs.is_empty() {
            self.right_open = false;
        }
        cx.notify();
    }

    /// 打开/聚焦「子代理」tab（通知卡点击）：未开则建面板实体并发加载请求；
    /// 已开（同 agent_id）只聚焦，不重复加载。
    pub(crate) fn open_subagent_tab(
        &mut self,
        session_id: String,
        agent_id: String,
        title: String,
        cx: &mut Context<Self>,
    ) {
        if !self.subagent_tabs.contains_key(&agent_id) {
            let panel = cx.new(|_| SubagentPanel::new(session_id.clone(), title));
            self.subagent_tabs.insert(agent_id.clone(), panel);
            self.agent.load_subagent(session_id, agent_id.clone());
        }
        let tab = RightTab::Subagent { agent_id };
        if !self.right_tabs.contains(&tab) {
            self.right_tabs.push(tab.clone());
        }
        self.right_active = Some(tab);
        self.right_open = true;
        cx.notify();
    }

    /// 开关标签页栏 "+" 的加面板菜单。
    pub(crate) fn toggle_right_menu(&mut self, click: &ClickEvent, cx: &mut Context<Self>) {
        // 菜单打开时点按钮：按下先触发菜单的 outside-close（记录按下位置），
        // 紧随的 click 按同一按下位置吞掉，避免收起又马上弹开（composer 弹层同款处理）
        let down_pos = match click {
            ClickEvent::Mouse(event) => Some(event.down.position),
            _ => None,
        };
        if self
            .right_menu_outside_close
            .take()
            .is_some_and(|pos| Some(pos) == down_pos)
        {
            return;
        }
        self.right_menu_open = !self.right_menu_open;
        cx.notify();
    }

    /// 浏览器/终端/侧边聊天为占位禁用项，快捷键先展示，功能后续加。
    pub(crate) fn right_menu_items() -> [RightMenuItem; 4] {
        [
            (
                "改动",
                AssetsIconName::GitBranch,
                Some(&ToggleChanges),
                false,
                Some(RightTab::Changes),
            ),
            (
                "浏览器",
                AssetsIconName::Globe,
                Some(&ToggleBrowser),
                true,
                None,
            ),
            ("终端", AssetsIconName::SquareTerminal, None, true, None),
            (
                "侧边聊天",
                AssetsIconName::MessageCircle,
                Some(&ToggleSideChat),
                true,
                None,
            ),
        ]
    }

    /// 快捷键芯片组（ZCode 样式：每个键一个小芯片；未绑键时不显示）。
    /// page = 面板首页：带边框的大号键帽，macOS 修饰键符号逐键拆分；
    /// 否则（下拉菜单）：muted 底小芯片，macOS 符号串整体一个芯片。
    pub(crate) fn render_shortcut_chips(
        &self,
        action: &dyn Action,
        page: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let binding = window
            .highest_precedence_binding_for_action_in_context(action, KeyContext::default())?;
        let stroke = binding.keystrokes().first()?.as_keystroke().clone();
        let text = Kbd::format(&stroke);
        // Windows 风格 "Ctrl+Shift+G" 按 + 拆成单键芯片；macOS 符号串无 +：
        // page 模式逐修饰键拆帽（"⌃⇧G" → ⌃ | ⇧ | G），普通字符连续段合一
        let keys: Vec<String> = if text.contains('+') {
            text.split('+').map(|s| s.to_string()).collect()
        } else if page {
            let mut keys = Vec::new();
            let mut run = String::new();
            for ch in text.chars() {
                if matches!(ch, '⌃' | '⌥' | '⇧' | '⌘') {
                    if !run.is_empty() {
                        keys.push(std::mem::take(&mut run));
                    }
                    keys.push(ch.to_string());
                } else {
                    run.push(ch);
                }
            }
            if !run.is_empty() {
                keys.push(run);
            }
            keys
        } else {
            vec![text]
        };
        Some(
            h_flex()
                .gap_1()
                .flex_shrink_0()
                .children(keys.into_iter().map(|key| {
                    let chip = div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_center()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(key);
                    if page {
                        chip.min_w_5()
                            .h_5()
                            .rounded(cx.theme().radius)
                            .border_1()
                            .border_color(cx.theme().border)
                    } else {
                        chip.px_1()
                            .py_0p5()
                            .min_w_5()
                            .rounded(cx.theme().radius.half())
                            .bg(cx.theme().muted)
                    }
                    .into_any_element()
                }))
                .into_any_element(),
        )
    }

    /// 菜单行：图标 + 名称 + 快捷键芯片；disabled 为占位项（不可点）。
    /// page = 面板首页：整列居中、带边框键帽；否则（「+」下拉菜单）：
    /// 紧凑行。两种模式快捷键都贴行右缘。
    pub(crate) fn render_right_menu_row(
        &self,
        ix: usize,
        page: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (label, icon, shortcut, disabled, tab) = Self::right_menu_items()[ix].clone();
        let chips =
            shortcut.and_then(|action| self.render_shortcut_chips(action, page, window, cx));
        h_flex()
            .id(("right-menu-item", ix))
            .w_full()
            .px_2()
            .gap_2()
            .map(|this| if page { this.py_2() } else { this.py_1p5() })
            .rounded(cx.theme().radius)
            .when(disabled, |this| this.opacity(0.5))
            .when(!disabled, |this| {
                this.cursor_pointer()
                    .hover(|this| this.bg(cx.theme().accent))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.right_menu_open = false;
                        if let Some(tab) = tab.clone() {
                            this.open_right_tab(tab, cx);
                        }
                    }))
            })
            .child(Icon::new(icon).size_4().text_color(if disabled {
                cx.theme().muted_foreground
            } else {
                cx.theme().foreground
            }))
            .child(
                div()
                    .when(page, |this| this.text_xs())
                    .when(!page, |this| this.text_sm())
                    .flex_1()
                    .whitespace_nowrap()
                    .text_color(if disabled {
                        cx.theme().muted_foreground
                    } else {
                        cx.theme().foreground
                    })
                    .child(label),
            )
            .when_some(chips, |this, chips| this.child(chips))
            .into_any_element()
    }

    /// 面板首页（菜单页）：展开面板但没有打开的 tab 时显示——
    /// 改动/浏览器/终端/侧边聊天四项（ZCode 同款，相当于面板的首页）。
    /// 宽松大行整列居中：行宽上限 320、名称贴左键帽贴右；上限固定，
    /// 面板拖宽时行不晃。
    pub(crate) fn render_right_menu_page(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        v_flex()
            .size_full()
            .justify_center()
            .items_center()
            .child(
                v_flex()
                    .w_full()
                    .max_w(px(280.))
                    .px_2()
                    .gap_1()
                    .children((0..4).map(|ix| self.render_right_menu_row(ix, true, window, cx))),
            )
            .into_any_element()
    }

    /// 标签页栏 "+" 的加面板菜单：deferred 到窗口层绘制，`Positioner::side(Bottom)`
    /// 锚定 "+" 按钮正下方（gpui-kit 的 dropdown_menu 走 corner 锚定，BottomRight
    /// 会把菜单弹到按钮上方、超出窗口顶部；且弹层盖住标题栏 HTCAPTION 拖拽区时
    /// 点击会被系统的窗口移动模态循环吞掉——故自绘，与 turn 导航条预览卡同一模式）。
    pub(crate) fn render_right_menu_dropdown(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let bounds = self.tab_add_btn_bounds.get();
        deferred(
            Positioner::side(bounds)
                .placement(Placement::Bottom)
                .align(Align::End)
                .offset(px(6.))
                .margin(px(8.))
                .occlude()
                .child(
                    v_flex()
                        .id("right-menu")
                        .w(px(220.))
                        .p_1()
                        .bg(cx.theme().popover)
                        .border_1()
                        .border_color(cx.theme().border)
                        .rounded_lg()
                        .shadow_lg()
                        .on_mouse_down_out(cx.listener(|this, event: &MouseDownEvent, _, cx| {
                            this.right_menu_open = false;
                            this.right_menu_outside_close = Some(event.position);
                            cx.notify();
                        }))
                        .children(
                            (0..4).map(|ix| self.render_right_menu_row(ix, false, window, cx)),
                        ),
                ),
        )
        .with_priority(1)
        .into_any_element()
    }

    /// 右侧标签页栏的单个 tab：图标 + 名称 + 关闭按钮（点击激活，× 关闭）
    pub(crate) fn render_right_tab(&self, tab: RightTab, cx: &mut Context<Self>) -> AnyElement {
        let active = self.right_active.as_ref() == Some(&tab);
        // 「子代理」tab：Bot 图标 + 面板标题（description 截断）；「改动」为内置页
        let (icon, label) = match &tab {
            RightTab::Changes => (
                Icon::new(AssetsIconName::GitBranch)
                    .size_3p5()
                    .into_any_element(),
                "改动".to_string(),
            ),
            RightTab::Subagent { agent_id } => {
                let title = self
                    .subagent_tabs
                    .get(agent_id)
                    .map(|panel| panel.read(cx).title().to_string())
                    .unwrap_or_else(|| "子代理".to_string());
                (
                    Icon::new(IconName::Bot).size_3p5().into_any_element(),
                    truncate_tab_label(&title),
                )
            }
        };
        h_flex()
            .id(format!("right-tab-{}", tab.key()))
            .gap_2()
            .pl_3()
            .pr_1()
            .py_1()
            .rounded(cx.theme().radius)
            .cursor_pointer()
            .when(active, |this| this.bg(cx.theme().accent))
            .when(!active, |this| {
                this.text_color(cx.theme().muted_foreground)
                    .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
            })
            .child(icon)
            .child(div().text_sm().child(label))
            .child(
                div()
                    .id(format!("right-tab-close-{}", tab.key()))
                    .p(px(1.))
                    .rounded(cx.theme().radius)
                    .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
                    .child(
                        Icon::new(IconName::Close)
                            .size_3()
                            .text_color(cx.theme().muted_foreground),
                    )
                    .on_click(cx.listener({
                        let tab = tab.clone();
                        move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.close_right_tab(tab.clone(), cx);
                        }
                    })),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.right_active = Some(tab.clone());
                this.right_open = true;
                cx.notify();
            }))
            .into_any_element()
    }

    /// 右侧面板顶部的标签页栏：tab 列表 + 末尾 "+"（加 tab 菜单）与收起按钮
    pub(crate) fn render_right_tab_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .w_full()
            .flex_shrink_0()
            .h(px(36.))
            .pl_2()
            .pr_1()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .children(
                self.right_tabs
                    .iter()
                    .map(|tab| self.render_right_tab(tab.clone(), cx)),
            )
            .child(div().flex_1())
            .child(
                div()
                    .id("right-tab-add-btn")
                    .on_prepaint({
                        let cell = self.tab_add_btn_bounds.clone();
                        move |bounds, _, _| cell.set(bounds)
                    })
                    .child(
                        Button::new("right-tab-add")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Plus)
                            .on_click(cx.listener(|this, event: &ClickEvent, _, cx| {
                                this.toggle_right_menu(event, cx);
                            })),
                    ),
            )
            .child(
                Button::new("right-panel-collapse")
                    .ghost()
                    .xsmall()
                    .icon(IconName::PanelRightClose)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.right_open = false;
                        cx.notify();
                    })),
            )
    }

    /// 右 dock 面板内容：tab 栏 +（有激活 tab 显示其内容，没有则显示面板首页/菜单页）
    pub(crate) fn render_right_dock_content(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let current_views = self.current.as_ref().and_then(|id| self.views.get(id));
        let content: AnyElement = match &self.right_active {
            Some(RightTab::Changes) => match current_views {
                Some(views) => views.review.clone().into_any_element(),
                None => v_flex()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child("开始会话后，这里会显示工作区改动"),
                    )
                    .into_any_element(),
            },
            Some(RightTab::Subagent { agent_id }) => match self.subagent_tabs.get(agent_id) {
                Some(panel) => panel.clone().into_any_element(),
                None => self.render_right_menu_page(window, cx),
            },
            None => self.render_right_menu_page(window, cx),
        };
        // 开合动画锚定层（同 Sidebar::render）：dock_frame 自带 overflow_hidden，
        // 补间期间 dock 实宽小于内容宽；内容固定 right_w 并左锚贴分隔线，收拢时
        // 整体右滑被裁而非压缩重排。稳态实宽 == right_w，绝对定位子层正好铺满
        div()
            .relative()
            .size_full()
            .overflow_hidden()
            .child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left_0()
                    .w(px(self.right_w))
                    .child(
                        v_flex()
                            .size_full()
                            // 分隔线由 dock 把手自带线绘制（与侧栏一致，不再自画 border_l）
                            .child(self.render_right_tab_bar(cx))
                            .child(div().flex_1().min_h_0().child(content)),
                    ),
            )
            .into_any_element()
    }
}
