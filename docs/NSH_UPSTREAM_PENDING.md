# NSH 新版本待办

当前项目的 `nsh-core`、`nsh-base`、`nsh-engine` 和 `nsh-testing` 均使用 `0.4.0`（见 `Cargo.toml`）。

本文只记录依赖 NSH 新版本的事项，不记录 Pig Code 自己的功能 TODO。NSH main 中存在某项实现，不代表它已经发布，也不代表可以直接替换当前版本；升级时必须重新检查公开 API 和兼容性。

## 已发布，已接入

### 1. 工具卡片和工作详情的展开/收起（已接入）

**当前实现**

消息虚拟 List 内的外层工作区、思考、工具输出、AgentSwarm 和 Plan 统一采用 NSH 0.4.0 Gallery Chat 的官方方式：`app/ui-context.ts` 在切换展开状态前调用 `LayoutAnimation.configureNext(LayoutAnimation.Presets.easeInEaseOut)`，各组件用响应式 `If` 挂载或卸载内容。自研的 `ListCollapsible`、逐帧高度采样、测量缓存、动画计数和滚动补偿已删除。活动分组的展开状态仍通过 `computed` 按 ID 隔离，避免其他卡片开关重置当前分组。Hook 汇总仍直接挂载/卸载。

正文之间连续两项及以上的 `reasoning` / `tool` 由 `groupConversationActivity` 生成 `activity` 行并默认折叠，单项不重复分组，展开后仍使用原有行组件；大活动分组每帧最多挂载或卸载 4 项，其余位置保留 28px 占位，直到全部展开或折叠完成，避免单帧挂载整组卡片。已完成回合的工作详情内部使用同一规则。分组 ID 取首项 ID，流式追加工具不会重置展开状态。汇总按搜索、查文件、命令、网络搜索、思考和普通工具分类计数并标记各类失败数，中英文文案分别维护；会话内搜索同时匹配分组内容。`App.activity.test.tsx` 覆盖默认折叠、展开/收起、追加工具后的状态保持和中英文汇总，`App.activity-large.fixture.tsx` 另覆盖 80 项内容的完整展开与交互。

Agent 卡片由 `SubagentActivity` 分配完成序号，Swarm 按完成优先及序号排序，展示父卡片和行号；等待审批、失败和完成状态分别显示。Plan 状态使用主题 `warning` / `success` / `destructive`；Write/Edit 显示文件名和目录；Read 路径支持复制与右侧打开；失败叉号通过官方 `Tooltip` 展示原因。工具输入/执行、正文和不同 ID 的思考开始时会结束旧思考的流式状态，已结束条目不会被后续增量重新开启。思考预览按非空行号切换：同一行追加只刷新文本，新行使用原生 `Animated` 纵向切换 300ms，两次切换至少间隔 800ms；快速更新保留有界队列，延迟时跳过过期中间行，展开/收起重置到最新行，减少动态效果时直接切换。预览按内容宽度排列并允许收缩，分隔点与文本共用 20px 行高，横向尾部跟随保留，箭头在左侧跟随内容；macOS 横向系统指示器由隐藏的内置 `Scrollbar` 主题替代，不显示横向滚动条。长命令、路径和活动标题使用 `min-w-0 shrink` 收缩，避免撑宽消息列表。共享 `app/components/shimmer-text.tsx` 用位置固定的双层文字和反向位移高亮层实现扫光，原生循环动画不逐帧运行 JS，文字更新不重启；根节点只暴露一个语义标签，减少动态效果或停止活动时卸载高亮。思考和运行中工具标签已接入，`App.shimmer-text.test.tsx` 覆盖语义去重、活动状态和减少动态效果，真实 macOS 帧截图与像素对比验证光带移动。历史回放的 `TurnComplete.duration_ms = 0` 只作为回放标记，工作行优先读取 `stats.duration_ms`；无统计信息时显示“处理完成”，不伪造 1 秒。每段思考独立记录 `startedAtMs` 和 `durationMs`，在后续工具、正文、新思考或回合结束时收尾，不再使用整个回合的耗时；回放没有分段计时时显示“思考过程”。`App.reasoning-duration.test.tsx` 覆盖多段思考与回合总耗时的独立计算，`App.replay-duration.test.tsx` 覆盖真实回放耗时和未知耗时，`App.conversation.test.tsx` 覆盖工作行耗时解析。`App.reasoning.test.tsx` 覆盖节奏队列、状态边界、可见标签、预览点击展开和收起恢复；`App.tool-cards.test.tsx` 覆盖工具交互及完成序号分配。

右侧子 Agent 输出（`app/components/right-panel.tsx`）复用主面板的工具行，因此也采用相同的 `LayoutAnimation + If`。模型高级设置（`app/components/settings/model-editor.tsx`）和侧栏分组不在消息 List 的虚拟行内，继续使用官方 `Collapsible.reveal`。折叠动画和减少动态效果均由官方组件或原生布局动画处理，不再维护应用侧的高度动画。

代码正文使用共享的 `app/components/code-view.tsx`，不再为 Read、命令和输出创建完整 `Editor`。组件用虚拟 List 只创建可见代码行，文本立即显示；`src/code_highlight.rs` 在后台线程使用与 NSH 相同的 `syntect` / `two-face` 解析高亮，并以 UTF-16 偏移原位着色。Read 保留真实行号，命令和输出不带行号；命令区按内容增高、上限 160，读取和输出上限 320，不保留固定空高或末尾滚动空白。非换行模式由外层原生纵向 Scroll 提供完整滚动范围，行号和正文只随原生纵向偏移移动，不再手动 `translateY`；独立横向 Scroll 处理长行，官方 `Scrollbar` 固定在当前视口底部。文本只创建视野及预取行，不同步两个纵向滚动视图，也不使用 `sticky`。横向条的厚度、颜色、最短滑块和淡出时间与 NSH 原生指示器一致；纵向滚动条区域在 Scroll 内使用默认箭头命中层，避免 macOS 将下层文本的 I-beam 用于滚动条，同时保留原生拖动。代码行使用文本光标，滚动条覆盖在文本上但保持默认箭头，两个滚动条均固定在卡片可视区边缘。两种换行模式的代码正文都放在共享的 `selectable` View 中，支持跨行拖选和复制，行号栏不参与选区。`App.code-view.test.tsx` 覆盖长输出虚拟化、滚到末行、长行横纵组合滚动及命令/输出的展开和换行；真实 macOS 窗口另验证两种模式的多行选区。

**验收**

- 长工作区和长工具输出展开/收起时不出现空白、重叠、正文从顶部下闪或滚动位置异常。
- 快速连续展开/收起不会遗留不可见但可点击的内容。
- 虚拟列表、搜索定位和上翻阅读行为不受影响。
- 右侧面板和设置页的 `reveal` 保持原有效果。

`App.scroll.test.tsx` 和 `App.collapse.fixture.tsx` 覆盖长工作区、内层工具、长思考内容和快速连续切换；`App.activity.test.tsx` 验证 80 项活动的完整展开与交互，`App.reasoning.test.tsx` 验证思考内容的展开/收起，`App.tool-cards.test.tsx` 验证工具、Swarm 和 Plan。测试使用隔离 Store，不读取真实会话或钥匙串，不再要求应用报告中间高度或动画生命周期。

**上游参考**

严格依据 NSH `v0.4.0` tag（`06771fff7150e9ae115a0edde2ab764ec4e8454d`）的 `examples/gallery/demos/Chat.tsx` 接入：状态切换前配置全局 `LayoutAnimation`，内容使用响应式 `If`，滚动由原生 List 保持。此前复现中的行内容消失来自示例在可回收行函数中用普通 `if` 返回不同结构，且测试误把 `shows()` 当作窗口可见性；改为响应式 `If` 并通过 `onViewableItemsChange` 验证后，长滚动场景通过，不再据此判断 NSH 的 List 回收或官方动画存在缺陷。`examples/collapsible-reveal/main.tsx` 和 `examples/collapsible-reveal/repro/` 保留独立行为与分步截图验证，步骤见 `examples/collapsible-reveal/ISSUE.md`。右侧非虚拟消息行场景继续使用官方 `Collapsible.reveal`。当前没有需要上游补充的中间高度接口。

## 滚动与列表能力

### 2. 用原生 List 底部跟随替代手动跟随（已接入）

**当前实现**

`app/components/messages.tsx` 保留虚拟 `List`、稳定消息行身份和 `estimatedItemSize={28}`，始终使用 `maintainVisibleContentPosition={{ autoscrollToBottomThreshold: 120 }}`。列表在底部或接近底部时自动跟随内容增长和视口变化；用户上翻后由原生 List 保留可见位置并显示“最新消息”，展开和收起不再切换该配置，也不运行应用侧的逐帧滚动补偿。`latestVisible` 只根据实际距底位置控制按钮，只有切换 Session 时保留一次主动滚底。

`app/conversation.ts` 的 `reuseConversationRows` 按行 ID 复用未变化的数据对象，Session 切换时清空缓存。工具参数和输出等流式更新只替换自己的行，避免 NSH List 将其他行重置为未测量状态而导致滚动条抖动。

没有使用 `inverted`，短会话仍从顶部排列，长会话首屏由原生定位到最新内容。搜索定位和回合导航使用 `scrollToIndex`；导航条只在缩放、透明度目标或减少动态效果偏好实际变化时更新；滚动中的高亮直接更新，弹簧动画只用于 hover 聚焦和退出，不为每个未变化条目重复发送同目标弹簧动画。“最新消息”参考 noop 使用全宽 `box-none` 容器居中按钮，不对按钮自身做位移，避免视觉位置和点击命中区域分离；只要不在底部就出现。按钮使用 `hover:bg-accent` 提供悬停高亮，外层隐藏的可滚动承载层将滚轮增量转发给消息 List，内部 sticky 按钮保持视觉位置，滚轮穿透不影响点击。点击时不提前改变按钮状态；距底超过一个视口时先无动画定位到底部附近，再延迟 40ms 调用 `scrollToEnd({ animated: true })` 完成末段动画，避免动画穿越全部历史并保留近距离的原有滚动效果。按钮在滚动过程中保持显示，只有实际距底不超过 1px 时才隐藏，避免点击瞬间先消失再出现。

**验证**

`App.scroll.fixture.tsx`、`App.scroll-expand.fixture.tsx` 和 `App.scroll-short.fixture.tsx` 在 selftest 隔离环境中覆盖：长历史首屏与虚拟化、上翻后新消息不抢滚动、在 120px 跟随阈值内上翻后展开工作区不抢滚动、底部展开和收起后由原生 List 保持底部、点击“最新”恢复跟随、流式正文更新，以及短会话顶部排列。展开后立即收起用例验证最终展开状态和回答可见性，不再检查逐帧偏移、超调或中间高度。另有按钮状态序列测试，检查点击“最新消息”后在非底部不会隐藏，只有到达底部才消失；滚轮用例验证悬浮按钮上的滚动会移动消息 List，点击仍能回到底部，Windows 截图验证悬停高亮。测试不访问真实钥匙串。

`App.conversation.test.tsx` 验证工具输入更新时保留其他行的对象身份。`examples/collapsible-reveal/repro/` 另以原生可视行事件和真实 Windows 分步截图验证长滚动后的官方展开行为。

### 3. 插件 Engine 的真实内存与 CPU 用量

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

### 4. `uiTheme` 的公开 TypeScript 声明

**现状**

`app/view.tsx` 通过 `uiTheme` 应用动态主题，运行时可用，但当前公开声明没有该属性，因此保留了局部 `@ts-expect-error`。

NSH main 的 `crates/nsh-core/src/ops.rs` 仍将 `UiTheme` 标为 `internal`，不能仅凭 main 就删除该类型豁免。

**升级后的处理**

等待公开且稳定的根主题 API 或 TypeScript 声明后，移除局部豁免并检查动态主题切换。无需仅为这项声明问题升级。

## 已接入的组件导出

### 5. `nsh/base` 的 `Link`、`Select` 及其他正式组件

**当前实现**

- “获取密钥”已在 `app/components/settings/provider-detail.tsx` 改用官方 `Link`，保留下划线文字、外链图标和系统打开行为。
- API 协议、工作区、语言、字体和插件枚举配置已改用官方 `Select`；字体使用官方搜索与清空能力，插件枚举保留原始 JSON 值类型。
- 模型上下文窗口和最大输出使用 `NumberInput`；模型设置页签使用 `Tabs`/`Tab`，默认推理等级使用 `RadioGroup`/`Radio`。
- 模型编辑、Provider 预设和图标选择弹窗使用 `Dialog`；插件布尔配置使用 `Switch`。
- 权限和模型选择等包含额外操作的复合 Popover 保持现有实现，没有为了换组件而拆分产品流程。

### 6. macOS 原生 `Input` 滚动后的短暂空白

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

### 7. Windows 标题栏按钮自绘（未发布）

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

## 不属于 NSH 新版本待办

- 模型编辑弹窗的首次回显延迟和复选框缺少选中指示属于项目自身的初始化与样式问题；原生 `Input` 的滚动空白见第 6 节，不再笼统归为项目自身问题。
- 终端把 Bash 的 `printf`、`seq`、`for ... do` 交给 PowerShell 执行，属于项目自身的 shell 选择问题。
- 当前的虚拟列表、消息行身份和行高修复不依赖等待新版本。

## 升级检查

正式升级 NSH 时，先处理公开 API 的兼容变化，再运行：

```sh
nsh check && nsh test
```

另外用真实长会话检查流式输出、展开/收起、上翻和“最新”按钮，并检查 Windows 实际渲染，不能只依据类型检查或静态截图验收。
