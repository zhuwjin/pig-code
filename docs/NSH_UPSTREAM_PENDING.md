# NSH 新版本待办

当前项目的 `nsh-core`、`nsh-base`、`nsh-engine` 和 `nsh-testing` 均使用 `0.4.0`（见 `Cargo.toml`）。

本文只记录依赖 NSH 新版本的事项，不记录 Pig Code 自己的功能 TODO。NSH main 中存在某项实现，不代表它已经发布，也不代表可以直接替换当前版本；升级时必须重新检查公开 API 和兼容性。

## 已发布，待接入

### 1. 工具卡片和工作详情的逐步展开/收起

**现状**

当前 `Collapsible` 在 `open` 变化时一次性挂载或卸载内容。快速展开时，完整内容会在容器动画结束前显示；`LayoutAnimation` 只改变布局位置，不能逐步裁剪内容。

NSH `0.4.0` 已提供 `Collapsible.reveal`，当前项目尚未接入；仍不自建 `Reveal` 组件。

**涉及位置**

- `app/components/tool-card.tsx`：工具详情。
- `app/components/messages.tsx`：思考过程。
- `app/components/turn-rows.tsx`：工作详情和 Hook 汇总。
- `app/store.ts` 的 `toggleExpanded`、`toggleTurnExpanded`：目前配置全局布局动画。

**升级后的处理**

- 在这些区域启用正式的 `reveal`，让可见高度随展开进度增长，并在收起完成后卸载内容。
- 移除这些区域对应的全局布局动画，避免与局部 reveal 叠加；不要影响其他使用布局动画的界面。
- 保留减少动态效果（reduced motion）设置。

**验收**

- 展开第一帧、中间帧和结束帧都不会提前显示完整内容。
- 快速连续展开/收起不会跳变，也不会遗留不可见但可点击的内容。
- 虚拟列表、搜索定位和上翻阅读行为不受影响。

**上游参考**

NSH main 的 `crates/nsh-base/src/collapsible.rs` 已包含 `reveal` 的局部裁剪实现。

## 升级时再评估

### 2. 用原生 List 底部跟随替代手动跟随

**现状**

当前保留虚拟 `List`，通过稳定消息行身份、独立正文缓存和接近实际行高的 `estimatedItemSize={28}` 减少测量与滚动跳动。正文更新时仍由应用手动跟随底部。

在当前版本试用 `maintainVisibleContentPosition` 和 `initialScrollIndex` 时，长历史初始化与流式测量未通过实际回放验证，因此没有采用。

**评估条件**

NSH main 的 `crates/nsh-js/src/list.rs` 调整了消息行与索引的同步方式，但 main 同时包含大量其他 API 变化。不能据此认定它已经修复本项目的滚动问题，也不为此单独做全量升级。

新版本发布后，可用实际长会话重新评估 `maintainVisibleContentPosition`。只有验证通过才替换手动跟随，不恢复 `inverted`。

**验收**

- 短消息从顶部排列。
- 长历史首次打开不出现空白或跳变。
- 流式正文在底部连续跟随，滚动条比例稳定。
- 用户上翻后不强制拉回，点击“最新”后恢复跟随。
- 展开/收起和搜索不要求全量渲染消息。

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

## 非阻塞的组件导出

### 5. `nsh/base` 的 `Link` 与 `Select` 导出

**现状**

NSH `0.4.0` 已导出 `Link` 和 `Select`，当前项目尚未迁移：

- 设置页“获取密钥”使用 `Button` + `openUrl` 模拟链接外观，见 `app/components/settings/provider-detail.tsx`。
- API 协议选择使用 `Popover` + `Button` 实现，见同一文件的格式选择器。

两处当前功能正常，本次依赖升级不扩大为组件替换。

**后续处理**

- 用官方 `Link` 替换“获取密钥”的 `Button` + `openUrl`，保留下划线文字和外链图标。
- 评估是否用 `Select` 替换格式选择器；替换时保持现有外观、键盘操作和选择后立即保存的行为。

**验收**

- “获取密钥”由系统打开 `provider.key_url`，不改变页面状态。
- 格式选择器的触发器宽度、长标签显示、键盘操作和保存时机与当前一致。

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
