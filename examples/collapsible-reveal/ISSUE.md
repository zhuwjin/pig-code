# 虚拟 List 长滚动后展开：内容已创建但不进入可见区域

## 摘要

使用 NSH 0.4.0、未修改 NSH 源码的独立 TSX 应用，可以复现以下差异：

- 普通展开/收起，以及插入 80 行历史后在底部展开，均可工作。
- 加载长历史、向上滚动、定位到工作行再展开时，按钮已变为 `Collapse work`，List 外的 `Outside content` 也已展开，但 List 内的工具项没有正常显示。
- 自动化查询确认 `tool-0` 节点已经创建，等待 5 秒仍未进入可见区域。这不是简单的条件渲染未执行，也不能用截图失败解释。
- List 内使用官方 `LayoutAnimation` 配合 `If` 时可复现；此前将同一内容换成官方 `Collapsible.reveal` 也可复现。因此问题不能仅归因于某一个动画 API，当前证据指向 List 的定位、测量或回收链路，尚未进一步确定具体环节。

本目录不依赖 Pig Code 的生产消息组件。`main.tsx` 的 List 内使用 `If + LayoutAnimation`，List 外保留 `Collapsible.reveal` 作为对照。两种官方动画路径在普通场景中可用，但不能据此认定它们解决了长滚动场景。

## 环境

- NSH 0.4.0。
- 当前复现环境：Windows，60Hz 显示器。
- 从仓库根目录运行命令。
- 不需要启动 Pig Code，也不需要真实会话、模型服务或密钥。

当前环境无法验证真实 120fps，也不能承诺每帧不超过 8.3ms。本复现使用的是官方原生动画路径，没有逐帧自绘高度动画；是否达到 120fps 仍需在 120Hz 设备上用 `--fps` 实测。

## 手动运行

```bash
nsh run examples/collapsible-reveal/main.tsx --target windows --window-size 900x700
```

如需同时观察性能数据：

```bash
nsh run examples/collapsible-reveal/main.tsx --target windows --window-size 900x700 --fps
```

### 场景 A：普通展开和收起

1. 启动后应看到 `Worked for 1s`、`Final answer` 和顶部的 `Expand work`。
2. 点击 `Expand work`。
3. 按钮变为 `Collapse work`，`Worked for 1s` 下出现工具行，List 外出现 `Outside content`。
4. 示例共有 `tool-0` 至 `tool-39` 共 40 个工具项；List 只渲染可视范围，向下滚动可看到后续工具项，`tool-39` 后是 `Final answer`。40 个工具项不会同时挤在一个 900×700 的窗口内。
5. 点击 `Collapse work`，工具行消失，`Final answer` 保留。

### 场景 B：插入历史后在底部展开

1. 重新启动应用，点击 `Load 80 history rows`。
2. List 定位到 `History answer 80` 附近。
3. 点击 `Expand work`。
4. 预期与场景 A 相同：工具内容展开，可以滚动查看至 `tool-39` 和 `Final answer`。

### 场景 C：长滚动后展开（问题复现）

1. 重新启动应用，点击 `Load 80 history rows`。
2. 在消息区域向上滚动，出现 `Latest messages`；不要点击它。
3. 点击 `Show work`，将 `Worked for 1s` 定位到 List 顶部。
4. 点击 `Expand work`。

**正确画面应为：** `Worked for 1s` 下方直接出现工具行，List 的滚动范围随展开增长，可正常滚动查看工具项和后续 `Final answer`，不出现空白、残字、重叠或错误定位。

**当前 Windows 环境观察到：** 按钮变为 `Collapse work`，List 外的 `Outside content` 出现，但 List 内工具项不可见或只出现残缺文字。自动化同时确认 `tool-0` 节点已创建，因此不能把该现象当作未挂载或截图失败。

如果在另一台电脑得到不同结果，请记录操作系统、NSH 版本、窗口大小、显示缩放和刷新率，并同时保存自动化结果与实际窗口截图。

## 自动化与截图

普通场景，预期通过：

```bash
nsh test examples/collapsible-reveal/main.test.tsx
nsh test examples/collapsible-reveal/repro/main.test.tsx --target windows
nsh test examples/collapsible-reveal/repro/visual.test.tsx --target windows
```

`visual.test.tsx` 会生成：

- `target/test-tmp/official-reveal-expanded.png`
- `target/test-tmp/official-reveal-collapsed.png`

长滚动场景，用于复现问题，当前预期失败：

```bash
nsh test examples/collapsible-reveal/repro/long-scroll-up.test.tsx --target windows
nsh test examples/collapsible-reveal/repro/long.test.tsx --target windows
```

`long-scroll-up.test.tsx` 当前在 `await app.waitFor("id:tool-0")` 处超时，已观察到的错误为：

```text
id:tool-0 showing never happened in 5000 ms
```

`long.test.tsx` 还覆盖返回最新消息、定位工作行、展开后继续滚动的路径。这两个用例不是“应该通过的回归测试”，不要在未验证真实画面的情况下删除断言或将其改为只检查节点存在。

只捕获问题画面、不以内容不可见作为测试失败：

```bash
nsh test examples/collapsible-reveal/repro/long-scroll-visual.test.tsx --target windows
```

截图输出到 `target/test-tmp/long-scroll-expand.png`。该命令完成只表示截图已保存，不表示展开结果正确。

## 与生产实现的关系

生产消息 List 暂时保留可报告连续高度变化的折叠实现，用于同步滚动补偿。官方 `Collapsible.reveal` / `LayoutAnimation` 的父级 `onLayout` 只报告最终高度，不能满足现有的逐帧补偿；`AnimatedValue` 直接绑定 `height` 也不被当前 TypeScript 声明接受。

这份复现的目的不是证明官方动画普遍不可用，而是提供一个不依赖生产组件的最小场景，分别验证普通动画与长滚动 List 行为。后续若调整 NSH 或示例用法，应先让场景 C 的真实画面正确，再让两个长滚动行为用例通过。
