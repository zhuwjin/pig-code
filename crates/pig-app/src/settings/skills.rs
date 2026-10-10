use super::*;
use pig_utils::skills as skill_core;
use std::path::Path;

/// Display projection of one skill (core skills::Skill plus enable state plus
/// override flag)
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SkillInfo {
    /// Invocation name (frontmatter name, falling back to the directory name)
    pub name: String,
    /// Skill directory name (identity for file operations)
    pub dir_name: String,
    /// Absolute path of the skill directory
    pub directory: PathBuf,
    /// Absolute path of SKILL.md (the key of the enable state)
    pub path: PathBuf,
    pub description: String,
    pub when_to_use: Option<String>,
    pub source: skill_core::SkillSource,
    /// Enable state (skills-state.json; enabled by default)
    pub enabled: bool,
    /// This project-level skill overrides a user-level skill with the same name
    pub overrides_user: bool,
    /// Whether SKILL.md has frontmatter (prompted to add one when missing)
    pub has_frontmatter: bool,
}

/// The two skill roots plus the merged skill list (the settings skills page data
/// snapshot)
pub(crate) struct SkillsSnapshot {
    pub user_path: PathBuf,
    pub user_exists: bool,
    /// None = no workspace selected (project level does not participate)
    pub project_path: Option<PathBuf>,
    pub project_exists: bool,
    pub skills: Vec<SkillInfo>,
}

/// Read the user-level `<data_dir>/skills/` plus the project-level
/// `<workspace>/.pigcode/skills/` and merge (project-level same-name overrides);
/// the enable state comes from `<data_dir>/skills-state.json`
pub(crate) fn load_skills_snapshot(workspace: Option<&Path>) -> SkillsSnapshot {
    load_skills_from(&pig_utils::data_dir(), workspace)
}

/// Load with an explicit data_dir (for tests; same criteria as
/// load_skills_snapshot)
pub(crate) fn load_skills_from(data_dir: &Path, workspace: Option<&Path>) -> SkillsSnapshot {
    let user_path = skill_core::user_root(data_dir);
    let project_path = workspace.map(skill_core::project_root);
    let disabled = skill_core::load_disabled_map(data_dir);
    let project_skills = project_path
        .as_deref()
        .map(|root| discover_infos(root, skill_core::SkillSource::Project, &disabled))
        .unwrap_or_default();
    let mut skills = discover_infos(&user_path, skill_core::SkillSource::User, &disabled);
    // Merge criteria match core's discover: a project-level same-name skill
    // wholly replaces the user-level one
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

// ---------- Skill writes (create/edit/delete persist, then the whole page refreshes) ----------

/// Skill name → directory name: lowercase, whitespace/underscores collapsed to
/// `-`, only [a-z0-9-] kept, length 1-64
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

/// Create a skill: make the directory plus write SKILL.md, returning the
/// directory path. Errors when the directory already exists (no overwrite)
pub(crate) fn create_skill_dir(
    root: &Path,
    dir_name: &str,
    markdown: &str,
) -> Result<PathBuf, String> {
    let dir = root.join(dir_name);
    if dir.exists() {
        return Err(
            rust_i18n::t!("settings.skills.err_dir_exists", path = dir.display()).to_string(),
        );
    }
    std::fs::create_dir_all(&dir).map_err(|e| {
        rust_i18n::t!(
            "settings.skills.err_create_dir",
            path = dir.display(),
            error = e
        )
        .to_string()
    })?;
    std::fs::write(dir.join(skill_core::SKILL_FILE), markdown)
        .map_err(|e| rust_i18n::t!("settings.skills.err_write_skill", error = e).to_string())?;
    Ok(dir)
}

/// Edit save: rewrite the whole SKILL.md file
pub(crate) fn write_skill_md(directory: &Path, markdown: &str) -> Result<(), String> {
    std::fs::create_dir_all(directory).map_err(|e| {
        rust_i18n::t!(
            "settings.skills.err_create_dir",
            path = directory.display(),
            error = e
        )
        .to_string()
    })?;
    std::fs::write(directory.join(skill_core::SKILL_FILE), markdown)
        .map_err(|e| rust_i18n::t!("settings.skills.err_write_skill", error = e).to_string())
}

/// Delete a skill directory. Only direct children of the two controlled roots
/// may be deleted (the root itself/out-of-bounds paths are refused)
pub(crate) fn delete_skill_dir(directory: &Path, roots: &[&Path]) -> Result<(), String> {
    let inside = roots
        .iter()
        .any(|root| directory.parent().is_some_and(|parent| parent == *root) && directory != *root);
    if !inside {
        return Err(rust_i18n::t!(
            "settings.skills.err_delete_refused",
            path = directory.display()
        )
        .to_string());
    }
    std::fs::remove_dir_all(directory)
        .map_err(|e| rust_i18n::t!("settings.skills.err_delete_failed", error = e).to_string())
}

impl SettingsView {
    /// Refresh the skills page: on the event AppView re-reads the skill
    /// directories
    pub(crate) fn refresh_skills(&mut self, cx: &mut Context<Self>) {
        cx.emit(SettingsEvent::RefreshSkills);
        cx.notify();
    }

    /// Read by AppView when refreshing the snapshot: the currently selected scope
    pub(crate) fn skills_scope(&self) -> &McpScope {
        &self.skills_scope
    }

    /// Snapshot feed (called when AppView refreshes)
    pub(crate) fn set_skills_config(&mut self, snapshot: SkillsSnapshot, cx: &mut Context<Self>) {
        self.skills_snapshot = Some(snapshot);
        cx.notify();
    }

    /// Switch scope: refresh the whole page after changing the target (AppView
    /// re-reads the snapshot for the new scope)
    fn set_skills_scope(&mut self, scope: McpScope, cx: &mut Context<Self>) {
        self.skills_scope_popup = false;
        if self.skills_scope == scope {
            cx.notify();
            return;
        }
        self.skills_scope = scope;
        self.refresh_skills(cx);
    }

    /// Enable/disable toggle: write skills-state.json (global state, consistent
    /// across scopes), then refresh the whole page
    fn toggle_skill(&mut self, skill_path: &Path, enabled: bool, cx: &mut Context<Self>) {
        match skill_core::set_skill_enabled(&pig_utils::data_dir(), skill_path, enabled) {
            Ok(()) => {
                self.skills_write_error = None;
                self.refresh_skills(cx);
            }
            Err(e) => {
                // Structured CoreError → localized message (detail shown as a
                // note of the English original)
                self.skills_write_error = Some(crate::errors::core_error_text(&e));
                cx.notify();
            }
        }
    }

    /// Open the create (name=None)/edit dialog, prefilled from the existing skill
    pub(crate) fn open_skills_dialog(
        &mut self,
        name: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.skills_snapshot.as_ref();
        let existing =
            name.and_then(|n| snapshot.and_then(|s| s.skills.iter().find(|skill| skill.name == n)));
        let scope = existing
            .map(|skill| skill.source)
            .unwrap_or(skill_core::SkillSource::User);
        // Edit base: read and parse the existing SKILL.md (extra frontmatter
        // keys kept faithfully)
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
        // Workspace name for a project-level target (two levels up from the
        // project root = workspace; display name via the alias table)
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
                    .placeholder(rust_i18n::t!("settings.skills.name_placeholder"))
                    .default_value(existing.map(|skill| skill.name.clone()).unwrap_or_default())
            }),
            description: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(2, 5)
                    .default_value(description)
            }),
            when_to_use: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(rust_i18n::t!("settings.skills.when_to_use_placeholder"))
                    .default_value(when_to_use.unwrap_or_default())
            }),
            body: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(10, 24)
                    .default_value(if body.is_empty() {
                        rust_i18n::t!("settings.skills.default_body").to_string()
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

    /// Save the dialog: validate → generate SKILL.md → create a directory or
    /// rewrite in place → refresh
    fn save_skills_dialog(&mut self, cx: &mut Context<Self>) {
        let name = {
            let Some(dialog) = self.skills_dialog.as_ref() else {
                return;
            };
            dialog.name.read(cx).value().trim().to_string()
        };
        if name.is_empty() {
            self.set_skills_dialog_error(rust_i18n::t!("settings.skills.err_name_empty").as_ref());
            cx.notify();
            return;
        }
        if !name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
        {
            self.set_skills_dialog_error(
                rust_i18n::t!("settings.skills.err_name_charset").as_ref(),
            );
            cx.notify();
            return;
        }
        let Some(dialog) = self.skills_dialog.as_ref() else {
            return;
        };
        let description = dialog.description.read(cx).value().trim().to_string();
        let when_to_use = dialog.when_to_use.read(cx).value().trim().to_string();
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
            self.set_skills_dialog_error(rust_i18n::t!("settings.skills.err_not_loaded").as_ref());
            cx.notify();
            return;
        };
        let result = match &dialog.target_dir {
            // Editing: rewrite in the original directory (name locked,
            // directory untouched)
            Some(dir) => write_skill_md(dir, &markdown).map(|_| dir.clone()),
            // Creating: locate the root by scope; the directory name is the
            // normalized skill name
            None => {
                let root: Option<PathBuf> = match dialog.scope {
                    skill_core::SkillSource::User => Some(snapshot.user_path.clone()),
                    skill_core::SkillSource::Project => snapshot.project_path.clone(),
                };
                let Some(root) = root else {
                    self.set_skills_dialog_error(
                        rust_i18n::t!("settings.skills.err_project_unavailable").as_ref(),
                    );
                    cx.notify();
                    return;
                };
                let Some(dir_name) = sanitize_dir_name(&name) else {
                    self.set_skills_dialog_error(
                        rust_i18n::t!("settings.skills.err_dir_name").as_ref(),
                    );
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

    /// Delete the skill being edited (two-step confirm): only direct children
    /// of the controlled roots
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

    /// Switch scope (only selectable when creating; project level requires a
    /// selected workspace)
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

    /// Scope selector (pill button plus deferred popup, same as the MCP page)
    pub(crate) fn render_skills_scope(&self, cx: &mut Context<Self>) -> AnyElement {
        let (icon, label) = match &self.skills_scope {
            McpScope::User => (
                IconName::User,
                rust_i18n::t!("settings.common.scope_user").to_string(),
            ),
            McpScope::Workspace(path) => {
                let alias = self
                    .scope_workspaces
                    .iter()
                    .find(|(p, _)| p == path)
                    .map(|(_, name)| name.clone());
                (
                    IconName::Folder,
                    workspace_display_name(path, alias.as_deref()),
                )
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
                    .on_click(cx.listener(|this, event: &ClickEvent, _, cx| {
                        // Clicking the button while the popup is open:
                        // outside-close closes it first, and the same press's
                        // click is swallowed by position match
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
                    })),
            )
            .when(self.skills_scope_popup, |this| {
                this.child(self.render_skills_scope_popup(cx))
            })
            .into_any_element()
    }

    /// Scope dropdown: user level plus the workspace list (the session's
    /// workspace gets a "current session" badge)
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
                    .hover(|this| this.bg(cx.theme().accent).border_color(cx.theme().border))
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
                            .child(
                                div()
                                    .text_sm()
                                    .child(rust_i18n::t!("settings.common.scope_user").to_string()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .truncate()
                                    .child(
                                        rust_i18n::t!("settings.skills.scope_user_hint")
                                            .to_string(),
                                    ),
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
                    .child(rust_i18n::t!("settings.common.workspace_section").to_string()),
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
                        .hover(|this| this.bg(cx.theme().accent).border_color(cx.theme().border))
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
                                        .child(div().text_sm().truncate().child(display.clone()))
                                        .when(is_session_ws, |this| {
                                            this.child(
                                                div()
                                                    .text_xs()
                                                    .px_1()
                                                    .rounded_sm()
                                                    .bg(cx.theme().accent)
                                                    .flex_shrink_0()
                                                    .child(
                                                        rust_i18n::t!(
                                                            "settings.common.current_session"
                                                        )
                                                        .to_string(),
                                                    ),
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
                .child(
                    div()
                        .text_sm()
                        .child(rust_i18n::t!("settings.skills.loading").to_string()),
                )
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
        page = page.child(
            h_flex()
                .w_full()
                .gap_2()
                .items_center()
                .child(self.render_skills_scope(cx)),
        );
        if snapshot.skills.is_empty() {
            page = page.child(self.render_skills_empty(cx));
        } else if filtered.is_empty() {
            page = page.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .py_6()
                    .child(rust_i18n::t!("settings.skills.no_match").to_string()),
            );
        } else {
            page = page
                .child(
                    div()
                        .text_sm()
                        .font_semibold()
                        .child(if snapshot.skills.len() == 1 {
                            rust_i18n::t!("settings.skills.skill_count_one", n = 1).to_string()
                        } else {
                            rust_i18n::t!("settings.skills.skill_count", n = snapshot.skills.len())
                                .to_string()
                        }),
                )
                .children(
                    filtered
                        .iter()
                        .enumerate()
                        .map(|(ix, skill)| self.render_skill_card(ix, skill, cx))
                        .collect::<Vec<_>>(),
                );
        }
        page.into_any_element()
    }

    /// Help dialog: SKILL.md format plus skill directory paths plus when changes
    /// take effect (popped up by the header info button; when directories are
    /// not loaded the paths section is omitted and only the format notes are
    /// shown)
    pub(crate) fn render_skills_help_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .absolute()
            .inset_0()
            .bg(gpui_kit::black().opacity(0.5))
            .flex()
            .items_center()
            .justify_center()
            .child(
                v_flex()
                    .id("skills-help-dialog")
                    .w(px(560.))
                    .max_h(px(640.))
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
                            .child(rust_i18n::t!("settings.skills.help_title").to_string()),
                    )
                    .when_some(self.skills_snapshot.as_ref(), |this, snapshot| {
                        this.child(
                            v_flex()
                                .gap_1()
                                .child(
                                    div().text_sm().font_semibold().child(
                                        rust_i18n::t!("settings.skills.skill_dirs").to_string(),
                                    ),
                                )
                                .child(self.render_skill_sources(snapshot, cx)),
                        )
                    })
                    .child(
                        v_flex()
                            .gap_1()
                            .child(
                                div().text_sm().font_semibold().child(
                                    rust_i18n::t!("settings.skills.manual_edit").to_string(),
                                ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(
                                        rust_i18n::t!("settings.skills.manual_edit_hint")
                                            .to_string(),
                                    ),
                            )
                            .child(
                                div()
                                    .rounded_lg()
                                    .border_1()
                                    .border_color(cx.theme().border)
                                    .px_3()
                                    .py_2()
                                    .text_xs()
                                    .font_family(cx.theme().mono_font_family.clone())
                                    .text_color(cx.theme().muted_foreground)
                                    .child(rust_i18n::t!("settings.skills.example").to_string()),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(rust_i18n::t!("settings.skills.effective_hint").to_string()),
                    )
                    .child(
                        h_flex().gap_2().child(div().flex_1()).child(
                            Button::new("close-skills-help")
                                .primary()
                                .small()
                                .label(rust_i18n::t!("settings.common.close"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.skills_help_open = false;
                                    cx.notify();
                                })),
                        ),
                    ),
            )
            .into_any_element()
    }

    /// Onboarding when no skills exist: create entry point plus example SKILL.md
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
                    .child(rust_i18n::t!("settings.skills.empty_title").to_string()),
            )
            .child(
                Button::new("new-skill-empty")
                    .primary()
                    .icon(IconName::Plus)
                    .label(rust_i18n::t!("settings.skills.new_skill"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_skills_dialog(None, window, cx);
                    })),
            )
            .into_any_element()
    }

    /// Two directory source rows: user-level/project-level paths and whether
    /// they exist
    fn render_skill_sources(
        &self,
        snapshot: &SkillsSnapshot,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        v_flex()
            .gap_1()
            .child(skill_source_row(
                rust_i18n::t!("settings.common.scope_user").as_ref(),
                Some(&snapshot.user_path),
                snapshot.user_exists,
                cx,
            ))
            .child(match &snapshot.project_path {
                Some(path) => skill_source_row(
                    rust_i18n::t!("settings.common.scope_project").as_ref(),
                    Some(path),
                    snapshot.project_exists,
                    cx,
                ),
                None => skill_source_row(
                    rust_i18n::t!("settings.common.scope_project").as_ref(),
                    None,
                    false,
                    cx,
                ),
            })
            .into_any_element()
    }

    /// One skill card: status dot plus name plus source chip plus
    /// edit/enable-disable; second row the description, third row the directory
    /// (mono); a hint row when frontmatter is missing
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
            (
                cx.theme().success,
                rust_i18n::t!("settings.skills.state_enabled").to_string(),
            )
        } else {
            (
                cx.theme().muted_foreground,
                rust_i18n::t!("settings.skills.state_disabled").to_string(),
            )
        };
        let edit_name = skill.name.clone();
        let enabled = skill.enabled;
        let skill_path = skill.path.clone();
        let description = if skill.description.is_empty() {
            rust_i18n::t!("settings.skills.no_description").to_string()
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
                    .child(chip(&skill_source_label(skill.source), cx))
                    .when(skill.overrides_user, |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(rust_i18n::t!("settings.skills.overrides_user").to_string()),
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
                        .child(rust_i18n::t!("settings.skills.no_frontmatter").to_string()),
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
                            .child(
                                rust_i18n::t!("settings.skills.when_to_use", when = when)
                                    .to_string(),
                            ),
                    )
                },
            )
            .into_any_element()
    }

    /// Create/edit dialog (name/scope/description/when-to-use/body)
    pub(crate) fn render_skills_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(dialog) = &self.skills_dialog else {
            return div().into_any_element();
        };
        let editing = dialog.editing.is_some();

        let form = v_flex()
            .gap_3()
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(rust_i18n::t!("settings.common.field_name").to_string()),
                    )
                    .child(Input::new(&dialog.name).disabled(editing))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .opacity(0.7)
                            .child(rust_i18n::t!("settings.skills.name_hint").to_string()),
                    ),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(rust_i18n::t!("settings.common.scope").to_string()),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("skill-scope-user")
                                    .small()
                                    .when(dialog.scope == skill_core::SkillSource::User, |this| {
                                        this.primary()
                                    })
                                    .when(dialog.scope != skill_core::SkillSource::User, |this| {
                                        this.outline()
                                    })
                                    .disabled(editing)
                                    .label(rust_i18n::t!("settings.common.scope_user"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.set_skills_dialog_scope(
                                            skill_core::SkillSource::User,
                                            cx,
                                        );
                                    })),
                            )
                            .child(
                                Button::new("skill-scope-project")
                                    .small()
                                    .when(
                                        dialog.scope == skill_core::SkillSource::Project,
                                        |this| this.primary(),
                                    )
                                    .when(
                                        dialog.scope != skill_core::SkillSource::Project,
                                        |this| this.outline(),
                                    )
                                    .disabled(editing || !dialog.project_available)
                                    .label(match &dialog.project_workspace {
                                        Some(name) => rust_i18n::t!(
                                            "settings.common.scope_project_named",
                                            name = name
                                        )
                                        .to_string(),
                                        None => rust_i18n::t!("settings.common.scope_project")
                                            .to_string(),
                                    })
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.set_skills_dialog_scope(
                                            skill_core::SkillSource::Project,
                                            cx,
                                        );
                                    })),
                            ),
                    )
                    .when(!dialog.project_available, |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .opacity(0.7)
                                .child(
                                    rust_i18n::t!("settings.common.project_unavailable_hint")
                                        .to_string(),
                                ),
                        )
                    }),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(rust_i18n::t!("settings.skills.field_description").to_string()),
                    )
                    .child(Textarea::new(&dialog.description))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .opacity(0.7)
                            .child(rust_i18n::t!("settings.skills.description_hint").to_string()),
                    ),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(rust_i18n::t!("settings.skills.field_when_to_use").to_string()),
                    )
                    .child(Input::new(&dialog.when_to_use))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .opacity(0.7)
                            .child(rust_i18n::t!("settings.skills.when_to_use_hint").to_string()),
                    ),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(rust_i18n::t!("settings.skills.field_body").to_string()),
                    )
                    .child(Textarea::new(&dialog.body))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .opacity(0.7)
                            .child(rust_i18n::t!("settings.skills.body_hint").to_string()),
                    ),
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
                    .child(div().text_lg().font_semibold().child(if editing {
                        rust_i18n::t!(
                            "settings.skills.edit_title",
                            name = dialog.editing.clone().expect("editing")
                        )
                        .to_string()
                    } else {
                        rust_i18n::t!("settings.skills.new_skill").to_string()
                    }))
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
                                            rust_i18n::t!("settings.common.confirm_delete")
                                        } else {
                                            rust_i18n::t!("common.delete")
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
                                    .label(rust_i18n::t!("common.cancel"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.skills_dialog = None;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("skills-dialog-save")
                                    .primary()
                                    .small()
                                    .label(rust_i18n::t!("settings.common.save"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.save_skills_dialog(cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }
}

/// Skill source chip label. core's SkillSource::label is a Chinese constant
/// (pig-core does not use the GUI registry), so GUI text uniformly comes from
/// the local registry
fn skill_source_label(source: skill_core::SkillSource) -> String {
    match source {
        skill_core::SkillSource::User => rust_i18n::t!("settings.common.scope_user").to_string(),
        skill_core::SkillSource::Project => {
            rust_i18n::t!("settings.common.scope_project").to_string()
        }
    }
}

/// One directory source row: level label plus directory path plus a "not
/// created" note
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
                    None => rust_i18n::t!("settings.skills.source_pick_workspace").to_string(),
                }),
        )
        .when(path.is_some() && !exists, |this| {
            this.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(rust_i18n::t!("settings.common.not_created").to_string()),
            )
        })
}

#[cfg(test)]
mod tests;
