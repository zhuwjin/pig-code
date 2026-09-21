mod agent_client;
mod settings;
mod composer;
mod review_panel;
mod sidebar;
mod thread_view;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::resizable::{h_resizable, resizable_panel};
use gpui_kit::component::{
    ActiveTheme as _, IconName, Root, Sizable as _, StyledExt as _, Theme, ThemeMode, TitleBar,
    h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::InteractiveElement as _;
use gpui_kit::*;
use pig_protocol::{Event, ExecMode, SessionMeta};

gpui_kit::actions!(pig_app, [NewTask, FocusSearch, CloseSearch, CloseSettings, ToggleSidebar]);

/// 主题是否跟随系统外观：默认跟随；手动切换亮/暗后本次运行内固定为所选模式。
pub struct ThemeFollowSystem(pub bool);

impl Global for ThemeFollowSystem {}

/// 相对时间显示（刚刚 / N分钟 / N小时 / N天）
pub trait RelativeTime {
    fn relative(&self) -> String;
}

impl RelativeTime for u64 {
    fn relative(&self) -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let diff = now.saturating_sub(*self);
        match diff {
            0..=59 => "刚刚".to_string(),
            60..=3599 => format!("{} 分钟", diff / 60),
            3600..=86399 => format!("{} 小时", diff / 3600),
            _ => format!("{} 天", diff / 86400),
        }
    }
}

use crate::agent_client::AgentClient;
use crate::composer::{Composer, ComposerEvent};
use crate::review_panel::{ReviewEvent, ReviewPanel};
use crate::settings::{SettingsEvent, SettingsView};
use crate::sidebar::{Sidebar, SidebarEvent, SidebarSession};
use crate::thread_view::{ThreadEvent, ThreadView};

struct SessionViews {
    thread: Entity<ThreadView>,
    review: Entity<ReviewPanel>,
}

struct AppView {
    sidebar: Entity<Sidebar>,
    composer: Entity<Composer>,
    views: HashMap<String, SessionViews>,
    current: Option<String>,
    metas: Vec<SessionMeta>,
    running: HashSet<String>,
    approval_pending: HashSet<String>,
    stats: HashMap<String, (u32, u32)>,
    agent: AgentClient,
    cwd: PathBuf,
    config_path: Option<PathBuf>,
    exec_mode: pig_protocol::ExecMode,
    git_branch: Option<String>,
    /// hero 页选择的项目目录；None = 未选择（显示"选择项目"，发送时回落到启动目录）
    hero_cwd: Option<PathBuf>,
    hero_branch: Option<String>,
    hero_branches: Vec<String>,
    hero_is_git: bool,
    hero_error: Option<String>,
    pending_first_send: Option<(String, Vec<String>, ExecMode)>,
    projects: Vec<String>,
    settings: Entity<SettingsView>,
    settings_open: bool,
    sidebar_collapsed: bool,
    config: Option<pig_protocol::AppConfig>,
    /// (provider_id, model_id)
    current_model: Option<(String, String)>,
    reasoning_level: Option<String>,
    _agent_handle: pig_core::AgentHandle,
    _subscriptions: Vec<Subscription>,
}

impl AppView {
    fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        config_path: Option<PathBuf>,
        cwd: PathBuf,
    ) -> Self {
        let sidebar = cx.new(|cx| Sidebar::new(window, cx));
        let composer = cx.new(|cx| Composer::new(window, cx));
        let settings = cx.new(|cx| SettingsView::new(window, cx));
        let handle = pig_core::spawn_agent(config_path.clone(), cwd.clone());
        let agent = AgentClient::new(handle.ops.clone());

        let mut app = Self {
            sidebar,
            composer,
            views: HashMap::new(),
            current: None,
            metas: vec![],
            running: HashSet::new(),
            approval_pending: HashSet::new(),
            stats: HashMap::new(),
            agent,
            hero_cwd: None,
            cwd,
            config_path,
            exec_mode: pig_protocol::ExecMode::AutoEdit,
            git_branch: None,
            hero_branch: None,
            hero_branches: vec![],
            hero_is_git: false,
            hero_error: None,
            pending_first_send: None,
            projects: vec![],
            settings,
            settings_open: false,
            sidebar_collapsed: false,
            config: None,
            current_model: None,
            reasoning_level: None,
            _agent_handle: handle,
            _subscriptions: vec![],
        };
        app._subscriptions = vec![
            cx.subscribe_in(&app.composer, window, |this, _, event, window, cx| {
                this.on_composer_event(event, window, cx);
            }),
            cx.subscribe_in(&app.sidebar, window, |this, _, event, window, cx| {
                this.on_sidebar_event(event, window, cx);
            }),
            cx.subscribe(&app.settings, |this, _, event: &SettingsEvent, cx| {
                match event {
                    SettingsEvent::Save(config) => {
                        this.config = Some(config.clone());
                        this.agent.save_config(config.clone());
                        this.apply_config_to_composer(cx);
                    }
                    SettingsEvent::TestProvider(id) => this.agent.test_provider(id.clone()),
                    SettingsEvent::Close => {
                        this.settings_open = false;
                        cx.notify();
                    }
                }
            }),
            cx.observe_window_appearance(window, |_, window, cx| {
                if cx.try_global::<ThemeFollowSystem>().is_some_and(|flag| flag.0) {
                    Theme::sync_system_appearance(Some(window), cx);
                }
            }),
        ];
        app.spawn_event_pump(app._agent_handle.events.clone(), cx);
        app.agent.list_sessions();
        app.agent.list_projects();
        app.agent.get_config();
        if let Some(cwd) = app.hero_cwd.clone() {
            app.agent.git_info(cwd);
        }
        app.push_hero_info(cx);
        app.refresh_git_branch(cx);
        app
    }

    fn spawn_event_pump(&mut self, events: async_channel::Receiver<Event>, cx: &mut Context<Self>) {
        cx.spawn(async move |this: WeakEntity<AppView>, cx| {
            while let Ok(event) = events.recv().await {
                let alive = this
                    .update(cx, |app, cx| {
                        app.route_event(event, cx);
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();
    }

    /// 模拟重启：关旧 manager、清内存视图、重 spawn（自测用）。
    fn restart_agent(&mut self, cx: &mut Context<Self>) {
        let handle = pig_core::spawn_agent(self.config_path.clone(), self.cwd.clone());
        self.agent = AgentClient::new(handle.ops.clone());
        self.views.clear();
        self.current = None;
        self.running.clear();
        self.approval_pending.clear();
        self.stats.clear();
        self._agent_handle = handle;
        self.spawn_event_pump(self._agent_handle.events.clone(), cx);
        self.agent.list_sessions();
        cx.notify();
    }

    fn ensure_views(&mut self, session_id: &str, cx: &mut Context<Self>) {
        if self.views.contains_key(session_id) {
            return;
        }
        let thread = cx.new(|cx| ThreadView::new(cx));
        let review = cx.new(|cx| ReviewPanel::new(cx));
        self._subscriptions.push(cx.subscribe(&thread, |this, _, event, cx| match event {
            ThreadEvent::ApprovalReply {
                request_id,
                decision,
            } => {
                this.agent.approval_reply(request_id.clone(), *decision);
            }
            ThreadEvent::ExecutePlan => {
                this.on_execute_plan(cx);
            }
            ThreadEvent::CancelQueued(text) => {
                if let Some(sid) = this.current.clone() {
                    this.agent.cancel_queued(sid, text.clone());
                }
            }
        }));
        self._subscriptions.push(cx.subscribe(&review, |this, _, event: &ReviewEvent, _| {
            let ReviewEvent::Revert(path) = event;
            if let Some(sid) = this.current.clone() {
                this.agent.revert_file(sid, path.clone());
            }
        }));
        self.views.insert(
            session_id.to_string(),
            SessionViews { thread, review },
        );
    }

    fn route_event(&mut self, event: Event, cx: &mut Context<Self>) {
        match &event {
            Event::SessionConfigured {
                session_id, model, provider_name, ..
            } => {
                let session_id = session_id.clone();
                self.ensure_views(&session_id, cx);
                self.current = Some(session_id.clone());
                let label = if provider_name.is_empty() {
                    model.clone()
                } else {
                    format!("{provider_name}/{model}")
                };
                self.composer.update(cx, |composer, cx| {
                    composer.set_model_name(label, cx);
                    composer.set_hero_mode(false, cx);
                });
                if let Some((text, files, mode)) = self.pending_first_send.take() {
                    self.agent.send_message(session_id, text, files, mode);
                }
            }
            Event::ConfigSnapshot { config } => {
                self.config = Some(config.clone());
                let config = config.clone();
                self.settings
                    .update(cx, |settings, cx| settings.set_config(config, cx));
                self.apply_config_to_composer(cx);
            }
            Event::TestResult {
                provider_id,
                ok,
                message,
            } => {
                self.settings.update(cx, |settings, cx| {
                    settings.set_test_result(provider_id, *ok, message.clone(), cx);
                });
            }
            Event::ProjectList { projects } => {
                self.projects = projects
                    .iter()
                    .map(|p| p.path.display().to_string())
                    .collect();
            }
            Event::SessionList { sessions } => {
                self.metas = sessions.clone();
                if self.current.is_none() && !self.metas.is_empty() {
                    if let Some(meta) = self.metas.iter().find(|s| !s.archived) {
                        self.agent.open_session(meta.id.clone());
                    }
                }
                self.push_hero_info(cx);
            }
            Event::Error { session_id: None, message, .. } => {
                let message = message.clone();
                if self.is_hero(cx) {
                    self.hero_error = Some(message);
                } else if let Some(sid) = self.current.clone() {
                    self.ensure_views(&sid, cx);
                    self.views[&sid].thread.update(cx, |thread, cx| {
                        thread.add_system_note(&format!("⚠ {message}"), cx);
                    });
                }
            }
            Event::FileSearchResults { query, results, .. } => {
                let results = results.clone();
                let _ = query;
                self.composer
                    .update(cx, |composer, cx| composer.set_mention_results(results, cx));
            }
            Event::GitInfo {
                cwd,
                current_branch,
                branches,
            } => {
                if self.hero_cwd.as_ref() == Some(cwd) {
                    self.hero_branch = current_branch.clone();
                    self.hero_branches = branches.clone();
                    self.hero_is_git = current_branch.is_some();
                    if let Some(branch) = current_branch {
                        self.git_branch = Some(branch.clone());
                    }
                    self.push_hero_info(cx);
                }
            }
            Event::BranchChanged { cwd, branch } => {
                if self.hero_cwd.as_ref() == Some(cwd) {
                    self.hero_branch = Some(branch.clone());
                    self.git_branch = Some(branch.clone());
                    self.agent.git_info(cwd.clone());
                }
            }
            Event::ContextUsage {
                session_id, used, total, ..
            } => {
                if self.current.as_deref() == Some(session_id.as_str()) {
                    let (used, total) = (*used, *total);
                    self.composer.update(cx, |composer, cx| {
                        composer.set_context_usage(used, total, cx);
                    });
                }
            }
            Event::TurnComplete {
                session_id,
                duration_ms,
                ..
            } => {
                // duration_ms=0 是会话回放，不触发计划模式待执行标记
                if *duration_ms > 0 && self.exec_mode == pig_protocol::ExecMode::Plan {
                    let session_id = session_id.clone();
                    if let Some(views) = self.views.get(&session_id) {
                        views.thread.update(cx, |thread, cx| {
                            thread.set_plan_pending(true, cx);
                        });
                    }
                }
                self.refresh_git_branch(cx);
            }
            Event::ContextCompacted {
                session_id, note, ..
            } => {
                let session_id = session_id.clone();
                self.ensure_views(&session_id, cx);
                let note = note.clone();
                self.views[&session_id].thread.update(cx, |thread, cx| {
                    thread.add_system_note(&note, cx);
                });
            }
            _ => {}
        }

        // 运行状态跟踪（侧栏状态点 + 当前会话的 composer 状态）
        let sid = event_session_id(&event);
        if let Some(sid) = &sid {
            match &event {
                Event::TurnStarted { .. } => {
                    self.running.insert(sid.clone());
                }
                Event::TurnComplete { .. } | Event::TurnAborted { .. } => {
                    self.running.remove(sid);
                    self.approval_pending.remove(sid);
                }
                Event::ApprovalRequested { .. } => {
                    self.approval_pending.insert(sid.clone());
                }
                Event::Error { .. } => {
                    self.running.remove(sid);
                    self.approval_pending.remove(sid);
                }
                _ => {}
            }
        }

        // 路由到 per-session 视图
        if let Some(sid) = &sid {
            self.ensure_views(sid, cx);
            if let Some(views) = self.views.get(sid) {
                match &event {
                    Event::FileChanged {
                        path,
                        unified_diff,
                        additions,
                        deletions,
                        ..
                    } => {
                        let (path, diff, adds, dels) =
                            (path.clone(), unified_diff.clone(), *additions, *deletions);
                        let totals = views.review.update(cx, |review, cx| {
                            review.upsert(path, diff, adds, dels, cx);
                            review.totals()
                        });
                        self.stats.insert(sid.clone(), totals);
                        views.thread.update(cx, |thread, cx| {
                            thread.set_changes(totals.0, totals.1, cx);
                        });
                    }
                    Event::FileReverted { path, .. } => {
                        let path = path.clone();
                        let totals = views.review.update(cx, |review, cx| {
                            review.remove(&path, cx);
                            review.totals()
                        });
                        self.stats.insert(sid.clone(), totals);
                        views.thread.update(cx, |thread, cx| {
                            thread.set_changes(totals.0, totals.1, cx);
                        });
                    }
                    _ => {
                        views.thread.update(cx, |thread, cx| {
                            thread.reduce_event(event.clone(), cx);
                        });
                    }
                }
            }
            if self.current.as_ref() == Some(sid) {
                self.sync_composer_state(cx);
            }
        }
        self.refresh_sidebar(cx);
        self.sync_hero_mode(cx);
        cx.notify();
    }

    fn sync_composer_state(&self, cx: &mut Context<Self>) {
        let Some(sid) = &self.current else { return };
        let streaming = self.running.contains(sid);
        let pending = self.approval_pending.contains(sid);
        self.composer.update(cx, |composer, cx| {
            composer.set_streaming(streaming, cx);
            composer.set_approval_pending(pending, cx);
        });
    }

    /// 项目列表 = 手动项目 ∪ 会话 cwd；按最近活跃/添加时间倒序。
    fn compute_projects(&self) -> (Vec<String>, Vec<String>) {
        let manual: Vec<String> = self.projects.clone();
        let mut latest: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        for meta in &self.metas {
            let key = meta.cwd.display().to_string();
            let entry = latest.entry(key).or_insert(0);
            *entry = (*entry).max(meta.updated_at);
        }
        for path in &manual {
            latest.entry(path.clone()).or_insert(0);
        }
        let mut all: Vec<(String, u64)> = latest.into_iter().collect();
        all.sort_by_key(|(_, ts)| std::cmp::Reverse(*ts));
        (all.into_iter().map(|(path, _)| path).collect(), manual)
    }

    fn refresh_sidebar(&self, cx: &mut Context<Self>) {
        let sessions: Vec<SidebarSession> = self
            .metas
            .iter()
            .map(|meta| {
                let (added, removed) = self.stats.get(&meta.id).copied().unwrap_or((0, 0));
                SidebarSession {
                    id: meta.id.clone(),
                    title: meta.title.clone(),
                    cwd: meta.cwd.clone(),
                    updated_at: meta.updated_at,
                    pinned: meta.pinned,
                    archived: meta.archived,
                    running: self.running.contains(&meta.id),
                    waiting_approval: self.approval_pending.contains(&meta.id),
                    added: (added > 0).then_some(added),
                    removed: (removed > 0).then_some(removed),
                }
            })
            .collect();
        let (projects, manual_projects) = self.compute_projects();
        let active = self.current.clone();
        self.sidebar.update(cx, |sidebar, cx| {
            sidebar.set_state(sessions, projects, manual_projects, active, cx);
        });
    }

    fn switch_session(&mut self, session_id: String, cx: &mut Context<Self>) {
        if self.views.contains_key(&session_id) {
            self.current = Some(session_id);
            self.sync_composer_state(cx);
            self.refresh_sidebar(cx);
            cx.notify();
        } else {
            self.agent.open_session(session_id);
        }
    }

    /// hero 态：当前无会话，或当前会话没有任何消息
    fn is_hero(&self, cx: &App) -> bool {
        match &self.current {
            None => true,
            Some(sid) => self
                .views
                .get(sid)
                .map(|views| views.thread.read(cx).is_empty())
                .unwrap_or(true),
        }
    }

    fn push_hero_info(&self, cx: &mut Context<Self>) {
        let label = self
            .hero_cwd
            .as_ref()
            .map(|cwd| {
                cwd.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| cwd.display().to_string())
            })
            .unwrap_or_else(|| "选择项目".to_string());
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

    fn enter_hero(&mut self, cx: &mut Context<Self>) {
        self.current = None;
        self.hero_error = None;
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
        cx.notify();
    }

    fn hero_send(&mut self, text: String, files: Vec<String>, mode: ExecMode, cx: &mut Context<Self>) {
        self.pending_first_send = Some((text, files, mode));
        let cwd = self.hero_cwd.clone().unwrap_or_else(|| self.cwd.clone());
        self.agent.new_session(cwd);
        self.composer.update(cx, |composer, cx| {
            composer.set_hero_mode(false, cx);
        });
        cx.notify();
    }

    fn pick_directory(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("选择工作目录".into()),
        });
        let view = cx.entity();
        cx.spawn_in(window, async move |_, window| {
            let picked = rx.await.ok()?.ok()??.into_iter().next()?;
            if !picked.is_dir() {
                return None;
            }
            window
                .update(|_, cx| {
                    view.update(cx, |this, cx| {
                        this.hero_error = None;
                        this.hero_branch = None;
                        this.hero_branches = vec![];
                        this.agent.git_info(picked.clone());
                        this.hero_cwd = Some(picked);
                        this.push_hero_info(cx);
                        cx.notify();
                    });
                })
                .ok()?;
            Some(())
        })
        .detach();
    }

    fn on_execute_plan(&mut self, cx: &mut Context<Self>) {
        let Some(sid) = self.current.clone() else { return };
        self.exec_mode = pig_protocol::ExecMode::ConfirmBeforeEdit;
        self.composer.update(cx, |composer, cx| {
            composer.set_exec_mode(pig_protocol::ExecMode::ConfirmBeforeEdit, cx);
        });
        self.agent
            .set_exec_mode(sid.clone(), pig_protocol::ExecMode::ConfirmBeforeEdit);
        let text = "计划已确认，请按计划开始执行".to_string();
        if let Some(views) = self.views.get(&sid) {
            views.thread.update(cx, |thread, cx| {
                thread.append_user_message(text.clone(), vec![], cx);
            });
        }
        self.agent
            .send_message(sid, text, vec![], pig_protocol::ExecMode::ConfirmBeforeEdit);
    }

    fn on_composer_event(
        &mut self,
        event: &ComposerEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            ComposerEvent::Send { text, files, mode } => {
                if self.is_hero(cx) {
                    self.hero_send(text.clone(), files.clone(), *mode, cx);
                    return;
                }
                let Some(sid) = self.current.clone() else {
                    return;
                };
                // 用户消息由 core 的 Event::UserMessage 统一上屏（含排队出队路径）
                self.agent
                    .send_message(sid, text.clone(), files.clone(), *mode);
            }
            ComposerEvent::PickDirectory => self.pick_directory(window, cx),
            ComposerEvent::SelectCwd(cwd) => {
                let cwd = PathBuf::from(cwd);
                self.hero_error = None;
                self.agent.git_info(cwd.clone());
                self.hero_cwd = Some(cwd);
                self.push_hero_info(cx);
            }
            ComposerEvent::ClearCwd => {
                self.hero_cwd = None;
                self.hero_error = None;
                self.hero_branch = None;
                self.hero_branches = vec![];
                self.hero_is_git = false;
                self.push_hero_info(cx);
                cx.notify();
            }
            ComposerEvent::CheckoutBranch(branch) => {
                if let Some(cwd) = self.hero_cwd.clone() {
                    self.agent.checkout_branch(cwd, branch.clone());
                }
            }
            ComposerEvent::Stop => {
                if let Some(sid) = &self.current {
                    self.agent.interrupt(sid.clone());
                }
            }
            ComposerEvent::Clear => {
                if let Some(sid) = &self.current {
                    if let Some(views) = self.views.get(sid) {
                        views.thread.update(cx, |thread, cx| thread.clear(cx));
                    }
                }
            }
            ComposerEvent::Compact => {
                if let Some(sid) = &self.current {
                    self.agent.compact(sid.clone());
                }
            }
            ComposerEvent::SetModel {
                provider_id,
                model_id,
            } => {
                self.current_model = Some((provider_id.clone(), model_id.clone()));
                if let Some(sid) = &self.current {
                    self.agent.set_model(
                        sid.clone(),
                        provider_id.clone(),
                        model_id.clone(),
                        self.reasoning_level.clone(),
                    );
                }
            }
            ComposerEvent::SetReasoning(level) => {
                self.reasoning_level = level.clone();
                // 已选模型时立即带推理等级重发
                if let Some(sid) = &self.current {
                    if let Some((provider_id, model_id)) = self.current_model.clone() {
                        self.agent.set_model(
                            sid.clone(),
                            provider_id,
                            model_id,
                            level.clone(),
                        );
                    }
                }
            }
            ComposerEvent::SetExecMode(mode) => {
                self.exec_mode = *mode;
                if let Some(sid) = &self.current {
                    self.agent.set_exec_mode(sid.clone(), *mode);
                }
            }
            ComposerEvent::OpenSettings => self.open_settings(cx),
            ComposerEvent::SearchFiles(query) => {
                if let Some(sid) = &self.current {
                    self.agent.search_files(sid.clone(), query.clone());
                }
            }
        }
    }

    fn on_sidebar_event(
        &mut self,
        event: &SidebarEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            SidebarEvent::Select(id) => self.switch_session(id.clone(), cx),
            SidebarEvent::NewTask => {
                self.hero_cwd = None;
                self.enter_hero(cx);
            }
            SidebarEvent::NewTaskInProject(path) => {
                self.hero_cwd = Some(PathBuf::from(path));
                self.enter_hero(cx);
            }
            SidebarEvent::SetPinned(id, pinned) => self.agent.set_pinned(id, *pinned),
            SidebarEvent::SetArchived(id, archived) => self.agent.set_archived(id, *archived),
            SidebarEvent::AddProject => {
                let rx = cx.prompt_for_paths(PathPromptOptions {
                    files: false,
                    directories: true,
                    multiple: false,
                    prompt: Some("添加项目目录".into()),
                });
                let view = cx.entity();
                cx.spawn_in(window, async move |_, window| {
                    let picked = rx.await.ok()?.ok()??.into_iter().next()?;
                    if !picked.is_dir() {
                        return None;
                    }
                    window
                        .update(|_, cx| {
                            view.update(cx, |this, _| {
                                this.agent.add_project(picked);
                            });
                        })
                        .ok()?;
                    Some(())
                })
                .detach();
            }
            SidebarEvent::RemoveProject(path) => {
                self.agent.remove_project(PathBuf::from(path));
            }
            SidebarEvent::OpenSettings => self.open_settings(cx),
        }
    }

    fn refresh_git_branch(&mut self, cx: &mut Context<Self>) {
        let cwd = self.cwd.clone();
        let task = cx.background_executor().spawn(async move {
            let branch = std::process::Command::new("git")
                .args(["rev-parse", "--abbrev-ref", "HEAD"])
                .current_dir(&cwd)
                .output()
                .ok()?;
            if !branch.status.success() {
                return None;
            }
            let name = String::from_utf8_lossy(&branch.stdout).trim().to_string();
            (!name.is_empty()).then_some(name)
        });
        cx.spawn(async move |this: WeakEntity<AppView>, cx| {
            let branch = task.await;
            let _ = this.update(cx, |app, cx| {
                app.git_branch = branch;
                cx.notify();
            });
        })
        .detach();
    }

    /// ConfigSnapshot → composer 模型列表（启用供应商的启用模型，按供应商分组平铺）
    fn apply_config_to_composer(&self, cx: &mut Context<Self>) {
        let Some(config) = &self.config else { return };
        let models: Vec<crate::composer::ModelOption> = config
            .providers
            .iter()
            .filter(|p| p.enabled)
            .flat_map(|p| {
                p.models
                    .iter()
                    .filter(|m| m.enabled)
                    .map(|m| (p.name.clone(), p.id.clone(), m.id.clone(), m.reasoning_levels.clone()))
                    .collect::<Vec<_>>()
            })
            .collect();
        self.composer
            .update(cx, |composer, cx| composer.set_models(models, cx));
    }

    fn open_settings(&mut self, cx: &mut Context<Self>) {
        self.settings_open = true;
        self.agent.get_config();
        cx.notify();
    }

    /// 自测用。
    pub fn debug_config(&self) -> Option<&pig_protocol::AppConfig> {
        self.config.as_ref()
    }

    fn sync_hero_mode(&mut self, cx: &mut Context<Self>) {
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

    fn render_hero(&self, cx: &mut Context<Self>) -> AnyElement {
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
            ("总结这个项目", "请阅读 README 并总结这个项目的结构和主要模块。"),
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
                this.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().danger)
                        .child(error),
                )
            })
            .into_any_element()
    }

    fn render_title_bar(&self, cx: &mut Context<Self>) -> TitleBar {
        let is_dark = cx.theme().mode.is_dark();
        let title = self
            .current
            .as_ref()
            .and_then(|id| self.metas.iter().find(|m| &m.id == id))
            .map(|m| m.title.clone())
            .unwrap_or_else(|| "pig-code".to_string());

        TitleBar::new()
            .child(
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
                        .child(format!(
                            "pig-code · {title}{}",
                            self.git_branch
                                .as_ref()
                                .map(|b| format!(" · ⎇ {b}"))
                                .unwrap_or_default()
                        )),
                ),
            )
            .child(
                h_flex()
                    .gap_1()
                    .px_2()
                    .child(
                        Button::new("toggle-theme")
                            .ghost()
                            .small()
                            .occlude()
                            .icon(if is_dark { IconName::Sun } else { IconName::Moon })
                            .on_click(move |_, _, cx| {
                                Theme::change(
                                    if is_dark { ThemeMode::Light } else { ThemeMode::Dark },
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
                    ),
            )
    }
}

fn event_session_id(event: &Event) -> Option<String> {
    match event {
        Event::SessionConfigured { session_id, .. }
        | Event::UserMessage { session_id, .. }
        | Event::TurnStarted { session_id, .. }
        | Event::ReasoningDelta { session_id, .. }
        | Event::TextDelta { session_id, .. }
        | Event::TextDone { session_id, .. }
        | Event::ToolCallBegin { session_id, .. }
        | Event::ToolCallEnd { session_id, .. }
        | Event::ContextUsage { session_id, .. }
        | Event::ContextCompacted { session_id, .. }
        | Event::ApprovalRequested { session_id, .. }
        | Event::FileChanged { session_id, .. }
        | Event::FileReverted { session_id, .. }
        | Event::TurnComplete { session_id, .. }
        | Event::TurnAborted { session_id, .. }
        | Event::MessageQueued { session_id, .. }
        | Event::FileSearchResults { session_id, .. } => Some(session_id.clone()),
        Event::SessionList { .. }
        | Event::GitInfo { .. }
        | Event::BranchChanged { .. }
        | Event::ConfigSnapshot { .. }
        | Event::TestResult { .. }
        | Event::ProjectList { .. } => None,
        Event::Error { session_id, .. } => session_id.clone(),
    }
}

impl Render for AppView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let hero = self.is_hero(cx) && self.pending_first_send.is_none();
        let current_views = self.current.as_ref().and_then(|id| self.views.get(id));

        let center: AnyElement = if hero {
            self.render_hero(cx)
        } else if let Some(views) = current_views {
            v_flex()
                .size_full()
                .child(div().flex_1().min_h_0().child(views.thread.clone()))
                .child(self.composer.clone())
                .into_any_element()
        } else {
            v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("新建任务或从左侧选择会话"),
                )
                .into_any_element()
        };

        let page_tag = if self.settings_open {
            "settings".to_string()
        } else if hero {
            "hero".to_string()
        } else {
            self.current.clone().unwrap_or_default()
        };
        let center = div()
            .size_full()
            .with_animation(
                format!("page-{page_tag}"),
                Animation::new(std::time::Duration::from_millis(150)).with_easing(ease_out_quint()),
                |el, delta| el.opacity(delta),
            )
            .child(center)
            .into_any_element();

        let right: AnyElement = if let Some(views) = current_views {
            views.review.clone().into_any_element()
        } else {
            div().size_full().into_any_element()
        };

        v_flex()
            .id("app-root")
            .key_context("app")
            .on_action(cx.listener(|this, _: &NewTask, _, cx| {
                this.hero_cwd = None;
                this.enter_hero(cx);
            }))
            .on_action(cx.listener(|this, _: &FocusSearch, window, cx| {
                this.sidebar
                    .update(cx, |sidebar, cx| sidebar.open_search(window, cx));
            }))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| {
                this.sidebar_collapsed = !this.sidebar_collapsed;
                cx.notify();
            }))
            .size_full()
            .child(self.render_title_bar(cx))
            .child(
                div().flex_1().min_h_0().child(if self.settings_open {
                    self.settings.clone().into_any_element()
                } else {
                    h_resizable("main-columns")
                        .when(!self.sidebar_collapsed, |this| {
                            this.child(
                                resizable_panel()
                                    .size(px(220.))
                                    .size_range(px(180.)..px(360.))
                                    .child(
                                        div()
                                            .size_full()
                                            .with_animation(
                                                "sidebar-enter",
                                                Animation::new(std::time::Duration::from_millis(150))
                                                    .with_easing(ease_out_quint()),
                                                |el, delta| {
                                                    el.left(px(-8.0 * (1.0 - delta))).opacity(delta)
                                                },
                                            )
                                            .child(self.sidebar.clone()),
                                    ),
                            )
                        })
                        .child(center)
                        .child(
                            resizable_panel()
                                .size(px(300.))
                                .size_range(px(220.)..px(520.))
                                .child(right),
                        )
                        .into_any_element()
                }),
            )
    }
}

/// 自测环境：mock provider + 临时配置/工作目录 + 隔离数据目录。
struct SelftestEnv {
    config_path: PathBuf,
    cwd: PathBuf,
}

fn setup_selftest() -> SelftestEnv {
    let port = pig_core::mock::start_mock_server();
    let dir = std::env::temp_dir().join(format!("pig-app-selftest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create selftest dir");
    std::fs::write(
        dir.join(pig_core::mock::MOCK_FILE_NAME),
        pig_core::mock::MOCK_FILE_CONTENT,
    )
    .expect("write mock file");
    let data_dir = dir.join("data");
    std::fs::create_dir_all(&data_dir).expect("create data dir");
    // agent 通过 PIG_DATA_DIR 找到隔离数据目录
    unsafe { std::env::set_var("PIG_DATA_DIR", &data_dir) };
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"default_provider = "mock"
default_model = "mock-model"

[[providers]]
id = "mock"
name = "Mock"
base_url = "http://127.0.0.1:{port}/v1"
api_key = "mock-key"
api_format = "OpenAiChat"
enabled = true

[[providers.models]]
id = "mock-model"
context_window = 128000
max_output_tokens = 8192

[[providers]]
id = "anthropic"
name = "AnthropicMock"
base_url = "http://127.0.0.1:{port}/v1"
api_key = "mock-key"
api_format = "AnthropicMessages"
enabled = true

[[providers.models]]
id = "mock-model"
context_window = 200000
max_output_tokens = 8192
"#
        ),
    )
    .expect("write config");
    SelftestEnv {
        config_path,
        cwd: dir,
    }
}

fn main() {
    // PIG_NET_TEST=1：不开窗口，用真实配置逐个测试供应商连通性（网络排障用）
    // PIG_NET_TEST=full：再走一遍完整发消息链路（含系统提示词与工具），打印事件流
    if let Some(mode) = std::env::var_os("PIG_NET_TEST") {
        if mode == "full" {
            pig_core::net_test_full_turn(None);
        } else {
            pig_core::provider::net_test_blocking(&pig_core::config::default_path());
        }
        return;
    }

    let selftest = std::env::var_os("PIG_SELFTEST").is_some();
    let setup = selftest.then(setup_selftest);
    let cwd = setup
        .as_ref()
        .map(|s| s.cwd.clone())
        .unwrap_or_else(|| std::env::current_dir().expect("cwd"));
    let config_path = setup.map(|s| s.config_path);

    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            cx.set_global(ThemeFollowSystem(true));

            cx.bind_keys([
                KeyBinding::new("ctrl-n", NewTask, None),
                KeyBinding::new("ctrl-k", FocusSearch, None),
                KeyBinding::new("ctrl-b", ToggleSidebar, None),
                KeyBinding::new("escape", CloseSearch, Some("search")),
                KeyBinding::new("escape", CloseSettings, Some("settings")),
            ]);

            // 初始窗口不超出显示器可用区域：GPUI 的尺寸是逻辑像素，缩放下
            // 1280x800 可能比实际屏幕还大，底部会被任务栏挡住；给边框和任务栏留余量。
            let mut window_size = size(px(1280.), px(800.));
            if let Some(display) = cx.primary_display() {
                let bounds = display.bounds();
                window_size = size(
                    window_size.width.min(bounds.size.width - px(32.)),
                    window_size.height.min(bounds.size.height - px(96.)),
                );
            }
            let window_bounds = WindowBounds::centered(window_size, cx);

            cx.spawn(async move |cx| {
                let options = WindowOptions {
                    window_bounds: Some(window_bounds),
                    window_min_size: Some(size(px(960.), px(600.))),
                    ..TitleBar::window_options()
                };

                cx.open_window(options, |window, cx| {
                    // gpui-kit init 固定为亮色，开窗时按系统外观覆盖
                    Theme::sync_system_appearance(Some(window), cx);
                    let view =
                        cx.new(|cx| AppView::new(window, cx, config_path.clone(), cwd.clone()));
                    if selftest {
                        let view = view.clone();
                        cx.spawn(async move |cx| {
                            run_selftest(view, cx).await;
                        })
                        .detach();
                    }
                    cx.new(|cx| Root::new(view, window, cx))
                })
                .expect("Failed to open window");
            })
            .detach();
        });
}

/// PIG_SELFTEST=1：会话A完整修改链 → 会话B并行对话 → 切回A → 模拟重启 resume → @搜索。
async fn run_selftest(view: Entity<AppView>, cx: &mut AsyncApp) {
    use std::time::Duration;
    macro_rules! timer {
        ($ms:expr) => {
            cx.background_executor().timer(Duration::from_millis($ms))
        };
    }
    macro_rules! app {
        ($f:expr) => {
            view.update(cx, $f)
        };
    }

    timer!(800).await;

    // hero 态断言：无会话、hero 展示
    let is_hero = app!(|app: &mut AppView, cx| app.debug_is_hero(cx));
    assert!(is_hero, "启动应进入 hero 态");
    println!("[selftest] hero 态 OK");

    // hero 发送首条消息 → 自动建会话
    app!(|app: &mut AppView, cx| {
        app.exec_mode = pig_protocol::ExecMode::ConfirmBeforeEdit;
        app.hero_send(
            format!("{} 创建并修改文件，然后跑个命令", pig_core::mock::SCENARIO_B_TRIGGER),
            vec![],
            pig_protocol::ExecMode::ConfirmBeforeEdit,
            cx,
        );
    });
    let session_a = loop {
        timer!(200).await;
        let current = app!(|app: &mut AppView, _| app.current.clone());
        if let Some(id) = current {
            break id;
        }
    };
    println!("[selftest] hero 发送 → 会话 A 建立: {session_a}");

    // 会话 A：场景 B（审批×3）
    let mut approvals = 0u32;
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 60_000, "会话 A 回合超时");
        let approved = app!(|app: &mut AppView, cx| {
            let views = app.views.get(&session_a)?;
            let pending = views.thread.read(cx).pending_approval();
            pending.map(|_| {
                views.thread.update(cx, |thread, cx| {
                    thread.decide_pending(pig_protocol::ApprovalDecision::Allow, cx);
                });
            })
        });
        if approved == Some(()) {
            approvals += 1;
            continue;
        }
        let done = app!(|app: &mut AppView, cx| {
            let views = app.views.get(&session_a)?;
            let streaming = views.thread.read(cx).is_streaming();
            let (tool_done, text, _, tool_output) = views.thread.read(cx).debug_last_assistant();
            if !streaming && waited > 1000 && tool_done {
                assert!(text.contains(pig_core::mock::SCENARIO_B_MARKER), "A 文本标记: {text}");
                assert!(tool_output.contains(pig_core::mock::SCENARIO_B_BASH_MARKER));
                return Some(());
            }
            None
        });
        if done == Some(()) {
            break;
        }
    }
    assert_eq!(approvals, 3, "场景 B 三次审批");
    // hero → 会话态切换断言
    let is_hero = app!(|app: &mut AppView, cx| app.debug_is_hero(cx));
    assert!(!is_hero, "发送后应进入会话态");
    println!("[selftest] 会话 A 场景 B 完成（审批×3），输入框已沉底");

    // 项目视图：会话 cwd 应出现在项目列表，且按项目分组正确
    let cwd_str = app!(|app: &mut AppView, _| app.cwd.display().to_string());
    let (has_cwd, grouped) = app!(|app: &mut AppView, cx| {
        let sidebar = app.sidebar.read(cx);
        (
            sidebar.debug_projects().contains(&cwd_str),
            sidebar.debug_project_sessions(&cwd_str).contains(&session_a),
        )
    });
    assert!(has_cwd, "项目列表应包含会话 cwd");
    assert!(grouped, "项目视图应按 cwd 分组会话");

    // 添加/移除项目
    let extra = std::env::temp_dir().join(format!("pig-app-proj-{}", std::process::id()));
    std::fs::create_dir_all(&extra).unwrap();
    let extra_str = extra.display().to_string();
    app!(|app: &mut AppView, _| app.agent.add_project(extra.clone()));
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 10_000, "添加项目超时");
        let has = app!(|app: &mut AppView, cx| {
            app.sidebar.read(cx).debug_projects().contains(&extra_str)
        });
        if has {
            break;
        }
    }
    app!(|app: &mut AppView, _| app.agent.remove_project(extra.clone()));
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 10_000, "移除项目超时");
        let has = app!(|app: &mut AppView, cx| {
            app.sidebar.read(cx).debug_projects().contains(&extra_str)
        });
        if !has {
            break;
        }
    }
    println!("[selftest] 项目列表 OK（会话 cwd 自动出现 + 手动增删）");


    // 会话 B：新建 + 场景 A
    app!(|app: &mut AppView, _| app.agent.new_session(app.cwd.clone()));
    let session_b = loop {
        timer!(200).await;
        let current = app!(|app: &mut AppView, _| app.current.clone());
        if let Some(id) = current {
            if id != session_a {
                break id;
            }
        }
    };
    println!("[selftest] 会话 B 就绪: {session_b}");
    app!(|app: &mut AppView, _| {
        app.agent.send_message(
            session_b.clone(),
            "读一下 README.mock.md 并总结".to_string(),
            vec![],
            pig_protocol::ExecMode::AutoEdit,
        );
    });
    timer!(300).await; // 等第一个回合开跑
    app!(|app: &mut AppView, _| {
        app.agent.send_message(
            session_b.clone(),
            "ECHO_HISTORY".to_string(),
            vec![],
            pig_protocol::ExecMode::AutoEdit,
        );
    });
    let mut saw_queued = false;
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 60_000, "排队流程超时");
        let (queued_len, streaming, text) = app!(|app: &mut AppView, cx| {
            let Some(views) = app.views.get(&session_b) else {
                return (0, false, String::new());
            };
            let thread = views.thread.read(cx);
            let (_, text, _, _) = thread.debug_last_assistant();
            (thread.debug_queued().len(), thread.is_streaming(), text)
        });
        saw_queued |= queued_len > 0;
        if !streaming && text.contains("HISTORY_COUNT:6") {
            break;
        }
    }
    assert!(saw_queued, "应出现排队芯片");
    println!("[selftest] 会话 B 完成，消息排队 OK（自动接续，第二轮历史=6）");

    // 切回 A：内存状态应原样保留
    app!(|app: &mut AppView, cx| app.switch_session(session_a.clone(), cx));
    let current = app!(|app: &mut AppView, _| app.current.clone());
    assert_eq!(current.as_ref(), Some(&session_a));
    let kept = app!(|app: &mut AppView, cx| {
        let views = app.views.get(&session_a).expect("A 视图在内存");
        let (_, text, _, _) = views.thread.read(cx).debug_last_assistant();
        text.contains(pig_core::mock::SCENARIO_B_MARKER)
    });
    assert!(kept, "切回 A 后内容应保留");
    println!("[selftest] 会话切换 OK");

    // 模拟重启：drop manager 重 spawn + ListSessions/OpenSession 重放
    app!(|app: &mut AppView, cx| app.restart_agent(cx));
    // 等自动打开最近会话（B，updated_at 最新），再显式切到 A 触发重放
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 10_000, "重启后自动打开会话超时");
        let ready = app!(|app: &mut AppView, _| app.current.is_some());
        if ready {
            break;
        }
    }
    app!(|app: &mut AppView, cx| app.switch_session(session_a.clone(), cx));
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 30_000, "重启后重放超时");
        let done = app!(|app: &mut AppView, cx| {
            let views = app.views.get(&session_a)?;
            let (tool_done, text, _, _) = views.thread.read(cx).debug_last_assistant();
            let review_state = views.review.read(cx).debug_state();
            (tool_done
                && text.contains(pig_core::mock::SCENARIO_B_MARKER)
                && review_state.0 == 1
                && review_state.1 == 3)
            .then_some(())
        });
        if done == Some(()) {
            break;
        }
    }
    println!("[selftest] 模拟重启 resume OK（消息+工具卡+diff 全恢复）");

    // @搜索：真实文件
    app!(|app: &mut AppView, _| {
        app.agent.search_files(session_a.clone(), "hello".to_string());
    });
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 10_000, "@搜索超时");
        let found = app!(|app: &mut AppView, cx| {
            app.composer.read(cx).debug_mention_results().iter().any(|r| r.contains("hello.txt"))
        });
        if found {
            break;
        }
    }
    println!("[selftest] @搜索 OK");

    // compact（模型摘要）
    app!(|app: &mut AppView, _| app.agent.compact(session_a.clone()));
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 15_000, "compact 超时");
        let compacted = app!(|app: &mut AppView, cx| {
            app.views
                .get(&session_a)
                .map(|views| views.thread.read(cx).debug_system_notes())
                .unwrap_or_default()
                .iter()
                .any(|note| note.contains("模型摘要") && note.contains(pig_core::mock::SUMMARY_MARKER))
        });
        if compacted {
            break;
        }
    }
    println!("[selftest] 模型摘要 compact OK");

    // 场景 C：计划模式闭环
    app!(|app: &mut AppView, _| app.agent.new_session(app.cwd.clone()));
    let session_c = loop {
        timer!(200).await;
        let current = app!(|app: &mut AppView, _| app.current.clone());
        if let Some(id) = current {
            if id != session_a {
                break id;
            }
        }
    };
    app!(|app: &mut AppView, _| {
        app.exec_mode = pig_protocol::ExecMode::Plan;
        app.agent.set_exec_mode(session_c.clone(), pig_protocol::ExecMode::Plan);
        app.agent.send_message(
            session_c.clone(),
            format!("{} 给我一个改造计划", pig_core::mock::SCENARIO_C_TRIGGER),
            vec![],
            pig_protocol::ExecMode::Plan,
        );
    });
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 30_000, "场景 C 计划超时");
        let ready = app!(|app: &mut AppView, cx| {
            let views = app.views.get(&session_c)?;
            let thread = views.thread.read(cx);
            let (_, text, _, _) = thread.debug_last_assistant();
            (thread.is_plan_pending() && text.contains(pig_core::mock::PLAN_MARKER)).then_some(())
        });
        if ready == Some(()) {
            break;
        }
    }
    println!("[selftest] 计划模式输出计划，执行计划按钮出现");

    // 点「执行计划」（走与按钮相同路径）
    app!(|app: &mut AppView, cx| {
        let views = app.views.get(&session_c).expect("C 视图");
        views.thread.update(cx, |thread, cx| thread.trigger_execute_plan(cx));
    });
    let mode = app!(|app: &mut AppView, cx| app.composer.read(cx).debug_exec_mode());
    assert_eq!(mode, pig_protocol::ExecMode::ConfirmBeforeEdit, "模式应切到变更前确认");

    // 场景 B 工具链执行（ConfirmBeforeEdit → 3 次审批）
    let mut approvals = 0u32;
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 60_000, "场景 C 执行超时");
        let approved = app!(|app: &mut AppView, cx| {
            let views = app.views.get(&session_c)?;
            views.thread.read(cx).pending_approval().map(|_| {
                views.thread.update(cx, |thread, cx| {
                    thread.decide_pending(pig_protocol::ApprovalDecision::Allow, cx);
                });
            })
        });
        if approved == Some(()) {
            approvals += 1;
            continue;
        }
        let done = app!(|app: &mut AppView, cx| {
            let views = app.views.get(&session_c)?;
            let thread = views.thread.read(cx);
            let (tool_done, text, _, _) = thread.debug_last_assistant();
            (!thread.is_streaming() && waited > 1000 && tool_done
                && text.contains(pig_core::mock::SCENARIO_B_MARKER))
            .then_some(())
        });
        if done == Some(()) {
            break;
        }
    }
    assert_eq!(approvals, 3, "执行计划后场景 B 三次审批");
    println!("[selftest] 计划确认 → 模式切换 → 工具链执行 OK");

    // 水位条
    let usage = app!(|app: &mut AppView, cx| app.composer.read(cx).debug_context_usage());
    assert_eq!(usage, Some((142, 128_000)), "水位条数据: {usage:?}");
    println!("[selftest] 上下文水位条 OK");

    // 设置页数据：ConfigSnapshot 已收到、composer 模型列表已填充
    let (provider_count, model_count) = app!(|app: &mut AppView, cx| {
        (
            app.debug_config().map(|c| c.providers.len()).unwrap_or(0),
            app.composer.read(cx).debug_model_count(),
        )
    });
    assert_eq!(provider_count, 2, "ConfigSnapshot 应含 2 个供应商");
    assert_eq!(model_count, 2, "composer 应列出 2 个模型");
    println!("[selftest] ConfigSnapshot + 模型列表 OK");

    // Anthropic 供应商端到端：会话 D 切到 anthropic 模型跑场景 B
    app!(|app: &mut AppView, _| app.agent.new_session(app.cwd.clone()));
    let session_d = loop {
        timer!(200).await;
        let current = app!(|app: &mut AppView, _| app.current.clone());
        if let Some(id) = current {
            if id != session_a && id != session_b {
                break id;
            }
        }
    };
    app!(|app: &mut AppView, _| {
        app.agent.set_model(
            session_d.clone(),
            "anthropic".to_string(),
            "mock-model".to_string(),
            None,
        );
        app.agent.send_message(
            session_d.clone(),
            format!("{} 创建并修改文件，然后跑个命令", pig_core::mock::SCENARIO_B_TRIGGER),
            vec![],
            pig_protocol::ExecMode::AutoEdit,
        );
    });
    let mut approvals = 0u32;
    let mut waited = 0u64;
    loop {
        timer!(200).await;
        waited += 200;
        assert!(waited < 60_000, "Anthropic 场景 B 超时");
        let approved = app!(|app: &mut AppView, cx| {
            let views = app.views.get(&session_d)?;
            views.thread.read(cx).pending_approval().map(|_| {
                views.thread.update(cx, |thread, cx| {
                    thread.decide_pending(pig_protocol::ApprovalDecision::Allow, cx);
                });
            })
        });
        if approved == Some(()) {
            approvals += 1;
            continue;
        }
        let done = app!(|app: &mut AppView, cx| {
            let views = app.views.get(&session_d)?;
            let thread = views.thread.read(cx);
            let (tool_done, text, thinking, _) = thread.debug_last_assistant();
            let _ = thinking;
            (!thread.is_streaming() && waited > 1000 && tool_done
                && text.contains(pig_core::mock::SCENARIO_B_MARKER))
            .then_some(())
        });
        if done == Some(()) {
            break;
        }
    }
    assert_eq!(approvals, 1, "AutoEdit 下仅 bash 审批");
    println!("[selftest] Anthropic 供应商端到端 OK");

    println!("SELFTEST PASS");
    std::process::exit(0);
}
