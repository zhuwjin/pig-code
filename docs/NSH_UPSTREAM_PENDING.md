# NSH 新版本待办

当前项目的 `nsh-core`、`nsh-base`、`nsh-engine` 和 `nsh-testing` 均使用 `0.5.0`（见 `Cargo.toml`）。

本文只记录尚未完成的、依赖 NSH 新版本的事项，不记录 Pig Code 自己的功能 TODO，也不记录已经接入的历史。已完成的历史记录见 [NSH_UPSTREAM_HISTORY.md](./NSH_UPSTREAM_HISTORY.md)。NSH main 中存在某项实现，不代表它已经发布，也不代表可以直接替换当前版本；升级时必须重新检查公开 API 和兼容性。

## 插件引擎

### 1. 插件 Engine 的真实内存与 CPU 用量

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

## 平台问题

### 2. macOS 原生 `Input` 滚动后的短暂空白

**现状**

NSH 0.3.0 的 macOS 后端在滚动时先通过 `sublayerTransform` 移动图层，停止后再同步 AppKit 的真实视图位置。最后一次滚动后的提交延迟为 140ms；显示链路仍忙时会继续重试，并允许等到静止后的 1000ms。相关实现位于 `crates/nsh-platform-macos/src/scroll.rs` 和 `view.rs`。

实际界面中，单行和多行原生 `Input` 的文字会在快速滚出并返回视口后短暂空白，边框和滚动条仍正常，输入值没有丢失。普通单行搜索框也能复现，因此不能把原因归结为多行 JSON 输入框未限制高度。`Scroll`、`List.header` 和可回收的普通 `List` 行均有此现象；这不是应用额外添加的 CSS 动画。

模型编辑弹窗的多行 JSON 输入框仍应单独补上固定或最大高度，这是布局改进，不能作为本问题的根治方式。

**复现**

`examples/repros/` 保留了依据官方 API 编写的独立场景 `InputScrollRepro.tsx`（不是官方示例原文件）：

```sh
nsh run examples/repros/InputScrollRepro.tsx --target macos --window-size 900x700
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

### 3. VT 把 `CSI < u` 误判为恢复光标

**现状**

NSH 0.5.0 的 `crates/nsh-base/src/term/vt.rs` 在 `csi_dispatch` 中把 `'u'` 直接交给 `restore()`，没有检查 `intermediates`。Claude Code 等 TUI 会发送 `ESC[<u`（kitty keyboard protocol 查询）；模拟器把 `CSI < u` 当成 `CSI u`（恢复光标），在 TUI 退出时把光标恢复到较早保存的位置，PowerShell 随后把新提示符打印到 TUI 内容上方。

**复现**

在终端运行 `claude`，等待安全确认界面出现后按 Esc/选择 No 退出。提示符会出现在 Claude 界面上方。抓取原始 PTY 字节并直接喂给 `VtEmulator`，可见退出序列里的 `ESC[<u` 使光标从第 19 行跳到第 2 行；过滤该序列后，光标停在 TUI 下方第 18 行。

**上游修复**

`'u'` 和 `'s'` 都应只在 `intermediates.is_empty()` 时分别恢复/保存光标，例如：

```rust
'u' if !private && intermediates.is_empty() => self.restore(),
's' if !private && intermediates.is_empty() => self.save(),
```

**当前规避**

`src/terminal.rs` 的 `filter_kitty_keyboard` 在 PTY 输出进入官方 Terminal 前丢弃带 `<`、`>` 或 `=` 中间字节、以 `u` 结尾的 CSI 序列，并用 `filter_carry` 处理跨读取块的拆分。`terminal::tests` 覆盖完整序列、拆分序列和真正的 `CSI u` 恢复光标。上游修复后应删除此过滤。

### 4. 分批挂载导致嵌套虚拟列表出现空白

**现状**

活动明细改为最高 420 点的 `Scroll` 内嵌 `List viewport="parent"` 后，Windows 实际界面中静态展开包含 13 个工具记录的活动，曾只绘制前 7 项，剩余区域空白。JSONL 中的数据完整，不依赖流式追加。使用同一主会话和内容宽度进行自动化截图尚未稳定复现，因此当前现象与下述上游问题高度吻合，但尚未完全确认。

**上游参考**

NSH 0.5.0 的 [longbridge/nsh#1783](https://github.com/longbridge/nsh/issues/1783) 记录了 view pacing 的缺陷：一轮已创建至少 64 个视图后，`Scroll` 内虚拟 List 的被延迟单元格可能已经创建，却始终收不到 `Frame`，直到再次布局才恢复。节点仍可被自动化查询，因此 `shows()` / `waitFor()` 通过不代表实际绘制正常；继续滚动也可能触发布局并掩盖问题。

**升级后的处理**

- 检查该 issue 的修复是否已进入当前使用的 NSH 版本，并重新验证嵌套虚拟列表。
- 使用 `nsh run --target windows` 和真实静态会话展开工作区与活动，检查所有可见行均实际绘制，内部滚动可以到达最后一项且底部无异常空白。
- 不只依赖节点存在性断言或滚动后的截图；需要覆盖首次展开时的真实渲染。

## 升级检查

后续升级 NSH 时，先处理公开 API 的兼容变化，再运行：

```sh
nsh check && nsh test
```

另外用真实长会话检查流式输出、展开/收起、上翻和“最新消息”按钮；用 `App.stream-scroll.fixture.tsx` 复验上滚不被拉回。检查 Windows 实际渲染，不能只依据类型检查或静态截图验收。
