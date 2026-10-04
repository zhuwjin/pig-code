//! 内嵌终端：底部面板的对外入口。
//!
//! TerminalPanel 持有若干 TerminalView tab + 激活下标 + 新建 tab 用的 cwd。
//! 标签页栏样式参照 crates/pig-app/src/right_panel.rs 的 render_right_tab_bar。
//! 只有 TerminalPanel / TerminalPanelEvent 对外 pub，其余模块全是 crate 内实现细节。

mod colors;
mod element;
mod input;
mod pty;
mod term;
mod view;

use std::path::PathBuf;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, EventEmitter, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _,
    Subscription, Window, div, px,
};

use term::Terminal;
use view::{TerminalView, TerminalViewEvent};

/// 面板事件：目前只有「请求折叠」（点标签页栏末尾的向下箭头）
pub enum TerminalPanelEvent {
    RequestCollapse,
}

impl EventEmitter<TerminalPanelEvent> for TerminalPanel {}

/// 底部终端面板：上 = 标签页栏；下 = 激活 tab 的 TerminalView。
/// 全部 tab 关闭后留空态（「无终端」+ 新建按钮），不自动重开。
pub struct TerminalPanel {
    tabs: Vec<Entity<TerminalView>>,
    /// tabs 为空时无意义（渲染走空态分支）
    active: usize,
    /// 新建 tab 的工作目录（set_cwd 更新，已开的 tab 不受影响）
    cwd: PathBuf,
    /// 新建 tab 的 shell（None = 系统默认；set_shell 更新，已开的 tab 不受影响）
    shell: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl TerminalPanel {
    /// 创建面板并立即开一个终端 tab（cwd 为 shell 工作目录，shell None = 系统默认）
    pub fn new(
        cwd: PathBuf,
        shell: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            tabs: Vec::new(),
            active: 0,
            cwd,
            shell,
            _subscriptions: Vec::new(),
        };
        this.open_tab(window, cx);
        this
    }

    /// 更新后续新建 tab 的工作目录（会话切换时调用，已开的 tab 不变）
    pub fn set_cwd(&mut self, cwd: PathBuf, cx: &mut Context<Self>) {
        self.cwd = cwd;
        cx.notify();
    }

    /// 更新后续新建 tab 的 shell（设置变更时调用，已开的 tab 不变）
    pub fn set_shell(&mut self, shell: Option<String>, cx: &mut Context<Self>) {
        self.shell = shell;
        cx.notify();
    }

    /// 自测用：当前 tab 数
    pub(crate) fn debug_tab_count(&self) -> usize {
        self.tabs.len()
    }

    /// 用当前 cwd 与配置的 shell 起新 tab（PTY spawn 失败只记日志，不加 tab）
    fn open_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let terminal = match Terminal::spawn(&self.cwd, self.shell.as_deref()) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[terminal] 起 shell 失败（{}）：{e}", self.cwd.display());
                return;
            }
        };
        let view = cx.new(|cx| TerminalView::new(terminal, window, cx));
        // 子进程退出 → 刷新标签（追加「（已退出）」）
        self._subscriptions.push(
            cx.subscribe(&view, |_this, _view, _ev: &TerminalViewEvent, cx| {
                cx.notify()
            }),
        );
        self.tabs.push(view);
        self.active = self.tabs.len() - 1;
        self.focus_active(window, cx);
        cx.notify();
    }

    /// 关 tab：drop TerminalView 即释放 PTY（Pty 的 Drop 会 kill 子进程）。
    /// 关的是激活 tab 时激活相邻 tab（后一个滑入，末尾则退一个）。
    fn close_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix >= self.tabs.len() {
            return;
        }
        drop(self.tabs.remove(ix));
        if self.tabs.is_empty() {
            self.active = 0;
        } else {
            if ix < self.active {
                self.active -= 1;
            }
            if self.active >= self.tabs.len() {
                self.active = self.tabs.len() - 1;
            }
        }
        cx.notify();
    }

    /// 聚焦当前激活 tab 的终端视图（新建/切换/重新展开面板时调用）
    pub(crate) fn focus_active(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.tabs.get(self.active) {
            let handle = view.read(cx).focus_handle.clone();
            handle.focus(window, cx);
        }
    }

    /// 单个 tab：终端图标 + shell 名（进程退出后追加「（已退出）」）+ × 关闭钮。
    /// 样式学 right_panel.rs 的 render_right_tab。
    fn render_tab(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let view = &self.tabs[ix];
        let id = view.entity_id().as_u64();
        let (label, exited) = {
            let v = view.read(cx);
            (v.shell_name().to_string(), v.exited())
        };
        let label = if exited {
            format!("{label}（已退出）")
        } else {
            label
        };
        let active = ix == self.active;

        h_flex()
            .id(format!("terminal-tab-{id}"))
            .gap_2()
            .pl_3()
            .pr_1()
            // 固定 24px 高（标签栏 30px，上下各留 3px 空隙，不对齐栏边缘）
            .h(px(24.))
            .items_center()
            .rounded(cx.theme().radius)
            .cursor_pointer()
            .when(active, |this| this.bg(cx.theme().muted))
            .when(!active, |this| {
                this.text_color(cx.theme().muted_foreground)
                    .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
            })
            .child(
                Icon::new(IconName::SquareTerminal)
                    .size_3p5()
                    .into_any_element(),
            )
            .child(div().text_sm().child(label))
            .child(
                div()
                    .id(format!("terminal-tab-close-{id}"))
                    .p(px(1.))
                    .rounded(cx.theme().radius)
                    .hover(|this| this.bg(cx.theme().accent.opacity(0.6)))
                    .child(
                        Icon::new(IconName::Close)
                            .size_3()
                            .text_color(cx.theme().muted_foreground),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.close_tab(ix, cx);
                    })),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                this.active = ix;
                this.focus_active(window, cx);
                cx.notify();
            }))
            .into_any_element()
    }

    /// 标签页栏：tab 列表 + 末尾「+」新建与折叠钮（参照 render_right_tab_bar）
    fn render_tab_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .w_full()
            .flex_shrink_0()
            .h(px(30.))
            // 子项垂直居中：tab 不拉伸满栏高（上下留空隙）
            .items_center()
            .pl_2()
            .pr_1()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .children((0..self.tabs.len()).map(|ix| self.render_tab(ix, cx)))
            .child(div().flex_1())
            .child(
                Button::new("terminal-tab-add")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Plus)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_tab(window, cx);
                    })),
            )
            .child(
                Button::new("terminal-panel-collapse")
                    .ghost()
                    .xsmall()
                    .icon(IconName::ChevronDown)
                    .on_click(cx.listener(|_this, _, _, cx| {
                        cx.emit(TerminalPanelEvent::RequestCollapse);
                    })),
            )
    }

    /// 空态：全部 tab 关闭后显示「无终端」+ 新建按钮，不自动重开
    fn render_empty(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .flex_1()
            .min_h_0()
            .items_center()
            .justify_center()
            .gap_2()
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("无终端"),
            )
            .child(
                Button::new("terminal-empty-new")
                    .outline()
                    .xsmall()
                    .label("新建终端")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_tab(window, cx);
                    })),
            )
    }
}

impl Render for TerminalPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content: AnyElement = match self.tabs.get(self.active) {
            Some(view) => div()
                .flex_1()
                .min_h_0()
                .child(view.clone())
                .into_any_element(),
            None => self.render_empty(cx).into_any_element(),
        };
        v_flex()
            .size_full()
            .child(self.render_tab_bar(cx))
            .child(content)
    }
}
