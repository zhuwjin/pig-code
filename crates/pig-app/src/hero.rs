use super::*;

impl AppView {
    /// hero 态：当前无会话，或当前会话没有任何消息
    pub(crate) fn is_hero(&self, cx: &App) -> bool {
        match &self.current {
            None => true,
            Some(sid) => self
                .views
                .get(sid)
                .map(|views| views.thread.read(cx).is_empty())
                .unwrap_or(true),
        }
    }


    pub(crate) fn push_hero_info(&self, cx: &mut Context<Self>) {
        let label = self
            .hero_cwd
            .as_ref()
            .map(|cwd| {
                cwd.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| cwd.display().to_string())
            })
            .unwrap_or_else(|| "选择工作区".to_string());
        let mut cwds: Vec<String> = self
            .metas
            .iter()
            .map(|m| m.cwd.display().to_string())
            .collect();
        if let Some(cwd) = &self.hero_cwd {
            let current = cwd.display().to_string();
            if !cwds.contains(&current) {
                cwds.insert(0, current);
            }
        }
        let mut seen = std::collections::HashSet::new();
        cwds.retain(|c| seen.insert(c.clone()));
        let (branch, branches, is_git) = (
            self.hero_branch.clone(),
            self.hero_branches.clone(),
            self.hero_is_git,
        );
        let cwd = self.hero_cwd.as_ref().map(|c| c.display().to_string());
        self.composer.update(cx, |composer, cx| {
            composer.set_hero_info(cwd, label, cwds, branch, branches, is_git, cx);
        });
    }


    /// hero 默认值：把「工作区最近活跃会话」的模型/模式/思考等级铺到 composer，
    /// 作为下次新建会话的默认值（用户可再改；hero_send 时按当前选择创建）。
    /// 无种子（新工作区）则不动，保留 app 默认/上次选择。
    pub(crate) fn apply_hero_defaults(&mut self, cx: &mut Context<Self>) {
        let cwd = self.hero_cwd.clone().unwrap_or_else(|| self.cwd.clone());
        let Some(seed) = self
            .metas
            .iter()
            .filter(|m| !m.archived && m.cwd == cwd)
            .max_by_key(|m| m.updated_at)
            .cloned()
        else {
            return;
        };
        self.exec_mode = seed.exec_mode;
        self.reasoning_level = seed.reasoning_level.clone();
        // 模型显示名：config 里按 provider_id 查供应商名，查不到退化为 model_id。
        // hero 态用户已显式选过模型时不覆盖（模式/思考等级仍铺种子）
        let label = match (&seed.provider_id, &seed.model_id) {
            (Some(p), Some(m)) if !self.hero_model_dirty => {
                self.current_model = Some((p.clone(), m.clone()));
                self.model_display_label(p, m)
            }
            // 种子没有模型选择：模型展示不动，只铺模式/思考等级
            _ => String::new(),
        };
        self.composer.update(cx, |composer, cx| {
            composer.set_exec_mode(seed.exec_mode, cx);
            composer.set_reasoning_level(seed.reasoning_level.clone(), cx);
            composer.set_fs_access(seed.fs_read_outside, seed.fs_write_outside, cx);
            if !label.is_empty() {
                composer.set_model_name(label, cx);
            }
        });
        cx.notify();
    }


    pub(crate) fn enter_hero(&mut self, cx: &mut Context<Self>) {
        self.current = None;
        self.git_branch = None;
        self.title_branches = vec![];
        self.title_branch_menu_open = false;
        self.hero_error = None;
        // 新的 hero 周期：显式模型选择标记复位（工作区种子重新生效）
        self.hero_model_dirty = false;
        self.composer.update(cx, |composer, cx| {
            composer.clear_context_usage(cx);
            // 进度/任务/改动 chip 同属上个会话的状态，一并清掉（setter 会收起对应弹层）
            composer.set_todos(vec![], cx);
            composer.set_tasks(vec![], cx);
            composer.set_changes(0, 0, vec![], cx);
        });
        if let Some(cwd) = self.hero_cwd.clone() {
            self.agent.git_info(cwd);
        } else {
            self.hero_branch = None;
            self.hero_branches = vec![];
            self.hero_is_git = false;
        }
        self.composer.update(cx, |composer, cx| {
            composer.set_hero_mode(true, cx);
            composer.set_streaming(false, cx);
        });
        self.push_hero_info(cx);
        self.refresh_sidebar(cx);
        self.apply_hero_defaults(cx);
        cx.notify();
    }


    pub(crate) fn hero_send(
        &mut self,
        text: String,
        files: Vec<String>,
        images: Vec<pig_protocol::PendingImage>,
        mode: ExecMode,
        cx: &mut Context<Self>,
    ) {
        self.pending_first_send = Some((text, files, images, mode));
        let cwd = self.hero_cwd.clone().unwrap_or_else(|| self.cwd.clone());
        // 带上 UI 当前选择：新建会话用它们（而不是工作区种子）初始化，
        // 避免 SessionConfigured 回来把用户刚选的模式/思考等级覆盖掉
        let (provider_id, model_id) = match self.current_model.clone() {
            Some((p, m)) => (Some(p), Some(m)),
            None => (None, None),
        };
        eprintln!(
            "[model] hero_send 新建会话：cwd={} 模型={:?} 思考={:?}",
            cwd.display(),
            provider_id.as_deref().zip(model_id.as_deref()),
            self.reasoning_level
        );
        self.agent.new_session(
            cwd,
            provider_id,
            model_id,
            self.reasoning_level.clone(),
            Some(mode),
        );
        self.composer.update(cx, |composer, cx| {
            composer.set_hero_mode(false, cx);
        });
        cx.notify();
    }


    /// 关闭 Yolo 确认框并回焦输入框
    pub(crate) fn close_yolo_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.yolo_confirm_open = false;
        self.composer.update(cx, |composer, cx| {
            composer.focus_input(window, cx);
        });
        cx.notify();
    }


    /// Yolo 确认框「开启无管制模式」：应用模式并关闭
    pub(crate) fn confirm_yolo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_yolo_confirm(window, cx);
        self.apply_exec_mode(ExecMode::Yolo, cx);
    }


    /// 「开启无管制模式？」确认框（ModelDialog 同款覆盖层：遮罩 + 居中卡片）。
    /// 取消/点遮罩/Esc 不生效；确认才切 Yolo。每次切换都弹，不记住选择。
    pub(crate) fn render_yolo_confirm(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("yolo-confirm-overlay")
            .absolute()
            .inset_0()
            .bg(gpui_kit::black().opacity(0.5))
            .flex()
            .items_center()
            .justify_center()
            .on_click(cx.listener(|this, _, window, cx| {
                this.close_yolo_confirm(window, cx);
            }))
            .child(
                v_flex()
                    .id("yolo-confirm")
                    .track_focus(&self.yolo_confirm_focus)
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                        if event.keystroke.key == "escape" {
                            this.close_yolo_confirm(window, cx);
                        }
                    }))
                    .on_click(|_, _, cx| cx.stop_propagation()) // 点卡片不触发遮罩取消
                    .w(px(420.))
                    .gap_3()
                    .p_4()
                    .rounded(cx.theme().radius_lg)
                    .bg(cx.theme().popover)
                    .border_1()
                    .border_color(cx.theme().border)
                    .child(
                        div()
                            .text_lg()
                            .font_semibold()
                            .text_color(cx.theme().danger)
                            .child("开启无管制模式？"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child("此模式下所有操作直接执行：不弹任何确认，危险命令也不再拦截。仅建议在容器、虚拟机等隔离环境中使用。")
                            .child("注意：敏感文件（.env / 私钥 / 云凭据）仍会拦截。"),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .justify_end()
                            .child(
                                // 与问卷/审批条按钮同尺寸（Small），确认弹框按钮字号一致
                                Button::new("yolo-cancel")
                                    .label("取消")
                                    .outline()
                                    .small()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.close_yolo_confirm(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("yolo-confirm")
                                    .label("开启无管制模式")
                                    .danger()
                                    .small()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.confirm_yolo(window, cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }


    pub(crate) fn sync_hero_mode(&mut self, cx: &mut Context<Self>) {
        let hero = self.is_hero(cx) && self.pending_first_send.is_none();
        self.composer
            .update(cx, |composer, cx| composer.set_hero_mode(hero, cx));
        if hero {
            self.push_hero_info(cx);
        }
    }


    /// 自测用。
    pub fn debug_is_hero(&self, cx: &App) -> bool {
        self.is_hero(cx)
    }


    pub(crate) fn render_hero(&self, cx: &mut Context<Self>) -> AnyElement {
        let hour = time::OffsetDateTime::now_local()
            .map(|t| t.hour() as i64)
            .unwrap_or_else(|_| {
                let secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0) as i64;
                (secs / 3600 + 8) % 24
            });
        let greeting = match hour {
            5..=11 => "上午好呀",
            12..=17 => "下午好呀",
            _ => "晚上好呀",
        };

        let chips: Vec<(&'static str, &'static str)> = vec![
            (
                "总结这个工作区",
                "请阅读 README 并总结这个工作区的结构和主要模块。",
            ),
            ("修复一个报错", "我遇到了一个报错："),
            ("写单元测试", "请为主要模块写单元测试。"),
        ];

        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_5()
            .child(
                div()
                    .text_xl()
                    .font_semibold()
                    .child(format!("{greeting}，接下来交给我吧")),
            )
            .child(
                div()
                    .w_full()
                    .max_w(px(720.))
                    .px_4()
                    .child(self.composer.clone()),
            )
            .child(
                h_flex()
                    .gap_2()
                    .children(chips.into_iter().map(|(label, fill)| {
                        let composer = self.composer.clone();
                        h_flex()
                            .id(("chip", label.len()))
                            .px_3()
                            .py_1()
                            .rounded_full()
                            .border_1()
                            .border_color(cx.theme().border)
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .cursor_pointer()
                            .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
                            .child(label)
                            .on_click(move |_, window, cx| {
                                composer.update(cx, |composer, cx| {
                                    composer.fill_text(fill, window, cx);
                                });
                            })
                    })),
            )
            .when_some(self.hero_error.clone(), |this, error| {
                this.child(div().text_xs().text_color(cx.theme().danger).child(error))
            })
            .into_any_element()
    }


}
