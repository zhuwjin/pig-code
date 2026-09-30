use super::*;
use pig_core::skills as skill_core;
use std::path::Path;

/// 空技能列表时的引导示例（标准 SKILL.md 形态）
const SKILL_EXAMPLE: &str = r#"---
name: commit-helper
description: 按仓库惯例生成提交信息并提交
when_to_use: 用户要求提交代码时
---

# 提交助手

1. 用 git diff 了解本次改动
2. 按仓库现有提交风格写提交信息
3. 用户确认后提交
"#;

/// 一个技能的展示投影（core skills::Skill + 启停状态 + 覆盖标记）
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SkillInfo {
    /// 调用名（frontmatter name，缺省回退目录名）
    pub name: String,
    /// 技能目录名（文件操作身份）
    pub dir_name: String,
    /// 技能目录绝对路径
    pub directory: PathBuf,
    /// SKILL.md 绝对路径（启停状态的 key）
    pub path: PathBuf,
    pub description: String,
    pub when_to_use: Option<String>,
    pub source: skill_core::SkillSource,
    /// 启停状态（skills-state.json；默认启用）
    pub enabled: bool,
    /// 项目级技能覆盖了用户级同名技能
    pub overrides_user: bool,
    /// SKILL.md 是否带 frontmatter（无则提示补写）
    pub has_frontmatter: bool,
}

/// 两个技能根目录 + 合并后的技能清单（设置页技能页数据快照）
pub(crate) struct SkillsSnapshot {
    pub user_path: PathBuf,
    pub user_exists: bool,
    /// None = 未选择工作区（项目级不参与）
    pub project_path: Option<PathBuf>,
    pub project_exists: bool,
    pub skills: Vec<SkillInfo>,
}

/// 读用户级 `<data_dir>/skills/` + 项目级 `<workspace>/.pigcode/skills/`
/// 并合并（项目级同名覆盖），启停状态取 `<data_dir>/skills-state.json`
pub(crate) fn load_skills_snapshot(workspace: Option<&Path>) -> SkillsSnapshot {
    load_skills_from(&pig_core::data_dir(), workspace)
}

/// 按显式 data_dir 加载（测试用；口径与 load_skills_snapshot 一致）
pub(crate) fn load_skills_from(data_dir: &Path, workspace: Option<&Path>) -> SkillsSnapshot {
    let user_path = skill_core::user_root(data_dir);
    let project_path = workspace.map(skill_core::project_root);
    let disabled = skill_core::load_disabled_map(data_dir);
    let project_skills = project_path
        .as_deref()
        .map(|root| discover_infos(root, skill_core::SkillSource::Project, &disabled))
        .unwrap_or_default();
    let mut skills = discover_infos(&user_path, skill_core::SkillSource::User, &disabled);
    // 合并口径与 core discover 一致：项目级同名整体覆盖用户级
    for mut skill in project_skills {
        if let Some(existing) = skills.iter_mut().find(|s| s.name == skill.name) {
            skill.overrides_user = true;
            *existing = skill;
        } else {
            skills.push(skill);
        }
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    SkillsSnapshot {
        user_exists: user_path.is_dir(),
        user_path,
        project_exists: project_path.as_ref().is_some_and(|p| p.is_dir()),
        project_path,
        skills,
    }
}

fn discover_infos(
    root: &Path,
    source: skill_core::SkillSource,
    disabled: &std::collections::HashSet<String>,
) -> Vec<SkillInfo> {
    skill_core::discover_root(root, source)
        .into_iter()
        .map(|skill| {
            let key = skill.path.to_string_lossy().replace('\\', "/");
            SkillInfo {
                name: skill.name,
                dir_name: skill
                    .directory
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                directory: skill.directory,
                path: skill.path,
                description: skill.description,
                when_to_use: skill.when_to_use,
                source: skill.source,
                enabled: !disabled.contains(&key),
                overrides_user: false,
                has_frontmatter: skill.has_frontmatter,
            }
        })
        .collect()
}

// ---------- 技能写入（新建/编辑/删除落盘，之后整页刷新） ----------

/// 技能名 → 目录名：小写、空白/下划线归并为 `-`，只留 [a-z0-9-]，长度 1-64
pub(crate) fn sanitize_dir_name(name: &str) -> Option<String> {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in name.trim().chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_lowercase() || lower.is_ascii_digit() {
            out.push(lower);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    (!trimmed.is_empty() && trimmed.len() <= 64).then_some(trimmed)
}

/// 新建技能：建目录 + 写 SKILL.md，返回目录路径。目录已存在时报错（不覆盖）
pub(crate) fn create_skill_dir(root: &Path, dir_name: &str, markdown: &str) -> Result<PathBuf, String> {
    let dir = root.join(dir_name);
    if dir.exists() {
        return Err(format!("目录已存在: {}", dir.display()));
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建目录失败 {}: {e}", dir.display()))?;
    std::fs::write(dir.join(skill_core::SKILL_FILE), markdown)
        .map_err(|e| format!("写入 SKILL.md 失败: {e}"))?;
    Ok(dir)
}

/// 编辑保存：整文件重写 SKILL.md
pub(crate) fn write_skill_md(directory: &Path, markdown: &str) -> Result<(), String> {
    std::fs::create_dir_all(directory)
        .map_err(|e| format!("创建目录失败 {}: {e}", directory.display()))?;
    std::fs::write(directory.join(skill_core::SKILL_FILE), markdown)
        .map_err(|e| format!("写入 SKILL.md 失败: {e}"))
}

/// 删除技能目录。仅允许删两个受控根的直接子目录（root 自身/越界路径拒绝）
pub(crate) fn delete_skill_dir(
    directory: &Path,
    roots: &[&Path],
) -> Result<(), String> {
    let inside = roots.iter().any(|root| {
        directory.parent().is_some_and(|parent| parent == *root) && directory != *root
    });
    if !inside {
        return Err(format!(
            "拒绝删除 {}: 不在受控的技能目录（skills/ 的直接子目录）内",
            directory.display()
        ));
    }
    std::fs::remove_dir_all(directory).map_err(|e| format!("删除失败: {e}"))
}

impl SettingsView {
    /// 刷新技能页：AppView 收到事件后重读技能目录
    pub(crate) fn refresh_skills(&mut self, cx: &mut Context<Self>) {
        cx.emit(SettingsEvent::RefreshSkills);
        cx.notify();
    }

    /// AppView 刷新快照时读取：当前选中的作用域
    pub(crate) fn skills_scope(&self) -> &McpScope {
        &self.skills_scope
    }

    /// 快照喂入（AppView 刷新时调用）
    pub(crate) fn set_skills_config(
        &mut self,
        snapshot: SkillsSnapshot,
        cx: &mut Context<Self>,
    ) {
        self.skills_snapshot = Some(snapshot);
        cx.notify();
    }

    /// 切换作用域：换目标后整页刷新（AppView 按新作用域重读快照）
    fn set_skills_scope(&mut self, scope: McpScope, cx: &mut Context<Self>) {
        self.skills_scope_popup = false;
        if self.skills_scope == scope {
            cx.notify();
            return;
        }
        self.skills_scope = scope;
        self.refresh_skills(cx);
    }

    /// 启停开关：写 skills-state.json（全局状态，任意作用域一致），然后整页刷新
    fn toggle_skill(&mut self, skill_path: &Path, enabled: bool, cx: &mut Context<Self>) {
        match skill_core::set_skill_enabled(&pig_core::data_dir(), skill_path, enabled) {
            Ok(()) => {
                self.skills_write_error = None;
                self.refresh_skills(cx);
            }
            Err(e) => {
                self.skills_write_error = Some(e);
                cx.notify();
            }
        }
    }

    /// 打开新建（name=None）/编辑对话框，按现有技能预填
    pub(crate) fn open_skills_dialog(
        &mut self,
        name: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.skills_snapshot.as_ref();
        let existing = name.and_then(|n| {
            snapshot.and_then(|s| s.skills.iter().find(|skill| skill.name == n))
        });
        let scope = existing
            .map(|skill| skill.source)
            .unwrap_or(skill_core::SkillSource::User);
        // 编辑底稿：读现有 SKILL.md 解析（保真额外 frontmatter 键）
        let (description, when_to_use, body, extra) = match existing {
            Some(skill) => match std::fs::read_to_string(&skill.path) {
                Ok(content) => {
                    let parsed = skill_core::parse_skill_markdown(&content);
                    let fm = parsed.frontmatter.unwrap_or_default();
                    (
                        fm.description.unwrap_or_default(),
                        fm.when_to_use,
                        parsed.body,
                        fm.extra,
                    )
                }
                Err(_) => (String::new(), None, String::new(), vec![]),
            },
            None => (String::new(), None, String::new(), vec![]),
        };
        // 项目级目标的工作区名（项目根上溯两级 = 工作区；显示名走别名表）
        let project_root = snapshot
            .and_then(|s| s.project_path.as_ref())
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
            .map(Path::to_path_buf);
        let project_workspace = project_root.map(|root| {
            let alias = self
                .scope_workspaces
                .iter()
                .find(|(path, _)| *path == root)
                .map(|(_, name)| name.clone());
            workspace_display_name(&root, alias.as_deref())
        });
        let dialog = SkillDialog {
            editing: existing.map(|skill| skill.dir_name.clone()),
            target_dir: existing.map(|skill| skill.directory.clone()),
            scope,
            project_available: snapshot.is_some_and(|s| s.project_path.is_some()),
            project_workspace,
            name: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("例如 commit-helper")
                    .default_value(existing.map(|skill| skill.name.clone()).unwrap_or_default())
            }),
            description: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(2, 5)
                    .default_value(description)
            }),
            when_to_use: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("可选：什么场景下用这个技能")
                    .default_value(when_to_use.unwrap_or_default())
            }),
            body: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(10, 24)
                    .default_value(if body.is_empty() {
                        "# 用途\n\n（这个技能做什么、怎么用）\n".to_string()
                    } else {
                        body
                    })
            }),
            extra_frontmatter: extra,
            delete_armed: false,
            error: None,
        };
        self.skills_dialog = Some(dialog);
        cx.notify();
    }

    /// 保存对话框：校验 → 生成 SKILL.md → 新建目录或原目录重写 → 刷新
    fn save_skills_dialog(&mut self, cx: &mut Context<Self>) {
        let name = {
            let Some(dialog) = self.skills_dialog.as_ref() else {
                return;
            };
            dialog.name.read(cx).value().trim().to_string()
        };
        if name.is_empty() {
            self.set_skills_dialog_error("技能名不能为空");
            cx.notify();
            return;
        }
        if !name.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_') {
            self.set_skills_dialog_error("技能名只允许字母、数字、- 和 _（将作为目录名与调用名）");
            cx.notify();
            return;
        }
        let Some(dialog) = self.skills_dialog.as_ref() else {
            return;
        };
        let description = dialog.description.read(cx).value().trim().to_string();
        let when_to_use = dialog
            .when_to_use
            .read(cx)
            .value()
            .trim()
            .to_string();
        let when_to_use = (!when_to_use.is_empty()).then_some(when_to_use);
        let body = dialog.body.read(cx).value().to_string();
        let markdown = skill_core::render_skill_markdown(
            &name,
            &description,
            when_to_use.as_deref(),
            &dialog.extra_frontmatter,
            &body,
        );
        let Some(snapshot) = self.skills_snapshot.as_ref() else {
            self.set_skills_dialog_error("技能目录尚未加载完成，请稍后重试");
            cx.notify();
            return;
        };
        let result = match &dialog.target_dir {
            // 编辑：原目录重写（名称锁定，目录不动）
            Some(dir) => write_skill_md(dir, &markdown).map(|_| dir.clone()),
            // 新建：按作用域定位根目录，目录名取技能名归一
            None => {
                let root: Option<PathBuf> = match dialog.scope {
                    skill_core::SkillSource::User => Some(snapshot.user_path.clone()),
                    skill_core::SkillSource::Project => snapshot.project_path.clone(),
                };
                let Some(root) = root else {
                    self.set_skills_dialog_error("项目级路径不可用（未选择工作区），请改用用户级");
                    cx.notify();
                    return;
                };
                let Some(dir_name) = sanitize_dir_name(&name) else {
                    self.set_skills_dialog_error("无法从技能名生成合法目录名（仅字母/数字/-）");
                    cx.notify();
                    return;
                };
                create_skill_dir(&root, &dir_name, &markdown)
            }
        };
        match result {
            Ok(_) => {
                self.skills_dialog = None;
                self.skills_write_error = None;
                self.refresh_skills(cx);
            }
            Err(e) => {
                self.skills_write_error = Some(e.clone());
                self.set_skills_dialog_error(&e);
                cx.notify();
            }
        }
    }

    fn set_skills_dialog_error(&mut self, error: &str) {
        if let Some(dialog) = self.skills_dialog.as_mut() {
            dialog.error = Some(error.to_string());
        }
    }

    /// 删除正在编辑的技能（两步确认）：仅受控根直接子目录
    fn delete_skills_dialog_skill(&mut self, cx: &mut Context<Self>) {
        let (armed, target) = {
            let Some(dialog) = &self.skills_dialog else {
                return;
            };
            (dialog.delete_armed, dialog.target_dir.clone())
        };
        if !armed {
            if let Some(dialog) = self.skills_dialog.as_mut() {
                dialog.delete_armed = true;
                dialog.error = None;
            }
            cx.notify();
            return;
        }
        let Some(target) = target else {
            return;
        };
        let Some(snapshot) = self.skills_snapshot.as_ref() else {
            return;
        };
        let roots: Vec<&Path> = snapshot
            .project_path
            .as_deref()
            .into_iter()
            .chain(std::iter::once(snapshot.user_path.as_path()))
            .collect();
        match delete_skill_dir(&target, &roots) {
            Ok(()) => {
                self.skills_dialog = None;
                self.skills_write_error = None;
                self.refresh_skills(cx);
            }
            Err(e) => {
                if let Some(dialog) = self.skills_dialog.as_mut() {
                    dialog.delete_armed = false;
                    dialog.error = Some(e.clone());
                }
                self.skills_write_error = Some(e);
                cx.notify();
            }
        }
    }

    /// 切换作用域（仅新建时可选；项目级要求已选工作区）
    fn set_skills_dialog_scope(&mut self, scope: skill_core::SkillSource, cx: &mut Context<Self>) {
        let Some(dialog) = self.skills_dialog.as_mut() else {
            return;
        };
        if dialog.editing.is_some() {
            return;
        }
        if scope == skill_core::SkillSource::Project && !dialog.project_available {
            return;
        }
        dialog.scope = scope;
        cx.notify();
    }

    /// 作用域选择器（pill 按钮 + deferred 弹层，MCP 页同款）
    pub(crate) fn render_skills_scope(&self, cx: &mut Context<Self>) -> AnyElement {
        let (icon, label) = match &self.skills_scope {
            McpScope::User => (IconName::User, "用户级".to_string()),
            McpScope::Workspace(path) => {
                let alias = self
                    .scope_workspaces
                    .iter()
                    .find(|(p, _)| p == path)
                    .map(|(_, name)| name.clone());
                (IconName::Folder, workspace_display_name(path, alias.as_deref()))
            }
        };
        div()
            .on_prepaint({
                let cell = self.skills_scope_btn_bounds.clone();
                move |bounds, _, _| cell.set(bounds)
            })
            .child(
                Button::new("skills-scope")
                    .outline()
                    .small()
                    .icon(icon)
                    .label(label)
                    .on_click(cx.listener(
                        |this, event: &ClickEvent, _, cx| {
                            // 弹层打开时点按钮：outside-close 先关掉，同按压的 click 按位置吞掉
                            let down_pos = match event {
                                ClickEvent::Mouse(e) => Some(e.down.position),
                                _ => None,
                            };
                            if this
                                .skills_scope_outside_close
                                .take()
                                .is_some_and(|pos| Some(pos) == down_pos)
                            {
                                return;
                            }
                            this.skills_scope_popup = !this.skills_scope_popup;
                            cx.notify();
                        },
                    )),
            )
            .when(self.skills_scope_popup, |this| {
                this.child(self.render_skills_scope_popup(cx))
            })
            .into_any_element()
    }

    /// 作用域下拉：用户级 + 工作区清单（会话所在工作区带「当前会话」标记）
    fn render_skills_scope_popup(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut popup = v_flex()
            .id("skills-scope-popup")
            .w(px(380.))
            .max_h(px(320.))
            .overflow_y_scroll()
            .py_1()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().popover)
            .shadow_lg()
            .on_mouse_down_out(cx.listener(|this, event: &MouseDownEvent, _, cx| {
                this.skills_scope_popup = false;
                this.skills_scope_outside_close = Some(event.position);
                cx.notify();
            }))
            .child(
                h_flex()
                    .id("skills-scope-user")
                    .gap_2()
                    .mx_1()
                    .px_2()
                    .py_1()
                    .rounded(cx.theme().radius)
                    .border_1()
                    .border_color(gpui_kit::black().opacity(0.))
                    .cursor_pointer()
                    .hover(|this| {
                        this.bg(cx.theme().accent)
                            .border_color(cx.theme().border)
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.set_skills_scope(McpScope::User, cx);
                    }))
                    .child(
                        Icon::new(IconName::User)
                            .size_4()
                            .text_color(cx.theme().muted_foreground),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(div().text_sm().child("用户级"))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .truncate()
                                    .child("全局技能，对所有工作区生效"),
                            ),
                    )
                    .when(self.skills_scope == McpScope::User, |this| {
                        this.child(
                            Icon::new(IconName::Check)
                                .size_4()
                                .flex_shrink_0()
                                .text_color(cx.theme().primary),
                        )
                    }),
            );
        if !self.scope_workspaces.is_empty() {
            popup = popup.child(
                div()
                    .px_3()
                    .py_1()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("工作区"),
            );
            for (path, display) in self.scope_workspaces.clone() {
                let selected = self.skills_scope == McpScope::Workspace(path.clone());
                let is_session_ws = self.session_cwd.as_deref() == Some(path.as_path());
                let path_text = path.display().to_string();
                popup = popup.child(
                    h_flex()
                        .id(gpui_kit::SharedString::from(path_text.clone()))
                        .gap_2()
                        .mx_1()
                        .px_2()
                        .py_1()
                        .rounded(cx.theme().radius)
                        .border_1()
                        .border_color(gpui_kit::black().opacity(0.))
                        .cursor_pointer()
                        .hover(|this| {
                            this.bg(cx.theme().accent)
                                .border_color(cx.theme().border)
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_skills_scope(McpScope::Workspace(path.clone()), cx);
                        }))
                        .child(
                            Icon::new(IconName::Folder)
                                .size_4()
                                .text_color(cx.theme().muted_foreground),
                        )
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap_0p5()
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .items_center()
                                        .min_w_0()
                                        .child(
                                            div()
                                                .text_sm()
                                                .truncate()
                                                .child(display.clone()),
                                        )
                                        .when(is_session_ws, |this| {
                                            this.child(
                                                div()
                                                    .text_xs()
                                                    .px_1()
                                                    .rounded_sm()
                                                    .bg(cx.theme().accent)
                                                    .flex_shrink_0()
                                                    .child("当前会话"),
                                            )
                                        }),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .truncate()
                                        .child(path_text),
                                ),
                        )
                        .when(selected, |this| {
                            this.child(
                                Icon::new(IconName::Check)
                                    .size_4()
                                    .flex_shrink_0()
                                    .text_color(cx.theme().primary),
                            )
                        }),
                );
            }
        }
        deferred(
            Positioner::side(self.skills_scope_btn_bounds.get())
                .placement(Placement::Bottom)
                .align(Align::Start)
                .offset(px(4.))
                .margin(px(8.))
                .occlude()
                .child(popup),
        )
        .with_priority(1)
        .into_any_element()
    }

    pub(crate) fn render_skills(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(snapshot) = &self.skills_snapshot else {
            return h_flex()
                .w_full()
                .items_center()
                .justify_center()
                .gap_2()
                .py_8()
                .text_color(cx.theme().muted_foreground)
                .child(Spinner::new().small())
                .child(div().text_sm().child("正在读取技能目录…"))
                .into_any_element();
        };

        let query = self.skills_search.read(cx).value().trim().to_lowercase();
        let filtered: Vec<&SkillInfo> = snapshot
            .skills
            .iter()
            .filter(|skill| {
                query.is_empty()
                    || skill.name.to_lowercase().contains(&query)
                    || skill.description.to_lowercase().contains(&query)
            })
            .collect();

        let mut page = v_flex().gap_4();
        if let Some(error) = &self.skills_write_error {
            page = page.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().danger)
                    .child(error.clone()),
            );
        }
        page = page.child(h_flex().w_full().gap_2().items_center().child(
            self.render_skills_scope(cx),
        ));
        if snapshot.skills.is_empty() {
            page = page.child(self.render_skills_empty(cx));
        } else if filtered.is_empty() {
            page = page.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .py_6()
                    .child("没有匹配的技能"),
            );
        } else {
            page = page
                .child(
                    div()
                        .text_sm()
                        .font_semibold()
                        .child(format!("技能（{}）", snapshot.skills.len())),
                )
                .children(
                    filtered
                        .iter()
                        .enumerate()
                        .map(|(ix, skill)| self.render_skill_card(ix, skill, cx))
                        .collect::<Vec<_>>(),
                );
        }
        // 目录来源挪到列表下方（路径作次要信息）
        page = page.child(
            v_flex().gap_1().child(
                div()
                    .text_sm()
                    .font_semibold()
                    .child("技能目录"),
            )
            .child(self.render_skill_sources(snapshot, cx)),
        );
        page.into_any_element()
    }

    /// 无技能时的引导：新建入口 + 示例 SKILL.md
    fn render_skills_empty(&self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .gap_3()
            .items_center()
            .py_6()
            .child(
                Icon::new(IconName::BookOpen)
                    .size_8()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("当前作用域还没有技能"),
            )
            .child(
                Button::new("new-skill-empty")
                    .primary()
                    .icon(IconName::Plus)
                    .label("新建技能")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_skills_dialog(None, window, cx);
                    })),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("每个技能是一个目录，内含 SKILL.md（frontmatter 写 name/description）；项目级覆盖用户级同名技能："),
            )
            .child(
                div()
                    .rounded_lg()
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().group_box)
                    .px_3()
                    .py_2()
                    .text_xs()
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_color(cx.theme().muted_foreground)
                    .child(SKILL_EXAMPLE),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("技能清单注入系统提示词（仅名称+描述，会话开始时冻结），正文由 Skill 工具按需加载；改动对新建会话生效。"),
            )
            .into_any_element()
    }

    /// 目录来源两行：用户级/项目级路径与是否存在
    fn render_skill_sources(
        &self,
        snapshot: &SkillsSnapshot,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        v_flex()
            .gap_1()
            .child(skill_source_row(
                "用户级",
                Some(&snapshot.user_path),
                snapshot.user_exists,
                cx,
            ))
            .child(match &snapshot.project_path {
                Some(path) => skill_source_row("项目级", Some(path), snapshot.project_exists, cx),
                None => skill_source_row("项目级", None, false, cx),
            })
            .into_any_element()
    }

    /// 单个技能卡片：状态点 + 名称 + 来源 chip + 编辑/启停，
    /// 第二行描述，第三行目录（mono），无 frontmatter 时给提示行
    fn render_skill_card(
        &self,
        ix: usize,
        skill: &SkillInfo,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let chip = |label: &str, cx: &mut Context<SettingsView>| {
            div()
                .text_xs()
                .px_1()
                .rounded_sm()
                .bg(cx.theme().accent)
                .child(label.to_string())
        };
        let (dot, status_text) = if skill.enabled {
            (cx.theme().success, "已启用")
        } else {
            (cx.theme().muted_foreground, "已停用")
        };
        let edit_name = skill.name.clone();
        let enabled = skill.enabled;
        let skill_path = skill.path.clone();
        let description = if skill.description.is_empty() {
            "（无描述——建议在编辑里补充，清单里只显示名称）".to_string()
        } else {
            skill.description.clone()
        };

        v_flex()
            .w_full()
            .gap_1()
            .px_3()
            .py_2()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().size_2().rounded_full().bg(dot))
                    .child(div().text_sm().font_semibold().child(skill.name.clone()))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(status_text),
                    )
                    .child(chip(skill.source.label(), cx))
                    .when(skill.overrides_user, |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("覆盖用户级同名技能"),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        Button::new(("skill-edit", ix))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Settings2)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_skills_dialog(Some(&edit_name), window, cx);
                            })),
                    )
                    .child(
                        Switch::new(("skill-enabled", ix))
                            .checked(enabled)
                            .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                this.toggle_skill(&skill_path, *checked, cx);
                            })),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(description),
            )
            .child(
                div()
                    .text_xs()
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_color(cx.theme().muted_foreground)
                    .child(skill.directory.display().to_string()),
            )
            .when(!skill.has_frontmatter, |this| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .opacity(0.8)
                        .child("SKILL.md 无 frontmatter：name 取目录名、description 为空，建议编辑补写"),
                )
            })
            .when_some(
                skill.when_to_use.clone().filter(|w| !w.trim().is_empty()),
                |this, when| {
                    this.child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .opacity(0.8)
                            .child(format!("适用场景：{when}")),
                    )
                },
            )
            .into_any_element()
    }

    /// 新建/编辑对话框（名称/作用域/描述/适用场景/正文）
    pub(crate) fn render_skills_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(dialog) = &self.skills_dialog else {
            return div().into_any_element();
        };
        let editing = dialog.editing.is_some();

        let form = v_flex()
            .gap_3()
            .child(
                v_flex().gap_1()
                    .child(div().text_xs().text_color(cx.theme().muted_foreground).child("名称"))
                    .child(Input::new(&dialog.name).disabled(editing))
                    .child(div().text_xs().text_color(cx.theme().muted_foreground).opacity(0.7)
                        .child("模型经 Skill 工具按名调用；编辑时不可改名（删除后重建），目录名取名称归一小写")),
            )
            .child(
                v_flex().gap_1()
                    .child(div().text_xs().text_color(cx.theme().muted_foreground).child("作用域"))
                    .child(
                        h_flex().gap_1()
                            .child(
                                Button::new("skill-scope-user")
                                    .small()
                                    .when(dialog.scope == skill_core::SkillSource::User, |this| this.primary())
                                    .when(dialog.scope != skill_core::SkillSource::User, |this| this.outline())
                                    .disabled(editing)
                                    .label("用户级")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.set_skills_dialog_scope(skill_core::SkillSource::User, cx);
                                    })),
                            )
                            .child(
                                Button::new("skill-scope-project")
                                    .small()
                                    .when(dialog.scope == skill_core::SkillSource::Project, |this| this.primary())
                                    .when(dialog.scope != skill_core::SkillSource::Project, |this| this.outline())
                                    .disabled(editing || !dialog.project_available)
                                    .label(match &dialog.project_workspace {
                                        Some(name) => format!("项目级（{name}）"),
                                        None => "项目级".to_string(),
                                    })
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.set_skills_dialog_scope(skill_core::SkillSource::Project, cx);
                                    })),
                            ),
                    )
                    .when(!dialog.project_available, |this| {
                        this.child(div().text_xs().text_color(cx.theme().muted_foreground).opacity(0.7)
                            .child("用户级视图只写用户级；在列表上方切换到具体工作区后可写项目级"))
                    }),
            )
            .child(
                v_flex().gap_1()
                    .child(div().text_xs().text_color(cx.theme().muted_foreground).child("描述"))
                    .child(Textarea::new(&dialog.description))
                    .child(div().text_xs().text_color(cx.theme().muted_foreground).opacity(0.7)
                        .child("写进系统提示词的技能清单，说明这个技能做什么；建议一句话+适用边界")),
            )
            .child(
                v_flex().gap_1()
                    .child(div().text_xs().text_color(cx.theme().muted_foreground).child("适用场景（可选）"))
                    .child(Input::new(&dialog.when_to_use))
                    .child(div().text_xs().text_color(cx.theme().muted_foreground).opacity(0.7)
                        .child("when_to_use：什么情况下应该用这个技能，拼在清单描述后面")),
            )
            .child(
                v_flex().gap_1()
                    .child(div().text_xs().text_color(cx.theme().muted_foreground).child("正文（SKILL.md body）"))
                    .child(Textarea::new(&dialog.body))
                    .child(div().text_xs().text_color(cx.theme().muted_foreground).opacity(0.7)
                        .child("技能的完整说明：步骤、约束、示例；相对路径相对技能目录，可用 ${SKILL_DIR} 占位")),
            );

        div()
            .absolute()
            .inset_0()
            .bg(gpui_kit::black().opacity(0.5))
            .flex()
            .items_center()
            .justify_center()
            .child(
                v_flex()
                    .id("skills-dialog")
                    .w(px(640.))
                    .max_h(px(680.))
                    .overflow_y_scroll()
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
                            .child(if editing {
                                format!(
                                    "编辑技能「{}」",
                                    dialog.editing.clone().expect("editing")
                                )
                            } else {
                                "新建技能".to_string()
                            }),
                    )
                    .child(form)
                    .when_some(dialog.error.clone(), |this, error| {
                        this.child(div().text_xs().text_color(cx.theme().danger).child(error))
                    })
                    .child(
                        h_flex()
                            .gap_2()
                            .mt_1()
                            .when(editing, |this| {
                                this.child(
                                    Button::new("skills-dialog-delete")
                                        .ghost()
                                        .small()
                                        .label(if dialog.delete_armed {
                                            "确认删除？"
                                        } else {
                                            "删除"
                                        })
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.delete_skills_dialog_skill(cx);
                                        })),
                                )
                            })
                            .child(div().flex_1())
                            .child(
                                Button::new("skills-dialog-cancel")
                                    .outline()
                                    .small()
                                    .label("取消")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.skills_dialog = None;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("skills-dialog-save")
                                    .primary()
                                    .small()
                                    .label("保存")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.save_skills_dialog(cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }
}

/// 目录来源行：级别标签 + 目录路径 + 「未创建」标注
fn skill_source_row(
    label: &str,
    path: Option<&Path>,
    exists: bool,
    cx: &mut Context<SettingsView>,
) -> Div {
    h_flex()
        .gap_2()
        .items_center()
        .child(
            div()
                .w(px(48.))
                .flex_shrink_0()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(label.to_string()),
        )
        .child(
            div()
                .flex_1()
                .text_xs()
                .font_family(cx.theme().mono_font_family.clone())
                .child(match path {
                    Some(path) => path.display().to_string(),
                    None => "选择工作区后显示其项目级技能目录".to_string(),
                }),
        )
        .when(path.is_some() && !exists, |this| {
            this.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("（未创建）"),
            )
        })
}

#[cfg(test)]
mod tests;
