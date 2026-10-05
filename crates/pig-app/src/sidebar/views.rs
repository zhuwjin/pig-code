use super::*;

impl Sidebar {
    pub(crate) fn render_action_rows(&self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .p_2()
            .gap_1()
            .child(
                h_flex()
                    .id("new-task")
                    .gap_2()
                    .px_2()
                    .py_1()
                    .w_full()
                    .rounded(cx.theme().radius)
                    .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
                    .on_click(cx.listener(|_, _, _, cx| {
                        cx.emit(SidebarEvent::NewTask);
                    }))
                    .child(Icon::new(IconName::Plus).size_4())
                    .child(div().text_sm().flex_1().child("新建任务"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("Ctrl+N"),
                    ),
            )
            .child(
                h_flex()
                    .id("open-search")
                    .gap_2()
                    .px_2()
                    .py_1()
                    .w_full()
                    .rounded(cx.theme().radius)
                    .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_search(window, cx);
                    }))
                    .child(Icon::new(IconName::Search).size_4())
                    .child(div().text_sm().flex_1().child("搜索"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("Ctrl+K"),
                    ),
            )
            .when(self.search_open, |this| {
                this.child(
                    div()
                        .key_context("search")
                        .on_action(cx.listener(|this, _: &crate::CloseSearch, window, cx| {
                            this.search_open = false;
                            this.search_input.update(cx, |input, cx| {
                                input.set_value("", window, cx);
                            });
                            cx.notify();
                        }))
                        .child(Input::new(&self.search_input).small()),
                )
            })
            .into_any_element()
    }

    /// 「会话」标题行：折叠/展开全部工作区（仅分组视图）+ 列表管理菜单
    pub(crate) fn render_list_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let any_expanded = self.workspaces.iter().any(|w| self.expanded.contains(w));
        let mut row = h_flex().pl_3().pr_2().py_1().gap_1().child(
            div()
                .flex_1()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child("会话"),
        );
        if self.view == SidebarView::Workspace {
            row = row.child(
                Button::new("toggle-all-workspaces")
                    .ghost()
                    .xsmall()
                    .icon(if any_expanded {
                        AssetsIconName::FoldVertical
                    } else {
                        AssetsIconName::UnfoldVertical
                    })
                    .tooltip(if any_expanded {
                        "折叠全部工作区"
                    } else {
                        "展开全部工作区"
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        let any_expanded =
                            this.workspaces.iter().any(|w| this.expanded.contains(w));
                        if any_expanded {
                            // 折叠全部：即时卸载（不播动画），并取消进行中的收起态
                            this.expanded.clear();
                            for anim in this.expand_anims.values_mut() {
                                anim.collapsing = false;
                            }
                        } else {
                            // 展开全部：逐个播滑开动画；generation+1 同时作废
                            // 进行中的收起卸载计时器
                            for w in &this.workspaces {
                                this.expanded.insert(w.clone());
                                let anim = this.expand_anims.entry(w.clone()).or_default();
                                anim.generation += 1;
                                anim.collapsing = false;
                            }
                        }
                        cx.notify();
                    })),
            );
        }
        row.child(
            Button::new("list-manage")
                .ghost()
                .xsmall()
                .icon(AssetsIconName::SlidersHorizontal)
                .tooltip("列表管理")
                .dropdown_menu_with_anchor(
                    Anchor::TopRight,
                    Self::view_menu(&cx.entity().downgrade(), self.view),
                ),
        )
        .into_any_element()
    }

    pub(crate) fn render_session_row(
        &self,
        window: &Window,
        ix: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let session = &self.sessions[ix];
        let id = session.id.clone();
        let active = self.active.as_deref() == Some(session.id.as_str());
        // 状态指示：待审批黄点；运行中不画点，转圈占用行尾时间槽（见下）
        let status_color = if session.waiting_approval {
            Some(cx.theme().warning)
        } else {
            None
        };
        let renaming = self.renaming == Some(RenameTarget::Session(session.id.clone()));
        let hovered = self.hovered_session.as_deref() == Some(session.id.as_str());
        // 渐隐底色 = 行背景：选中与悬停同为 sidebar + accent 60%，常态 sidebar
        let (fade_base, fade_tint) = if hovered || active {
            (cx.theme().sidebar, Some(cx.theme().accent.opacity(0.6)))
        } else {
            (cx.theme().sidebar, None)
        };

        let hover_id = session.id.clone();
        let mut row = h_flex()
            .id(("session", ix))
            .mx_2()
            .px_2()
            .py_1()
            .gap_2()
            .rounded(cx.theme().radius)
            .when(session.archived, |this| {
                this.text_color(cx.theme().muted_foreground)
            })
            .when(active, |this| this.bg(cx.theme().accent.opacity(0.6)))
            .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.emit(SidebarEvent::Select(id.clone()));
            }))
            .on_hover(cx.listener(move |this, is_hovered: &bool, _, cx| {
                let was = this.hovered_session.as_deref() == Some(hover_id.as_str());
                if *is_hovered != was {
                    this.hovered_session = (*is_hovered).then(|| hover_id.clone());
                    cx.notify();
                }
            }));
        if renaming {
            // 行内重命名：只留输入框（回车/失焦提交，空值取消）
            row = row.child(div().flex_1().child(Input::new(&self.rename_input).small()));
            return row.into_any_element();
        }
        row = row
            .child(self.render_title_scroll(
                &session.id,
                ("session-title", ix),
                &session.title,
                fade_base,
                fade_tint,
                window,
                cx,
            ))
            .when_some(status_color, |this, color| {
                this.child(div().size_2().rounded_full().bg(color))
            });
        if !hovered {
            // 悬停时行尾让位给置顶/归档按钮；运行中时间换成转圈
            row = row.child(if session.running {
                Spinner::new()
                    .xsmall()
                    .color(cx.theme().muted_foreground)
                    .into_any_element()
            } else {
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(session.updated_at.relative())
                    .into_any_element()
            });
        }
        if hovered {
            // 悬停才渲染置顶/归档按钮：让出的宽度归标题，此时才裁减文字
            row = row
                .child({
                    let id = session.id.clone();
                    let pinned = session.pinned;
                    Button::new(("pin", ix))
                        .ghost()
                        .xsmall()
                        .icon(if pinned {
                            IconName::StarFill
                        } else {
                            IconName::Star
                        })
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(SidebarEvent::SetPinned(id.clone(), !pinned));
                        }))
                })
                .child({
                    let id = session.id.clone();
                    let archived = session.archived;
                    Button::new(("archive", ix))
                        .ghost()
                        .xsmall()
                        .icon(if archived {
                            IconName::Undo2
                        } else {
                            IconName::Inbox
                        })
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(SidebarEvent::SetArchived(id.clone(), !archived));
                        }))
                });
        }
        // 右键菜单：重命名 / 置顶 / 归档 / 删除
        let menu = Self::session_menu(&cx.entity().downgrade(), session);
        row.context_menu(menu).into_any_element()
    }

    /// 平铺列表：跨工作区的单一时间线，置顶会话在前（无小节头），
    /// 行内第二行标注所属工作区；归档会话在设置页「已归档的会话」管理
    pub(crate) fn render_flat_view(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let query = self.query(cx);
        let mut pinned: Vec<usize> = vec![];
        let mut rest: Vec<usize> = vec![];
        for (ix, s) in self.sessions.iter().enumerate() {
            if s.archived || !self.matches(&query, &s.title) {
                continue;
            }
            if s.pinned {
                pinned.push(ix);
            } else {
                rest.push(ix);
            }
        }
        let by_recency = |a: &usize, b: &usize| {
            self.sessions[*b]
                .updated_at
                .cmp(&self.sessions[*a].updated_at)
        };
        pinned.sort_by(by_recency);
        rest.sort_by(by_recency);

        // 归档会话不在侧栏渲染：设置页「已归档的会话」统一管理
        pinned
            .into_iter()
            .chain(rest)
            .map(|ix| self.render_detailed_session_row(window, ix, cx))
            .collect()
    }

    /// 双行详情会话行：标题 + 时间一行，所属工作区一行。用于置顶区
    /// （跨工作区集中展示需标注归属）与平铺视图
    pub(crate) fn render_detailed_session_row(
        &self,
        window: &Window,
        ix: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let session = &self.sessions[ix];
        let id = session.id.clone();
        let active = self.active.as_deref() == Some(session.id.as_str());
        let hovered = self.hovered_session.as_deref() == Some(session.id.as_str());
        // 状态指示：待审批黄点；运行中不画点，转圈占用行尾时间槽（见下）
        let status_color = if session.waiting_approval {
            Some(cx.theme().warning)
        } else {
            None
        };
        let (fade_base, fade_tint) = if hovered || active {
            (cx.theme().sidebar, Some(cx.theme().accent.opacity(0.6)))
        } else {
            (cx.theme().sidebar, None)
        };
        let marquee_key = format!("pinned-{}", session.id);
        let workspace_name = self.workspace_name(&session.cwd.display().to_string());

        let mut line1 = h_flex()
            .gap_2()
            .child(self.render_title_scroll(
                &marquee_key,
                ("pinned-title", ix),
                &session.title,
                fade_base,
                fade_tint,
                window,
                cx,
            ))
            .when_some(status_color, |this, color| {
                this.child(div().size_2().rounded_full().bg(color))
            });
        if hovered {
            // 悬停才渲染置顶/归档按钮（与单行会话行一致），时间让位不显示，
            // 此时标题才让宽裁减
            let pin_id = session.id.clone();
            let pinned = session.pinned;
            let archive_id = session.id.clone();
            let archived = session.archived;
            line1 = line1
                .child(
                    Button::new(("pin", ix))
                        .ghost()
                        .xsmall()
                        .icon(if pinned {
                            IconName::StarFill
                        } else {
                            IconName::Star
                        })
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(SidebarEvent::SetPinned(pin_id.clone(), !pinned));
                        })),
                )
                .child(
                    Button::new(("archive", ix))
                        .ghost()
                        .xsmall()
                        .icon(if archived {
                            IconName::Undo2
                        } else {
                            IconName::Inbox
                        })
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(SidebarEvent::SetArchived(archive_id.clone(), !archived));
                        })),
                );
        } else if session.running {
            // 运行中：时间槽显示转圈
            line1 = line1.child(Spinner::new().xsmall().color(cx.theme().muted_foreground));
        } else {
            line1 = line1.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(session.updated_at.relative()),
            );
        }

        let hover_id = session.id.clone();
        let row = div()
            .id(("pinned-session", ix))
            .mx_2()
            .px_2()
            .py_1()
            .rounded(cx.theme().radius)
            .when(active, |this| this.bg(cx.theme().accent.opacity(0.6)))
            .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.emit(SidebarEvent::Select(id.clone()));
            }))
            .on_hover(cx.listener(move |this, is_hovered: &bool, _, cx| {
                let was = this.hovered_session.as_deref() == Some(hover_id.as_str());
                if *is_hovered != was {
                    this.hovered_session = (*is_hovered).then(|| hover_id.clone());
                    cx.notify();
                }
            }))
            .child(
                v_flex().gap_0p5().child(line1).child(
                    h_flex()
                        .gap_1()
                        .child(
                            Icon::new(IconName::FolderClosed)
                                .size_3()
                                .text_color(cx.theme().muted_foreground),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .child(workspace_name),
                        ),
                ),
            );
        // 右键菜单：重命名 / 置顶 / 归档 / 删除
        let menu = Self::session_menu(&cx.entity().downgrade(), session);
        row.context_menu(menu).into_any_element()
    }

    pub(crate) fn render_workspace_view(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let query = self.query(cx);
        let mut out: Vec<AnyElement> = vec![];

        // 置顶区：跨工作区集中展示置顶会话（归档的不显示），行内标注所属工作区
        let mut pinned: Vec<usize> = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| s.pinned && !s.archived && self.matches(&query, &s.title))
            .map(|(ix, _)| ix)
            .collect();
        pinned.sort_by_key(|ix| std::cmp::Reverse(self.sessions[*ix].updated_at));
        if !pinned.is_empty() {
            out.push(
                div()
                    .px_3()
                    .py_1()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("置顶")
                    .into_any_element(),
            );
            out.extend(
                pinned
                    .iter()
                    .map(|ix| self.render_detailed_session_row(window, *ix, cx)),
            );
        }

        for (p_ix, workspace) in self.workspaces.iter().enumerate() {
            let name = self.workspace_name(workspace);
            if !self.matches(&query, &name) && !self.matches(&query, workspace) {
                continue;
            }
            let expanded = self.expanded.contains(workspace);
            // 收起动画播放期间内容仍挂载：expanded 保持 true，open 立即翻 false
            //（图标即时反馈；此时再点 = 取消收起重新展开）
            let collapsing = self
                .expand_anims
                .get(workspace)
                .is_some_and(|a| a.collapsing);
            let open = expanded && !collapsing;
            let renaming = self.renaming == Some(RenameTarget::Workspace(workspace.to_string()));
            let workspace_path = workspace.clone();

            // 该工作区下的会话：置顶（入置顶区）与归档的不在此列，按 updated 倒序
            let mut sessions: Vec<usize> = self
                .sessions
                .iter()
                .enumerate()
                .filter(|(_, s)| {
                    s.cwd.display().to_string() == *workspace && !s.pinned && !s.archived
                })
                .map(|(ix, _)| ix)
                .collect();
            sessions.sort_by_key(|ix| std::cmp::Reverse(self.sessions[*ix].updated_at));

            let hovered = self.hovered_workspace.as_deref() == Some(workspace.as_str());
            let mut row = h_flex()
                .id(("workspace", p_ix))
                .relative()
                .mx_2()
                .px_2()
                .py_1()
                .gap_2()
                .rounded(cx.theme().radius)
                .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
                .child(
                    Icon::new(if open {
                        IconName::FolderOpen
                    } else {
                        IconName::FolderClosed
                    })
                    .size_4()
                    .text_color(cx.theme().muted_foreground),
                );
            if renaming {
                row = row.child(div().flex_1().child(Input::new(&self.rename_input).small()));
            } else {
                row = row.child(
                    div()
                        .text_sm()
                        .flex_1()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child(name),
                );
            }
            if renaming {
                out.push(row.into_any_element());
            } else {
                let menu = Self::workspace_menu(&cx.entity().downgrade(), workspace);
                let new_task_path = workspace.clone();
                let hover_path = workspace.clone();
                let mut row = row
                    .on_hover(cx.listener(move |this, is_hovered: &bool, _, cx| {
                        let was = this.hovered_workspace.as_deref() == Some(hover_path.as_str());
                        if *is_hovered != was {
                            this.hovered_workspace = (*is_hovered).then(|| hover_path.clone());
                            cx.notify();
                        }
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let collapsing = this
                            .expand_anims
                            .get(&workspace_path)
                            .is_some_and(|a| a.collapsing);
                        if this.expanded.contains(&workspace_path) && !collapsing {
                            // 收起：内容保持挂载播滑收动画，计时器到期才卸载；
                            // 期间再点开（代次不符）自动作废
                            let anim = this.expand_anims.entry(workspace_path.clone()).or_default();
                            anim.generation += 1;
                            anim.collapsing = true;
                            let generation = anim.generation;
                            let path = workspace_path.clone();
                            cx.spawn(async move |this, cx| {
                                cx.background_executor()
                                    .timer(EXPAND_ANIM_DUR + std::time::Duration::from_millis(50))
                                    .await;
                                this.update(cx, |this, cx| {
                                    let stale = this.expand_anims.get(&path).is_none_or(|a| {
                                        !a.collapsing || a.generation != generation
                                    });
                                    if !stale {
                                        this.expanded.remove(&path);
                                        if let Some(a) = this.expand_anims.get_mut(&path) {
                                            a.collapsing = false;
                                        }
                                        cx.notify();
                                    }
                                })
                                .ok();
                            })
                            .detach();
                        } else {
                            this.expanded.insert(workspace_path.clone());
                            let anim = this.expand_anims.entry(workspace_path.clone()).or_default();
                            anim.generation += 1;
                            anim.collapsing = false;
                        }
                        cx.notify();
                    }))
                    .context_menu(menu.clone());
                if hovered {
                    // 悬停才渲染行尾浮层：名字右端渐隐 + 选项（...）/新建任务（+）。
                    // 浮层绝对定位不占位，未悬停时名字用满行宽不被裁减；渐隐与
                    // 按钮托底各叠两层底色（sidebar + accent 60%），合成结果与行
                    // .hover() 背景一致，浮层盖住文字处无色差
                    let sidebar_bg = cx.theme().sidebar;
                    let hover_tint = cx.theme().accent.opacity(0.6);
                    row = row.child(
                        h_flex()
                            .absolute()
                            .right_2()
                            .top_0()
                            .bottom_0()
                            .child(
                                div()
                                    .w(px(20.))
                                    .h_full()
                                    .bg(linear_gradient(
                                        90.,
                                        linear_color_stop(sidebar_bg.opacity(0.), 0.),
                                        linear_color_stop(sidebar_bg, 1.),
                                    ))
                                    .child(div().size_full().bg(linear_gradient(
                                        90.,
                                        linear_color_stop(hover_tint.opacity(0.), 0.),
                                        linear_color_stop(hover_tint, 1.),
                                    ))),
                            )
                            .child(
                                div().h_full().bg(sidebar_bg).child(
                                    div().h_full().bg(hover_tint).child(
                                        h_flex()
                                            .h_full()
                                            .child(
                                                Button::new(("workspace-menu", p_ix))
                                                    .ghost()
                                                    .xsmall()
                                                    .icon(IconName::Ellipsis)
                                                    .dropdown_menu(menu.clone()),
                                            )
                                            .child(
                                                Button::new(("workspace-add", p_ix))
                                                    .ghost()
                                                    .xsmall()
                                                    .icon(IconName::Plus)
                                                    .on_click(cx.listener(move |_, _, _, cx| {
                                                        cx.emit(SidebarEvent::NewTaskInWorkspace(
                                                            new_task_path.clone(),
                                                        ));
                                                    })),
                                            ),
                                    ),
                                ),
                            ),
                    );
                }
                out.push(row.into_any_element());
            }

            if expanded || collapsing {
                // 分页：默认一页（5 条），展开更多每次 +1 页，收起回到一页。
                // 基础页直排；「多出的页」独立子动画块——展开更多 = 滑开淡入、
                // 收起 = 内容保持挂载滑收淡出（计时器到期才回一页），收/放全程
                // 旧行被容器裁剪而不是瞬换
                let shown = self
                    .workspace_shown
                    .get(workspace)
                    .copied()
                    .unwrap_or(WORKSPACE_PAGE_SIZE);
                let total = sessions.len();
                let paginate_collapsing = self
                    .paginate_anims
                    .get(workspace)
                    .is_some_and(|a| a.collapsing);
                let mut block = v_flex().gap_1();
                for ix in sessions.iter().take(shown.min(WORKSPACE_PAGE_SIZE)) {
                    block = block.child(
                        div()
                            // 缩进 24px：会话文字与工作区名字对齐（行 mx+px 16 +
                            // 图标 16 + gap 8 = 40）
                            .pl_6()
                            .child(self.render_session_row(window, *ix, cx)),
                    );
                }
                if shown > WORKSPACE_PAGE_SIZE || paginate_collapsing {
                    let mut extra = v_flex().gap_1();
                    for ix in sessions
                        .iter()
                        .skip(WORKSPACE_PAGE_SIZE)
                        .take(shown.saturating_sub(WORKSPACE_PAGE_SIZE))
                    {
                        extra = extra
                            .child(div().pl_6().child(self.render_session_row(window, *ix, cx)));
                    }
                    match self.paginate_anims.get(workspace) {
                        Some(anim) => {
                            block = block.child(crate::anim::expand_anim_wrap(
                                format!("ws-page:{}:{}", workspace, anim.generation),
                                anim,
                                extra.into_any_element(),
                            ));
                        }
                        None => block = block.child(extra),
                    }
                }
                let can_more = shown < total;
                let can_collapse = shown > WORKSPACE_PAGE_SIZE;
                if can_more || can_collapse {
                    let mut controls = h_flex()
                        .mx_2()
                        .px_2()
                        .py_1()
                        .gap_3()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground);
                    if can_more {
                        let path = workspace.clone();
                        controls = controls.child(
                            h_flex()
                                .id(("workspace-more", p_ix))
                                .gap_1()
                                .px_1()
                                .rounded(cx.theme().radius)
                                .cursor_pointer()
                                .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    // 展开更多：多出的行进子动画块滑开
                                    *this
                                        .workspace_shown
                                        .entry(path.clone())
                                        .or_insert(WORKSPACE_PAGE_SIZE) += WORKSPACE_PAGE_SIZE;
                                    let anim = this.paginate_anims.entry(path.clone()).or_default();
                                    anim.generation += 1;
                                    anim.collapsing = false;
                                    cx.notify();
                                }))
                                .child(Icon::new(IconName::ChevronDown).size_3())
                                .child("展开更多")
                                .test_support(),
                        );
                    }
                    if can_more && can_collapse {
                        controls = controls.child("·");
                    }
                    if can_collapse {
                        let path = workspace.clone();
                        controls = controls.child(
                            h_flex()
                                .id(("workspace-collapse", p_ix))
                                .gap_1()
                                .px_1()
                                .rounded(cx.theme().radius)
                                .cursor_pointer()
                                .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    // 收起（分页）：多出的行保持挂载播滑收淡出，
                                    // 计时器到期才回一页（期间再展开代次不符作废）
                                    let anim = this.paginate_anims.entry(path.clone()).or_default();
                                    anim.generation += 1;
                                    anim.collapsing = true;
                                    let generation = anim.generation;
                                    let path = path.clone();
                                    cx.spawn(async move |this, cx| {
                                        cx.background_executor()
                                            .timer(
                                                EXPAND_ANIM_DUR
                                                    + std::time::Duration::from_millis(50),
                                            )
                                            .await;
                                        this.update(cx, |this, cx| {
                                            let stale =
                                                this.paginate_anims.get(&path).is_none_or(|a| {
                                                    !a.collapsing || a.generation != generation
                                                });
                                            if !stale {
                                                this.workspace_shown.remove(&path);
                                                if let Some(a) = this.paginate_anims.get_mut(&path)
                                                {
                                                    a.collapsing = false;
                                                }
                                                cx.notify();
                                            }
                                        })
                                        .ok();
                                    })
                                    .detach();
                                    cx.notify();
                                }))
                                .child(Icon::new(IconName::ChevronUp).size_3())
                                .child("收起")
                                .test_support(),
                        );
                    }
                    block = block.child(div().pl_6().child(controls));
                }
                match self.expand_anims.get(workspace) {
                    Some(anim) => out.push(
                        // 观测包装（test_support 无 feature 时透传，测试用它量容器实高）
                        div()
                            .id(("ws-block", p_ix))
                            .test_support()
                            .child(crate::anim::expand_anim_wrap(
                                format!("ws-expand:{}:{}", workspace, anim.generation),
                                anim,
                                block.into_any_element(),
                            ))
                            .into_any_element(),
                    ),
                    None => out.push(block.into_any_element()),
                }
            }
        }
        out
    }
}
