use super::*;

impl Composer {
    /// 工作区选择面板：Command 面板（搜索框 + 工作区列表 + 操作行），锚定在工作区芯片正上方。
    pub(crate) fn render_cwd_popup(&self, cx: &mut Context<Self>) -> AnyElement {
        let on_confirm_composer = cx.entity();
        let on_cancel_composer = cx.entity();

        let command = Command::new(&self.cwd_command)
            .placeholder("搜索工作区")
            .items(self.hero_cwds.iter().map(|cwd| {
                let name = std::path::Path::new(cwd)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| cwd.clone());
                CommandItem::new()
                    .label(name)
                    .keywords([cwd.clone()])
                    .icon(IconName::Folder)
                    .checked(self.hero_cwd.as_deref() == Some(cwd.as_str()))
            }))
            .separator()
            .item(
                CommandItem::new()
                    .label("打开文件夹")
                    .icon(IconName::FolderOpen),
            )
            .when(self.hero_cwd.is_some(), |this| {
                this.item(
                    CommandItem::new()
                        .label("不在工作区中工作")
                        .icon(IconName::CircleX),
                )
            })
            .empty(|_, _, cx| {
                div()
                    .px_2()
                    .py_1p5()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("没有匹配的工作区")
            })
            .on_confirm(move |ix, window, cx| {
                on_confirm_composer.update(cx, |this, cx| {
                    let len = this.hero_cwds.len();
                    if let Some(cwd) = this.hero_cwds.get(ix.row) {
                        cx.emit(ComposerEvent::SelectCwd(cwd.clone()));
                    } else if ix.row == len {
                        cx.emit(ComposerEvent::PickDirectory);
                    } else {
                        cx.emit(ComposerEvent::ClearCwd);
                    }
                    this.close_command_popup(window, cx);
                });
            })
            .on_cancel(move |window, cx| {
                on_cancel_composer.update(cx, |this, cx| {
                    this.close_command_popup(window, cx);
                });
            });
        self.command_popup_shell(
            "composer-cwd-popup",
            &self.cwd_command,
            command,
            PopupAnchor::Left,
            cx,
        )
    }

    /// 分支选择面板：Command 面板（搜索框 + 分支列表），锚定在分支芯片正上方。
    pub(crate) fn render_branch_popup(&self, cx: &mut Context<Self>) -> AnyElement {
        let on_confirm_composer = cx.entity();
        let on_cancel_composer = cx.entity();

        let command = Command::new(&self.branch_command)
            .placeholder("搜索分支")
            .items(self.hero_branches.iter().map(|branch| {
                CommandItem::new()
                    .label(branch.clone())
                    .keywords([branch.clone()])
                    .icon(IconName::Github)
                    .checked(self.hero_branch.as_deref() == Some(branch.as_str()))
            }))
            .empty(|_, _, cx| {
                div()
                    .px_2()
                    .py_1p5()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("没有匹配的分支")
            })
            .on_confirm(move |ix, window, cx| {
                on_confirm_composer.update(cx, |this, cx| {
                    if let Some(branch) = this.hero_branches.get(ix.row) {
                        cx.emit(ComposerEvent::CheckoutBranch(branch.clone()));
                    }
                    this.close_command_popup(window, cx);
                });
            })
            .on_cancel(move |window, cx| {
                on_cancel_composer.update(cx, |this, cx| {
                    this.close_command_popup(window, cx);
                });
            });
        self.command_popup_shell(
            "composer-branch-popup",
            &self.branch_command,
            command,
            PopupAnchor::Left,
            cx,
        )
    }

    /// 执行模式面板：Command 单选模式列表（键盘导航保持可用）。
    /// 「工作区外访问」开关区放在 Command 的 footer 槽：模式（单选）与开关（多选）
    /// 分区展示，开关行不参与 Command 的键盘选择（鼠标交互，可接受的取舍）。
    pub(crate) fn render_exec_mode_popup(&self, cx: &mut Context<Self>) -> AnyElement {
        let on_confirm_composer = cx.entity();
        let on_cancel_composer = cx.entity();

        let command = Command::new(&self.exec_command)
            .searchable(false)
            .items(
                EXEC_MODES
                    .iter()
                    .enumerate()
                    .map(|(ix, (label, desc, mode))| {
                        let icon = exec_mode_icon(*mode);
                        CommandItem::new()
                            .label(*label)
                            .checked(ix == self.exec_mode)
                            .child(move |_, cx| {
                                // 图标与 label 按模式危险程度上色（显式 text_color，
                                // hover/选中的继承色盖不住模式色）；描述保持 muted
                                let mode_color = exec_mode_color(*mode, cx);
                                h_flex()
                                    .flex_1()
                                    .gap_2()
                                    .items_center()
                                    .child(Icon::new(icon).size_4().text_color(mode_color))
                                    .child(
                                        v_flex()
                                            .child(div().text_color(mode_color).child(*label))
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(cx.theme().muted_foreground)
                                                    .child(*desc),
                                            ),
                                    )
                            })
                    }),
            )
            .on_confirm(move |ix, window, cx| {
                on_confirm_composer.update(cx, |this, cx| {
                    if let Some((_, _, mode)) = EXEC_MODES.get(ix.row) {
                        if *mode == ExecMode::Yolo && this.exec_mode != ix.row {
                            // 无管制模式：先关弹层再弹确认框（确认后才 emit SetExecMode）
                            this.close_command_popup(window, cx);
                            cx.emit(ComposerEvent::RequestYoloConfirm);
                            return;
                        }
                        this.exec_mode = ix.row;
                        cx.emit(ComposerEvent::SetExecMode(*mode));
                    }
                    this.close_command_popup(window, cx);
                });
            })
            .on_cancel(move |window, cx| {
                on_cancel_composer.update(cx, |this, cx| {
                    this.close_command_popup(window, cx);
                });
            })
            // footer 槽：分隔线 + 单行开关区（「工作区外访问  读 ☐  写 ☑」）；
            // 复选框即开关态视觉，不再用 CommandItem.checked 的 ✓
            .footer({
                let composer = cx.entity();
                let read_on = self.fs_read_outside;
                let write_on = self.fs_write_outside;
                move |_, _window, cx| {
                    let muted = cx.theme().muted_foreground;
                    v_flex()
                        .w_full()
                        .pt_1()
                        .child(Separator::horizontal())
                        .child(
                            h_flex()
                                .w_full()
                                .px_2()
                                .py_1p5()
                                .items_center()
                                .justify_between()
                                .child(div().text_xs().text_color(muted).child("工作区外访问"))
                                .child(
                                    h_flex()
                                        .gap_3()
                                        .items_center()
                                        .child(fs_toggle(
                                            "fs-access-read",
                                            "读",
                                            read_on,
                                            true,
                                            composer.clone(),
                                            cx,
                                        ))
                                        .child(fs_toggle(
                                            "fs-access-write",
                                            "写",
                                            write_on,
                                            false,
                                            composer.clone(),
                                            cx,
                                        )),
                                ),
                        )
                        .into_any_element()
                }
            });
        // 定宽 300px：开关区描述文字按宽度换行，不再被弹层裁切
        let content = v_flex().w(px(300.)).child(command).into_any_element();
        self.popup_shell(
            "composer-exec-popup",
            content,
            PopupAnchor::Left,
            Some(self.exec_command.clone()),
            cx,
        )
    }

    /// 模型面板：搜索框 + 按供应商分组的模型列表 + 「管理模型」操作行。
    pub(crate) fn render_model_popup(&self, cx: &mut Context<Self>) -> AnyElement {
        let on_confirm_composer = cx.entity();
        let on_cancel_composer = cx.entity();

        // 按供应商分组，保持配置中的出现顺序
        let mut groups: Vec<(String, Vec<ModelOption>)> = Vec::new();
        for model in &self.models {
            match groups.iter_mut().find(|(name, _)| *name == model.0) {
                Some((_, items)) => items.push(model.clone()),
                None => groups.push((model.0.clone(), vec![model.clone()])),
            }
        }

        let mut command = Command::new(&self.model_command).placeholder("搜索模型");
        if groups.is_empty() {
            command = command.item(
                CommandItem::new()
                    .label("还没有配置模型，去设置页添加")
                    .icon(IconName::Info),
            );
        } else {
            for (provider_name, items) in &groups {
                command = command.group(CommandGroup::new().label(provider_name.clone()).items(
                    items.iter().map(|(provider_name, _, model_id, _)| {
                        CommandItem::new()
                            .label(model_id.clone())
                            .keywords([provider_name.clone()])
                            .checked(self.model == format!("{provider_name}/{model_id}"))
                    }),
                ));
            }
        }
        // 未分组项固定为 section 0，分组从 section 1 起（与 entries 书写顺序无关）
        command = command.separator().item(
            CommandItem::new()
                .label("管理模型")
                .icon(AssetIconName::Settings),
        );
        let command = command
            .empty(|_, _, cx| {
                div()
                    .px_2()
                    .py_1p5()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("没有匹配的模型")
            })
            .on_confirm(move |ix, window, cx| {
                on_confirm_composer.update(cx, |this, cx| {
                    if ix.section == 0 {
                        eprintln!("[model-popup] 点击「管理模型」(section=0)");
                        cx.emit(ComposerEvent::OpenSettings);
                    } else if let Some((_, items)) = groups.get(ix.section - 1)
                        && let Some((provider_name, provider_id, model_id, _)) = items.get(ix.row)
                    {
                        eprintln!(
                            "[model-popup] 选中 section={} row={} → {provider_id}/{model_id}",
                            ix.section, ix.row
                        );
                        this.model = format!("{provider_name}/{model_id}");
                        cx.emit(ComposerEvent::SetModel {
                            provider_id: provider_id.clone(),
                            model_id: model_id.clone(),
                        });
                    } else {
                        eprintln!(
                            "[model-popup] 点击未能解析为模型: section={} row={}",
                            ix.section, ix.row
                        );
                    }
                    this.close_command_popup(window, cx);
                });
            })
            .on_cancel(move |window, cx| {
                on_cancel_composer.update(cx, |this, cx| {
                    this.close_command_popup(window, cx);
                });
            });
        self.command_popup_shell(
            "composer-model-popup",
            &self.model_command,
            command,
            PopupAnchor::Right,
            cx,
        )
    }

    /// 思考等级面板：无搜索框，「关闭」+ 等级列表，当前等级勾选。
    /// levels 为 (id, 显示名)；界面展示显示名，确认回传 id。
    pub(crate) fn render_reasoning_popup(
        &self,
        levels: &[(String, String)],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let on_confirm_composer = cx.entity();
        let on_cancel_composer = cx.entity();
        let levels = levels.to_vec();

        let command = Command::new(&self.reasoning_command)
            .searchable(false)
            .item(
                CommandItem::new()
                    .label("关闭")
                    .checked(self.reasoning_level.is_none()),
            )
            .items(levels.iter().map(|(id, label)| {
                // 显示名与 id 不同则括号附上 id，避免歧义
                let text = if label == id {
                    id.clone()
                } else {
                    format!("{label}（{id}）")
                };
                CommandItem::new()
                    .label(text)
                    .checked(self.reasoning_level.as_deref() == Some(id.as_str()))
            }))
            .on_confirm(move |ix, window, cx| {
                on_confirm_composer.update(cx, |this, cx| {
                    let level = if ix.row == 0 {
                        None
                    } else {
                        levels.get(ix.row - 1).map(|(id, _)| id.clone())
                    };
                    this.reasoning_level = level.clone();
                    cx.emit(ComposerEvent::SetReasoning(level));
                    this.close_command_popup(window, cx);
                });
            })
            .on_cancel(move |window, cx| {
                on_cancel_composer.update(cx, |this, cx| {
                    this.close_command_popup(window, cx);
                });
            });
        self.command_popup_shell(
            "composer-reasoning-popup",
            &self.reasoning_command,
            command,
            PopupAnchor::Right,
            cx,
        )
    }
}
