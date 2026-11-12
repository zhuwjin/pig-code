# NSH 新版本历史记录

本文记录已经从 NSH 新版本接入、或曾经作为待办但已经解决的事项，只作历史记录，不再维护为待办。当前未完成事项见 [NSH_UPSTREAM_PENDING.md](./NSH_UPSTREAM_PENDING.md)。

## 消息折叠与工具输出

### 1. 工具卡片和工作详情的展开/收起（已完成）

**最终实现**

消息虚拟 List 内的折叠统一使用 `nsh/base` 的官方 `Collapsible`：`WorkRow`（工作了 N 秒）、`ReasoningRow`（思考过程）、`ActivityRow`（活动分组）、`ToolRow`（工具输出）、`PlanCard`、`AgentSwarm` 和回合结束的 hooks 面板使用 `Collapsible open reveal`，内容在收起动画结束后从树中移除。点击直接调用 Store 的展开方法，不再叠加全局 `LayoutAnimation`。右侧子 Agent 输出复用同一套行组件。

`nsh 0.4.0` 的 Gallery Chat 曾先把这些位置改为 `LayoutAnimation.configureNext(easeInEaseOut)` + 响应式 `If`；后来为统一手感并解决「文字先显示、容器再动画」的问题，改回官方的 `Collapsible.reveal`。自研的 `ListCollapsible`、逐帧高度采样、测量缓存、动画计数和滚动补偿已删除。

用户长消息仍保留 `max-h-28` 像素裁剪，不拆分文本：`toggleUserExpanded` 使用 265ms、`springDamping: 1.0` 的布局动画，裁剪容器始终保留 `overflow-hidden`，展开/收起按钮保持同一节点，避免按钮被重建后先行跳位。

活动分组的展开状态仍通过 `computed` 按 ID 隔离，避免其他卡片开关重置当前分组；大活动分组每帧最多挂载或卸载 4 项，其余位置保留 28px 占位，直到全部展开或折叠完成。分组 ID 取首项 ID，流式追加工具不会重置展开状态。汇总按搜索、查文件、命令、网络搜索、思考和普通工具分类计数并标记各类失败数，中英文文案分别维护；会话内搜索同时匹配分组内容。`App.activity.test.tsx` 覆盖默认折叠、展开/收起、追加工具后的状态保持和中英文汇总，`App.activity-large.fixture.tsx` 另覆盖 80 项内容的完整展开与交互。

Agent 卡片由 `SubagentActivity` 分配完成序号，Swarm 按完成优先及序号排序，展示父卡片和行号；等待审批、失败和完成状态分别显示。Plan 状态使用主题 `warning` / `success` / `destructive`；Write/Edit 显示文件名和目录；Read 路径支持复制与右侧打开；失败叉号通过官方 `Tooltip` 展示原因。工具输入/执行、正文和不同 ID 的思考开始时会结束旧思考的流式状态，已结束条目不会被后续增量重新开启。思考预览按非空行号切换：同一行追加只刷新文本，新行使用原生 `Animated` 纵向切换 300ms，两次切换至少间隔 800ms；快速更新保留有界队列，延迟时跳过过期中间行，展开/收起重置到最新行，减少动态效果时直接切换。预览按内容宽度排列并允许收缩，分隔点与文本共用 20px 行高，横向尾部跟随保留，箭头在左侧跟随内容；macOS 横向系统指示器由隐藏的内置 `Scrollbar` 主题替代，不显示横向滚动条。长命令、路径和活动标题使用 `min-w-0 shrink` 收缩，避免撑宽消息列表。共享 `app/components/shimmer-text.tsx` 用位置固定的双层文字和反向位移高亮层实现扫光，原生循环动画不逐帧运行 JS，文字更新不重启；根节点只暴露一个语义标签，减少动态效果或停止活动时卸载高亮。思考和运行中工具标签已接入，`App.shimmer-text.test.tsx` 覆盖语义去重、活动状态和减少动态效果，真实 macOS 帧截图与像素对比验证光带移动。历史回放的 `TurnComplete.duration_ms = 0` 只作为回放标记，工作行优先读取 `stats.duration_ms`；无统计信息时显示“处理完成”，不伪造 1 秒。每段思考独立记录 `startedAtMs` 和 `durationMs`，在后续工具、正文、新思考或回合结束时收尾，不再使用整个回合的耗时；回放没有分段计时时显示“思考过程”。`App.reasoning-duration.test.tsx` 覆盖多段思考与回合总耗时的独立计算，`App.replay-duration.test.tsx` 覆盖真实回放耗时和未知耗时，`App.conversation.test.tsx` 覆盖工作行耗时解析。`App.reasoning.test.tsx` 覆盖节奏队列、状态边界、可见标签、预览点击展开和收起恢复；`App.tool-cards.test.tsx` 覆盖工具交互及完成序号分配。

代码正文使用共享的 `app/components/code-view.tsx`，不再为 Read、命令和输出创建完整 `Editor`。组件用虚拟 List 只创建可见代码行，文本立即显示；`src/code_highlight.rs` 在后台线程使用与 NSH 相同的 `syntect` / `two-face` 解析高亮，并以 UTF-16 偏移原位着色。Read 保留真实行号，命令和输出不带行号；命令区按内容增高、上限 160，读取和输出上限 320，不保留固定空高或末尾滚动空白。非换行模式由外层原生纵向 Scroll 提供完整滚动范围，行号和正文只随原生纵向偏移移动，不再手动 `translateY`；独立横向 Scroll 处理长行，官方 `Scrollbar` 固定在当前视口底部。文本只创建视野及预取行，不同步两个纵向滚动视图，也不使用 `sticky`。横向条的厚度、颜色、最短滑块和淡出时间与 NSH 原生指示器一致；纵向滚动条区域在 Scroll 内使用默认箭头命中层，避免 macOS 将下层文本的 I-beam 用于滚动条，同时保留原生拖动。代码行使用文本光标，滚动条覆盖在文本上但保持默认箭头，两个滚动条均固定在卡片可视区边缘。两种换行模式的代码正文都放在共享的 `selectable` View 中，支持跨行拖选和复制，行号栏不参与选区。`App.code-view.test.tsx` 覆盖长输出虚拟化、滚到末行、长行横纵组合滚动及命令/输出的展开和换行；真实 macOS 窗口另验证两种模式的多行选区。

**验证**

长工作区和长工具输出展开/收起时不出现空白、重叠、正文从顶部下闪或滚动位置异常；快速连续展开/收起不会遗留不可见但可点击的内容；虚拟列表、搜索定位和上翻阅读行为不受影响。`App.scroll.test.tsx` 和 `App.collapse.fixture.tsx` 覆盖长工作区、内层工具、长思考内容和快速连续切换；`App.activity.test.tsx` 验证 80 项活动的完整展开与交互，`App.reasoning.test.tsx` 验证思考内容的展开/收起，`App.tool-cards.test.tsx` 验证工具、Swarm 和 Plan。测试使用隔离 Store，不读取真实会话或钥匙串。

**上游参考**

严格依据 NSH `v0.4.0` tag（`06771fff7150e9ae115a0edde2ab764ec4e8454d`）的 `examples/gallery/demos/Chat.tsx` 接入过 `LayoutAnimation + If`：状态切换前配置全局 `LayoutAnimation`，内容使用响应式 `If`，滚动由原生 List 保持。此前复现中的行内容消失来自示例在可回收行函数中用普通 `if` 返回不同结构，且测试误把 `shows()` 当作窗口可见性；改为响应式 `If` 并通过 `onViewableItemsChange` 验证后，长滚动场景通过。`examples/collapsible-reveal/main.tsx` 和 `examples/collapsible-reveal/repro/` 保留独立行为与分步截图验证，步骤见 `examples/collapsible-reveal/ISSUE.md`。最终消息 List 内使用官方 `Collapsible.reveal`，右侧非虚拟消息行和设置页继续使用同一组件；当前没有需要上游补充的中间高度接口。

## 滚动与列表能力

### 2. 底部跟随的演进（已完成）

**最终实现**

`app/components/messages.tsx` 保留虚拟 `List`、稳定消息行身份和 `estimatedItemSize={28}`，不再使用原生 `maintainVisibleContentPosition`：流式新增行时，平台滚动偏移可能尚未回到核心，原生 `follow_end` 会把用户刚开始的上滚当成仍在底部并重置到末尾，鼠标滚轮和触控板都会出现“被拉回”。跟随改为应用侧延迟执行：`onScroll` 维护距底状态和“最新消息”按钮，距底不超过 1px 时，内容变化后延迟 80ms 调用 `scrollToEnd`；`onScroll` 和 `onScrollDragStart` 会取消待执行的跟随，因此用户开始上滚的同一帧内就能阻止这次跟随。展开和收起仍不切换列表配置，也不运行逐帧滚动补偿。只有切换 Session 时保留一次主动滚底。右侧子 Agent 列表使用同一策略。

`app/conversation.ts` 的 `reuseConversationRows` 按行 ID 复用未变化的数据对象，Session 切换时清空缓存。工具参数和输出等流式更新只替换自己的行，避免 NSH List 将其他行重置为未测量状态而导致滚动条抖动。

没有使用 `inverted`，短会话仍从顶部排列，长会话首屏由原生定位到最新内容。搜索定位和回合导航使用 `scrollToIndex`；导航条只在缩放、透明度目标或减少动态效果偏好实际变化时更新；滚动中的高亮直接更新，弹簧动画只用于 hover 聚焦和退出，不为每个未变化条目重复发送同目标弹簧动画。“最新消息”参考 noop 使用全宽 `box-none` 容器居中按钮，不对按钮自身做位移，避免视觉位置和点击命中区域分离；只要不在底部就出现。按钮使用 `hover:bg-accent` 提供悬停高亮，外层隐藏的可滚动承载层将滚轮增量转发给消息 List，内部 sticky 按钮保持视觉位置，滚轮穿透不影响点击。点击时不提前改变按钮状态；距底超过一个视口时先无动画定位到底部附近，再延迟 40ms 调用 `scrollToEnd({ animated: true })` 完成末段动画，避免动画穿越全部历史并保留近距离的原有滚动效果。按钮在滚动过程中保持显示，只有实际距底不超过 1px 时才隐藏，避免点击瞬间先消失再出现。

**验证**

`App.scroll.fixture.tsx`、`App.scroll-expand.fixture.tsx` 和 `App.scroll-short.fixture.tsx` 在 selftest 隔离环境中覆盖：长历史首屏与虚拟化、上翻后新消息不抢滚动、上翻后展开工作区不抢滚动、底部展开和收起后保持底部、点击“最新”恢复跟随、流式正文更新，以及短会话顶部排列。`App.stream-scroll.fixture.tsx` 让思考、工具和正文按 220ms 持续新增行，测试在该过程中分五次向上滚动并断言仍远离底部，覆盖流式新增行时不被跟随拉回。展开后立即收起用例验证最终展开状态和回答可见性，不再检查逐帧偏移、超调或中间高度。另有按钮状态序列测试，检查点击“最新消息”后在非底部不会隐藏，只有到达底部才消失；滚轮用例验证悬浮按钮上的滚动会移动消息 List，点击仍能回到底部，Windows 截图验证悬停高亮。测试不访问真实钥匙串。

`App.conversation.test.tsx` 验证工具输入更新时保留其他行的对象身份。`examples/collapsible-reveal/repro/` 另以原生可视行事件和真实 Windows 分步截图验证长滚动后的官方展开行为。

**上游参考**

0.4.0 期间曾使用 `maintainVisibleContentPosition={{ autoscrollToBottomThreshold: 120 }}`，后来把阈值降到 0 仍存在用户上滚与流式新增行的时序竞态，最终改为应用侧延迟跟随。0.5.0 已按 Noop 的写法接入 `alignment="bottom"` + `maintainVisibleContentPosition`，见下面的「0.5.0 升级与窗口」。

## 0.5.0 升级与窗口

### 1. 升级到 0.5.0（已完成）

CLI 和 `nsh-core` / `nsh-base` / `nsh-engine` / `nsh-platform-windows` / `nsh-testing` 从 0.4.0 升级到 0.5.0。0.5.0 移除了 `Window.setTitle` / `Window.setTitleBar`，改为渲染时调用 `currentWindow().set({ title, titleBar, frame })`；`nsh check` 和定向测试通过。

### 2. 原生底部跟随（已完成）

主消息列表和右侧子 Agent 列表改为 `alignment="bottom"` + `maintainVisibleContentPosition={{ autoscrollToBottomThreshold: 24 }}`。删除了 80ms 延迟跟随、`cancelFollow`、`onScrollDragStart` 取消和 Session 切换主动滚底；`onScroll` 只保留“最新消息”按钮状态。

折叠展开时通过 `followSuspended` 临时关闭原生跟随，直到用户滚回距底 24 点以内再恢复，避免在底部展开 activity 后被钉回末尾、刚展开的内容被顶出视口。展开新增 3 行以上且处于暂停状态时显示“最新消息”按钮，小折叠保持底部状态。`App.scroll.test.tsx` 9/9、`App.stream-scroll.test.tsx` 1/1、1000 项活动的虚拟化和完整展开、`App.conversation.test.tsx` 20/20、`App.tool-cards.test.tsx` 5/5 通过；1000 项测试先通过搜索跳到用户消息再展开，匹配真实操作。

`App.follow-matrix.test.tsx` 另覆盖 7 种位置与展开/收起组合：底部展开 work/activity、距底 10px 展开、距底 200px 展开、底部收起长折叠到短内容、短内容再展开、距底 10px 收起夹紧回底部、底部收起后内容仍高于视口。

### 3. Windows 无边框标题栏（已完成）

Windows 上设置 `frame: "none"`，标题栏右上角自绘最小化、最大化/还原、关闭按钮，按钮标记 `desktop={{ windowControl }}`，由平台原生处理点击和 Windows 11 贴靠布局；主标题栏和右侧面板头部预留 138pt。窗口四周和四角增加 `windowResize` 边缘（5px / 12px）。真实 Windows 窗口验证了最大化/还原和右边缘缩放（1372 → 1452 物理像素）。

### 4. 终端改用官方 Terminal（0.5.0）

`app/components/terminal-panel.tsx` 改用 `nsh/base` 的 `Terminal`；官方组件负责 VT 仿真、回滚、键盘、光标、ANSI 颜色和文本选择。Rust `src/terminal.rs` 只保留 `portable-pty` 的进程启动、UTF-8 输出缓冲、`write` / `resize` / `read` / `exit`，删除 `alacritty_terminal` 依赖、ANSI 色表、Canvas 绘制、自绘选区和 80ms 快照。面板保留 tab、错误提示、折叠和已退出标记；隐藏 tab 仍挂载以保留回滚，PTY 在面板不可见时忽略 1×1 的 resize。输出经过 `filter_kitty_keyboard` 规避 NSH 0.5.0 把 `CSI < u` 误判为恢复光标的问题（见待办）。

### 5. 主题应用改用公开 setTheme（0.5.0）

`app/view.tsx` 删除私有 `uiTheme` 和 `@ts-expect-error`，改为在 effect 中调用公开的 `setTheme()`；`app/themes.ts` 的 `serializedTheme()` 继续生成 `cssVars`，插件主题和 style pack 逻辑不变。`docs/NSH_UPSTREAM_PENDING.md` 中该项待办已移除。

### 6. CodeView 原生离屏创建评估（未采用）

`app/components/code-view.tsx` 的非换行模式尝试删除按 `scrollY` 计算 `visible()` 和 `spacers()` 的手工虚拟化，改为对全部行使用 `Each`。验证发现该模式是纵向 `Scroll` 内嵌横向 `Scroll`，0.5 的离屏创建按最近的横向视口判断，160 行会全部创建，`App.code-view.test.tsx` 的虚拟化断言失败。已保留手工虚拟化；如果后续 NSH 支持跨嵌套滚动视口的纵向剔除，再重新评估。

### 7. 消息行复用减法（已完成）

消息列表和右侧子 Agent 列表删除 `reuseConversationRows()` 及其缓存数组，依赖 0.5 `keyExtractor` diff 在行对象重建时保留行身份和滚动位置。`App.conversation.test.tsx` 中针对该函数的单元测试已移除。

## 已接入的组件导出

### 3. `nsh/base` 的 `Link`、`Select` 及其他正式组件（已完成）

- “获取密钥”已在 `app/components/settings/provider-detail.tsx` 改用官方 `Link`，保留下划线文字、外链图标和系统打开行为。
- API 协议、工作区、语言、字体和插件枚举配置已改用官方 `Select`；字体使用官方搜索与清空能力，插件枚举保留原始 JSON 值类型。
- 模型上下文窗口和最大输出使用 `NumberInput`；模型设置页签使用 `Tabs`/`Tab`，默认推理等级使用 `RadioGroup`/`Radio`。
- 模型编辑、Provider 预设和图标选择弹窗使用 `Dialog`；插件布尔配置使用 `Switch`。
- 权限和模型选择等包含额外操作的复合 Popover 保持现有实现，没有为了换组件而拆分产品流程。

## 不属于 NSH 新版本待办的备注

- 模型编辑弹窗的首次回显延迟和复选框缺少选中指示属于项目自身的初始化与样式问题；原生 `Input` 的滚动空白属于 NSH 平台问题，见待办文档。
- 终端把 Bash 的 `printf`、`seq`、`for ... do` 交给 PowerShell 执行，属于项目自身的 shell 选择问题。
- 当前的虚拟列表、消息行身份和行高修复不依赖等待新版本。
