# NSH 新版本待办

当前项目的 `nsh-core`、`nsh-base`、`nsh-engine` 和 `nsh-testing` 均使用 `0.4.0`（见 `Cargo.toml`）。

本文只记录依赖 NSH 新版本的事项，不记录 Pig Code 自己的功能 TODO。NSH main 中存在某项实现，不代表它已经发布，也不代表可以直接替换当前版本；升级时必须重新检查公开 API 和兼容性。

## 已发布，待接入

### 1. 工具卡片和工作详情的展开/收起（按场景接入）

**当前实现**

消息虚拟 List 内的外层工作区、思考、工具输出、AgentSwarm 和 Plan 使用 `app/components/list-collapsible.tsx`。折叠窗口先通过 `onLayout` 测量真实内容高度，再以 20ms 为间隔、最多 14 步更新窗口高度；窗口和 List 使用同一份实际布局高度，不再分别运行 `LayoutAnimation` 和 List 行定位。首次已经展开的内容直接采用测量高度，不重放入场动画；嵌套卡片的测量变化同步给外层窗口，收起完成后卸载内容。Hook 汇总仍直接挂载/卸载。

正文之间连续两项及以上的 `reasoning` / `tool` 由 `groupConversationActivity` 生成 `activity` 行并默认折叠，单项不重复分组，展开后仍使用原有行组件；已完成回合的工作详情内部使用同一规则。分组 ID 取首项 ID，流式追加工具不会重置展开状态。汇总按搜索、查文件、命令、网络搜索、思考和普通工具分类计数并标记各类失败数，中英文文案分别维护；会话内搜索同时匹配分组内容。`App.activity.test.tsx` 覆盖默认折叠、展开/收起、追加工具后的状态保持和中英文汇总。

这里不使用官方 `Collapsible.reveal` 的局部 `animate_layout`，也不使用全局 `LayoutAnimation.configureNext`，避免原生布局动画与 List 的行高测量、回收和滚动范围更新各自修改位置。右侧子 Agent 输出（`app/components/right-panel.tsx`）和模型高级设置（`app/components/settings/model-editor.tsx`）不在消息 List 的虚拟行内，继续使用官方 `Collapsible.reveal`。减少动态效果时直接完成展开和收起。

代码正文使用共享的 `app/components/code-view.tsx`，不再为 Read、命令和输出创建完整 `Editor`。组件用虚拟 List 只创建可见代码行，文本立即显示；`src/code_highlight.rs` 在后台线程使用与 NSH 相同的 `syntect` / `two-face` 解析高亮，并以 UTF-16 偏移原位着色。Read 保留真实行号，命令和输出不带行号；命令区按内容增高、上限 160，读取和输出上限 320，不保留固定空高或末尾滚动空白。非换行模式由外层原生纵向 Scroll 提供完整滚动范围，行号和正文只随原生纵向偏移移动，不再手动 `translateY`；独立横向 Scroll 处理长行，官方 `Scrollbar` 固定在当前视口底部。文本只创建视野及预取行，不同步两个纵向滚动视图，也不使用 `sticky`。横向条的厚度、颜色、最短滑块和淡出时间与 NSH 原生指示器一致；纵向滚动条区域在 Scroll 内使用默认箭头命中层，避免 macOS 将下层文本的 I-beam 用于滚动条，同时保留原生拖动。代码行使用文本光标，滚动条覆盖在文本上但保持默认箭头，两个滚动条均固定在卡片可视区边缘。两种换行模式的代码正文都放在共享的 `selectable` View 中，支持跨行拖选和复制，行号栏不参与选区。`App.code-view.test.tsx` 覆盖长输出虚拟化、滚到末行、长行横纵组合滚动及命令/输出的展开和换行；真实 macOS 窗口另验证两种模式的多行选区。

**验收**

- 长工作区和长工具输出展开/收起时不出现空白、重叠、正文从顶部下闪或滚动位置异常。
- 快速连续展开/收起不会遗留不可见但可点击的内容。
- 虚拟列表、搜索定位和上翻阅读行为不受影响。
- 右侧面板和设置页的 `reveal` 保持原有效果。

`App.scroll.test.tsx` 和 `App.collapse.fixture.tsx` 覆盖长工作区、内层工具、长思考内容和快速连续切换；`App.list-collapsible.test.tsx` 通过实际 `onLayout` 记录验证中间高度、最多 14 步的节流更新和最终高度。测试使用隔离 Store，不读取真实会话或钥匙串。

**上游参考**

NSH 0.4.0 的 `Collapsible.reveal` 使用局部 `animate_layout`。官方 AI Chat 示例也在虚拟 List 中，但思考区使用直接挂载/卸载和普通 `LayoutAnimation`，没有使用该 `reveal`。待上游协调 List 锚定与行动画后，再评估是否在消息 List 内恢复。

## 滚动与列表能力

### 2. 用原生 List 底部跟随替代手动跟随（已接入）

**当前实现**

`app/components/messages.tsx` 保留虚拟 `List`、稳定消息行身份和 `estimatedItemSize={28}`，新增 `maintainVisibleContentPosition={{ autoscrollToBottomThreshold: 120 }}`。列表在接近底部时自动跟随内容增长和视口变化；用户上翻后保留可见内容，不再于正文每次更新时强制 `scrollToEnd`。只有切换 Session 时保留一次主动滚底。

`app/conversation.ts` 的 `reuseConversationRows` 按行 ID 复用未变化的数据对象，Session 切换时清空缓存。工具参数和输出等流式更新只替换自己的行，避免 NSH List 将其他行重置为未测量状态而导致滚动条抖动。

没有使用 `inverted`，短会话仍从顶部排列，长会话首屏由原生定位到最新内容。搜索定位和回合导航使用 `scrollToIndex`。“最新消息”参考 noop 使用全宽 `box-none` 容器居中按钮，不对按钮自身做位移，避免视觉位置和点击命中区域分离；只要不在底部就出现，点击后延迟 40ms 调用 `scrollToEnd({ animated: true })` 恢复跟随。

**验证**

`App.scroll.fixture.tsx` 和 `App.scroll-short.fixture.tsx` 在 selftest 隔离环境中覆盖：长历史首屏与虚拟化、上翻后新消息不抢滚动、点击“最新”恢复跟随、流式正文更新，以及短会话顶部排列。测试不访问真实钥匙串。

`App.conversation.test.tsx` 验证工具输入更新时保留其他行的对象身份。另用本地真实会话 JSONL 中的 Markdown 和 Edit 输入流，在 macOS 900×700 窗口按记录的约 84 秒 API 间隔回放：基线出现反复的高度重置，修复后 75 次高度采样均未回缩。真实日志和临时复现页不纳入仓库。

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
