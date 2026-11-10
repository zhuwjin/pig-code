# NSH 新版本待办

当前项目的 `nsh-core`、`nsh-base`、`nsh-engine` 和 `nsh-testing` 均使用 `0.4.0`（见 `Cargo.toml`）。

本文只记录尚未完成的、依赖 NSH 新版本的事项，不记录 Pig Code 自己的功能 TODO，也不记录已经接入的历史。已完成的历史记录见 [NSH_UPSTREAM_HISTORY.md](./NSH_UPSTREAM_HISTORY.md)。NSH main 中存在某项实现，不代表它已经发布，也不代表可以直接替换当前版本；升级时必须重新检查公开 API 和兼容性。

## 滚动与列表能力

### 1. 原生 List 的聊天式底部跟随（`alignment="bottom"`，待升级验证）

**现状**

0.4.0 的 List 没有 `alignment` 属性，只有 `inverted` 和 `maintainVisibleContentPosition`。`maintainVisibleContentPosition` / `follow_end` 在流式新增行时存在时序竞态：平台滚动偏移可能尚未回到核心，原生跟随会把用户刚开始的上滚当成仍在底部，并重置到末尾；阈值降到 0 也仍然存在。

当前应用侧采用延迟跟随规避：`onScroll` 维护距底状态，内容变化时若距底不超过 1px，延迟 80ms 调用 `scrollToEnd`；`onScroll` 和 `onScrollDragStart` 取消待执行的跟随。代价是底部跟随时新内容最多晚约 80ms，且不再使用原生 `maintainVisibleContentPosition` 的可见位置锚定。主消息列表和右侧子 Agent 列表都使用这套逻辑，细节见 [NSH_UPSTREAM_HISTORY.md](./NSH_UPSTREAM_HISTORY.md) 的「底部跟随的演进」。

**上游参考**

- NSH main PR #983 `core: Add List alignment to rest a list at its end like a chat`：新增 `alignment?: "top" | "bottom"`。`alignment="bottom"` 保持正常阅读顺序，打开时停在末尾、内容增长时贴住末尾、上滚后停止跟随、回到底部后恢复，短内容贴底；`maintainVisibleContentPosition` / `.follow_end` 只是扩大“多近算在末尾”的阈值。
- NSH main PR #1477 `list: Don't follow the end when a page is prepended above the reader`（修复 #1459）：把保持可见行位置产生的偏移计入 `kept`，避免 prepend 历史时误判为接近末尾。
- noop（`huacnlee/noop`）固定 NSH commit `c5214aaf4bbdf33bc1d59e9595e009b6aa54d702`，主时间线使用 `alignment="bottom"` 和 `maintainVisibleContentPosition={{ autoscrollToBottomThreshold: 24 }}`；`atEnd` 只用于未读计数，不切换该属性（`app/App.tsx` 的 `AT_END` 与 timeline）。
- 以上能力尚未发布：crates.io 上 `nsh-core` / `nsh-base` 最新仍为 0.4.0。

**升级后的处理**

- 升级到包含 #983 / #1477 的版本，先运行 `nsh check` 处理公开 API 变化。
- 尝试把主消息 List 和子 Agent List 改为 `alignment="bottom"`，恢复原生 `maintainVisibleContentPosition`（参考 noop 从 24 开始），删除 80ms 延迟跟随、`cancelFollow` 和相关的 `onScrollDragStart` 取消逻辑；`latestVisible` 只保留给“最新消息”按钮。
- 用一个新测试或现有复现场景验证：思考、工具、正文持续新增行时慢速上滚，视口不能被重置到末尾。现成场景是 `App.stream-scroll.fixture.tsx` / `App.stream-scroll.test.tsx`；在 0.4.0 上把原生跟随加回去即可复现被拉回。
- 确认 `alignment="bottom"` 的短内容贴底行为是否可以接受。Pig Code 目前要求短会话从顶部排列（`App.scroll-short.fixture.tsx` 的 `keeps short conversations at the top`）；如果不能接受，需要只在时间线高于视口时启用底部对齐，或继续保留应用侧跟随。
- 如果原生跟随的竞态仍然存在，保留当前应用侧方案，并把复现和 NSH 版本记录到这里，不要为了换原生而接受视口被拉回。

**验收**

- 流式新增行期间慢速上滚，列表偏移不被重置到末尾；在底部时新内容仍能连续跟随。
- 工作区/思考/工具/活动分组的展开收起不跳位，“最新消息”按钮、搜索定位和回合导航正常。
- 短会话仍然从顶部排列（或明确记录新的行为并更新测试）。
- 右侧子 Agent 列表行为一致。
- 在 headless 和真实 macOS 窗口都验证；不能只依赖类型检查或静态截图。

## 插件引擎

### 2. 插件 Engine 的真实内存与 CPU 用量

**现状**

`docs/PLUGIN_SYSTEM.md` 已明确：NSH 尚未公开按 Engine 查询真实内存与 CPU 的接口。当前资源预算只能作为上限，不能伪装成实际用量。

**升级后的处理**

待 NSH 提供查询接口后，再接入遥测字段，保持现有插件包和协议主版本兼容。

**验收**

- 遥测展示真实测量值，并标明数据来源。
- 无接口或查询失败时不显示虚假的实际用量。
- 资源上限和超时行为保持独立，不因展示用量而改变。

**参考**

`docs/PLUGIN_SYSTEM.md` 的 “Resource budgets and isolation” 部分。

## 非阻塞的声明完善

### 3. `uiTheme` 的公开 TypeScript 声明

**现状**

`app/view.tsx` 通过 `uiTheme` 应用动态主题，运行时可用，但当前公开声明没有该属性，因此保留了局部 `@ts-expect-error`。

NSH main 的 `crates/nsh-core/src/ops.rs` 仍将 `UiTheme` 标为 `internal`，不能仅凭 main 就删除该类型豁免。

**升级后的处理**

等待公开且稳定的根主题 API 或 TypeScript 声明后，移除局部豁免并检查动态主题切换。无需仅为这项声明问题升级。

## 平台问题

### 4. macOS 原生 `Input` 滚动后的短暂空白

**现状**

NSH 0.3.0 的 macOS 后端在滚动时先通过 `sublayerTransform` 移动图层，停止后再同步 AppKit 的真实视图位置。最后一次滚动后的提交延迟为 140ms；显示链路仍忙时会继续重试，并允许等到静止后的 1000ms。相关实现位于 `crates/nsh-platform-macos/src/scroll.rs` 和 `view.rs`。

实际界面中，单行和多行原生 `Input` 的文字会在快速滚出并返回视口后短暂空白，边框和滚动条仍正常，输入值没有丢失。普通单行搜索框也能复现，因此不能把原因归结为多行 JSON 输入框未限制高度。`Scroll`、`List.header` 和可回收的普通 `List` 行均有此现象；这不是应用额外添加的 CSS 动画。

模型编辑弹窗的多行 JSON 输入框仍应单独补上固定或最大高度，这是布局改进，不能作为本问题的根治方式。

**复现**

项目根目录保留了依据官方 API 编写的独立场景 `InputScrollRepro.tsx`（不是官方示例原文件）：

```sh
nsh run InputScrollRepro.tsx --target macos --window-size 900x700
```

在第一个输入框输入 `e`，滚到列表底部，停留几秒后快速滚回顶部，观察文字是否短暂空白。两个输入框都是普通 `List` 行，离屏后会被回收；多行 JSON 输入框固定为官方示例使用的 `height: 110`。

自动化读取已确认：

- 滚动前为 `e` 和 422 个 JSON 字符。
- 普通 `List` 行滚出视口后，两个输入框被回收，读取结果均为 `null`。
- 滚回后输入框重建，恢复为 `e` 和 422 个 JSON 字符。

**上游参考**

2026-11-02 检查 NSH `main@142565666c786e33b1c8dfec339121eea1178dd1`，尚未发现针对此问题的修复：`COMMIT_MS = 140`、`REST_WAIT = 1000`、图层滚动和延迟提交 bounds 的机制均保留。该结论只表示此提交时的代码状态，后续升级必须重新检查并实际运行复现页。

**升级后的处理**

- 检查新版是否修改滚动时的图层变换、bounds 提交时机或原生输入框同步方式。
- 用同一复现页验证 `Scroll` 和可回收的普通 `List` 行，不能只检查数据是否保留，也不能只依赖自动化截图。
- 不通过清空并重写输入值来掩盖问题。

**验收**

- 快速来回滚动及停留后，单行文字和多行 JSON 均不出现短暂空白。
- 输入框被回收和重建时，外层状态及用户正在编辑的内容保持正确。
- 普通表单、外观搜索框和模型编辑弹窗均通过相同场景检查。

## 窗口标题栏

### 5. Windows 标题栏按钮自绘（未发布）

**现状**

Windows 上隐藏系统标题栏后（`Window.setTitleBar({ style: "hidden", height: 44 })`），最小化、最大化和关闭三个按钮仍由 NSH 平台层绘制：`nsh-platform-windows` 的 `chrome.rs::draw` 用 Direct2D 按 Windows 11 样式画在应用内容之上，固定占用窗口右上角 `titleBar().right`（138pt）。点击由 `WM_NCHITTEST` 返回 `HTMINBUTTON` / `HTMAXBUTTON` / `HTCLOSE`，属于非客户区，应用的 View 收不到这些点击，也无法用自绘内容覆盖或替换。

当前应用只能给它们让位：`app/components/title-bar.tsx` 在标题栏贴窗口右边时预留 `titleBar().right`，`app/components/right-panel.tsx` 在右侧面板头部预留同一区域。0.4.0 的 `Window.setTitleBar` 只有 `"default"` 和 `"hidden"`，没有隐藏或替换系统按钮的选项。

**上游参考**

NSH main 已增加完全自绘窗口所需的能力（2026-11-10 检查 `main@643ca53d0cb0ff3d57a02500d2859f62698bb143`）：

- `WindowFrameStyle::None`：无边框窗口，平台不画标题栏、边框和按钮；`Attrs::frameless()` 为真时 `chrome::draw` 直接返回。
- `windowControl` 标记（`WindowControl::{Minimize, Maximize, Close}`）：应用自绘的按钮仍由 `WM_NCHITTEST` 返回对应的系统命中码，最大化按钮保留 Windows 11 贴靠布局。
- `windowResize` 标记以及 `Window.minimize()`、`Window.toggleMaximize()`、`Window.close()` 可以配合使用。

这些能力尚未发布：crates.io 上 `nsh-core` / `nsh-platform-windows` 最新仍为 0.4.0。

**升级后的处理**

- Windows 上把主窗口切换为无边框，自绘最小化、最大化、关闭按钮，使用主题色和 `hover:` / `active:` 样式；按钮标记 `windowControl` 并分别调用 `Window.minimize()`、`Window.toggleMaximize()`、`Window.close()`。
- 保留 `windowDrag` 的空白拖动区和双击最大化行为；移除 Windows 上标题栏和右侧面板头部为系统按钮预留的空白。
- macOS 先保留系统 traffic lights；如果要同样自绘，单独验证键盘、全屏和减少动态效果，不要在一次改动中同时切换所有平台。
- 检查无边框窗口的缩放边框、阴影、圆角、最大化/还原、贴靠布局和全屏行为。

**验收**

- Windows 上三个按钮的图标、悬停、按下以及最大化/还原状态与主题一致，点击分别最小化、最大化或还原、关闭窗口。
- 最大化按钮悬停仍显示 Windows 11 贴靠布局，双击空白标题栏仍最大化/还原。
- 标题栏和右侧面板头部不再为系统按钮留出空白区；窗口缩放边框、阴影和圆角正常。
- macOS 的 traffic lights、拖动和双击行为不回归。

## 升级检查

正式升级 NSH 时，先处理公开 API 的兼容变化，再运行：

```sh
nsh check && nsh test
```

另外用真实长会话检查流式输出、展开/收起、上翻和“最新消息”按钮；第 1 项完成前，还要用 `App.stream-scroll.fixture.tsx` 复验上滚不被拉回。检查 Windows 实际渲染，不能只依据类型检查或静态截图验收。
