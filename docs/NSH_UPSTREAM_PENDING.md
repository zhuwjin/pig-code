# NSH 新版本待办

当前项目的 `nsh-core`、`nsh-base`、`nsh-engine` 和 `nsh-testing` 均使用 `0.3.0`（见 `Cargo.toml`）。

本文只记录依赖 NSH 新版本的事项，不记录 Pig Code 自己的功能 TODO。NSH main 中存在某项实现，不代表它已经发布，也不代表可以直接替换当前版本；升级时必须重新检查公开 API 和兼容性。

## 已决定等待

### 1. 工具卡片和工作详情的逐步展开/收起

**现状**

当前 `Collapsible` 在 `open` 变化时一次性挂载或卸载内容。快速展开时，完整内容会在容器动画结束前显示；`LayoutAnimation` 只改变布局位置，不能逐步裁剪内容。

已决定暂不自建 `Reveal` 组件，等待 NSH 正式发布 `Collapsible.reveal`。

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

`docs/PLUGIN_SYSTEM.md` 的“资源预算、限制和隔离”部分（第 411 行附近）。

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

0.3.0 的 `nsh/base` 没有导出 `Link`，也没有导出 `Select`：

- 设置页“获取密钥”使用 `Button` + `openUrl` 模拟链接外观，见 `app/components/settings/provider-detail.tsx`。
- API 协议选择使用 `Popover` + `Button` 实现，见同一文件的格式选择器。

两处当前功能正常，不为此单独升级。

**升级后的处理**

- 新版提供 `Link` 后，用官方组件替换“获取密钥”的 `Button` + `openUrl`，保留下划线文字和外链图标。
- 新版提供 `Select` 后，评估是否替换格式选择器；替换时保持现有外观、键盘操作和选择后立即保存的行为。

**验收**

- “获取密钥”由系统打开 `provider.key_url`，不改变页面状态。
- 格式选择器的触发器宽度、长标签显示、键盘操作和保存时机与当前一致。

## 不属于 NSH 新版本待办

- 设置页供应商表单问题属于项目自身的页面或测试问题。
- 终端把 Bash 的 `printf`、`seq`、`for ... do` 交给 PowerShell 执行，属于项目自身的 shell 选择问题。
- 当前的虚拟列表、消息行身份和行高修复不依赖等待新版本。

## 升级检查

正式升级 NSH 时，先处理公开 API 的兼容变化，再运行：

```sh
nsh check && nsh test
```

另外用真实长会话检查流式输出、展开/收起、上翻和“最新”按钮，并检查 Windows 实际渲染，不能只依据类型检查或静态截图验收。
