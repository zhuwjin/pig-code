# 虚拟 List 长滚动与官方展开动画验证

## 修正后的结论

使用 NSH 0.4.0、未修改 NSH 源码的独立 TSX 应用，普通展开、插入历史后展开以及长滚动后展开均可工作。此前关于“官方动画导致 List 内容已创建但不进入可见区域”的判断不成立，问题来自示例写法和测试断言，不应作为 NSH 上游缺陷提交。

已确认并修正两类问题：

1. 虚拟 List 的行函数按 cell 创建，cell 会被复用。示例曾用普通 `if (row().kind === "work")` 返回两种不同结构，复用后可能保留旧结构，造成 `Worked` 消失、内容错位等现象。现改为响应式 `If`，根据 `row().kind` 挂载对应分支。
2. `waitFor` / `shows` 能匹配滚动内容树中的节点，但不能证明该节点位于窗口的可见区域。原测试对 `tool-0` 和 `tool-39` 的可见性断言不成立。现使用 List 官方的 `onViewableItemsChange` 检查可视行，并保留真实 Windows 截图检查实际渲染。

`Show work` 的目标行接近列表末尾，因此滚动到底后通常位于视口底部，而不是顶部。这是滚动范围钳制的正常结果，不是重复行或定位故障。

官方参考：[List：行复用、可视行事件和滚动方法](https://nsh.dev/elements/list)。

## 环境与限制

- NSH 0.4.0。
- 当前验证环境：Windows，60Hz 显示器。
- 从仓库根目录运行命令。
- 不需要启动 Pig Code，也不需要真实会话、模型服务或密钥。
- 示例使用与主应用相同的主题；暗色主题下应显示清晰的浅色文字。

当前环境无法验证真实 120fps，也不能承诺每帧不超过 8.3ms。本示例使用官方原生动画，没有逐帧自绘高度动画；是否达到 120fps 仍需在 120Hz 设备上用 `--fps` 实测。

## 手动运行

```bash
nsh run examples/collapsible-reveal/main.tsx --target windows --window-size 900x700
```

如需同时观察性能数据：

```bash
nsh run examples/collapsible-reveal/main.tsx --target windows --window-size 900x700 --fps
```

顶部的 `Visible rows` 显示 List 原生可视行数量，以及 `work` / `answer` 是否在视口中。它用于区分“节点存在于滚动内容树中”和“行实际进入视口”，固定占一行，不随内容换行。

### 场景 A：普通展开和收起

1. 启动后应看到 `Worked for 1s`、`Final answer` 和顶部的 `Expand work`。
2. 点击 `Expand work`。
3. 按钮变为 `Collapse work`，`Worked for 1s` 下出现工具行，List 外出现 `Outside content`。
4. 示例共有 `tool-0` 至 `tool-39` 共 40 个工具项；向下滚动可看到后续工具项，`tool-39` 后是 `Final answer`。40 个工具项不会同时挤在一个 900×700 的窗口内。
5. 点击 `Collapse work`，工具行消失，`Final answer` 保留。

### 场景 B：插入历史后在底部展开

1. 重新启动应用，点击 `Load 80 history rows`。
2. List 定位到 `History answer 80` 附近，应只有一个 `Worked for 1s`。
3. 点击 `Expand work`。
4. 工具内容正常展开，可以滚动查看至 `tool-39` 和 `Final answer`。

### 场景 C：长滚动后展开

1. 重新启动应用，点击 `Load 80 history rows`。
2. 在消息区域向上滚动，出现 `Latest messages`；不要点击它。
3. 点击 `Show work`。`Worked for 1s` 应进入视口，通常靠近底部；`Visible rows` 应显示 `work=yes`。
4. 点击 `Expand work`。

**预期画面：** 工具项正常显示，不出现重复的 `Worked`、残字、重叠或异常空白。继续向下滚动可以看到 `tool-39` 和 `Final answer`，此时 `Visible rows` 应显示 `answer=yes`。点击 `Collapse work` 后工具项消失。

如果另一台电脑得到不同结果，请记录操作系统、NSH 版本、窗口大小、显示缩放和刷新率，并同时保存 `Visible rows` 状态与实际窗口截图。不能只依据节点查询结果判断画面是否正常。

## 自动化与截图

以下行为测试当前均预期通过：

```bash
nsh test examples/collapsible-reveal/main.test.tsx
nsh test examples/collapsible-reveal/repro/main.test.tsx --target windows
nsh test examples/collapsible-reveal/repro/long-scroll-up.test.tsx --target windows
nsh test examples/collapsible-reveal/repro/long.test.tsx --target windows
```

无窗口模式不生成截图，需要真实最终布局或窗口截图的步骤只在支持截图的平台上执行；上述 `--target windows` 命令会完整执行这些步骤。长滚动行为测试如果已经观察到 `answer=yes`，不会再强制向下滚动。

`long-scroll-up.test.tsx` 在不点击 `Latest messages` 的情况下覆盖上翻、定位工作行、展开和继续向下；`long.test.tsx` 还覆盖先返回最新消息再定位工作行的路径。两者都使用原生可视行事件验证 `work` / `answer`，不再把 `shows()` 当作视口可见性证明。

真实窗口截图：

```bash
nsh test examples/collapsible-reveal/repro/visual.test.tsx --target windows
nsh test examples/collapsible-reveal/repro/long-scroll-visual.test.tsx --target windows
```

普通场景输出：

- `target/test-tmp/official-reveal-expanded.png`
- `target/test-tmp/official-reveal-collapsed.png`

长滚动场景逐步输出：

- `target/test-tmp/long-scroll-loaded.png`
- `target/test-tmp/long-scroll-scrolled.png`
- `target/test-tmp/long-scroll-show-work.png`
- `target/test-tmp/long-scroll-expand.png`
- `target/test-tmp/long-scroll-up-final.png`

截图命令通过只表示步骤和截图完成，仍应打开图片检查实际渲染。

## 与生产实现的关系

生产消息 List 已按 NSH `v0.4.0` Gallery Chat 改为官方 `LayoutAnimation + If`，并始终使用原生 `maintainVisibleContentPosition`。自研 `ListCollapsible`、逐帧高度采样、测量缓存和滚动补偿已删除。展开状态切换统一在 `app/ui-context.ts` 中配置原生布局动画。

此前的示例故障来自示例写法和测试断言，不再作为官方动画或 List 回收异常的依据。
