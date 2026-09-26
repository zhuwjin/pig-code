# pig-code 调研与实现规划

> 目标：用 Rust + [gpui-kit](https://gpui-kit.com/)（0.6）从零开发一个图形化 AI Code Agent 桌面应用，
> 界面对标 [ZCode](https://zcode.z.ai/cn/docs/agents)（Z.ai 的 Agentic Development Environment），
> 架构参考本地 `agent-workspaces/` 下的 codex / kimi-code / opencode 等成熟开源实现。
>
> 调研日期：2026-09-20。

---

## 一、调研结论

### 1.1 gpui-kit 能力评估（结论：高度匹配，两个缺口需自研）

gpui-kit 0.6 明显针对 chat/agent 场景补齐了专用组件，ZCode 式界面的绝大多数元素都有现成件：

| 界面需求 | gpui-kit 组件 | 结论 |
|---|---|---|
| 聊天记录虚拟列表 | `MessageScroller`（变高虚拟化、跟随尾部、流式增长重测行高） | ✅ 专用 |
| 消息气泡 | `Message` + `Bubble`（头像/头部/页脚槽位、对齐、变体） | ✅ 专用 |
| 流式 Markdown | `TextView`（CommonMark+GFM、tree-sitter 高亮、`push_str` 增量追加、`stream_fade` 渐显） | ✅ 杀手级，官方示例 `examples/stream-markdown` 即 SSE 边收边渲染范本 |
| 输入框 @文件 /命令 $技能 芯片 | `Input`/`Textarea` 的 `InlineToken`（原子内联 token，官方 story 注释原文就是 "A chat composer"） | ✅ 专用 |
| 命令面板 | `Command`（分组/过滤/快捷键/键盘导航） | ✅ |
| 三栏布局 | `Dock`（可拖拽/可序列化）或 `Resizable` + `Sidebar` | ✅ |
| 代码块/编辑器 | `Editor`（rope 支撑 20 万行、只读模式、decorations） | ✅ |
| 附件芯片 / Toast / Dialog / Skeleton / Shimmer / Tabs / Tooltip | `Attachment` / `Notification` / `Dialog` / `Skeleton` / `ShimmerText` 等 | ✅ |
| **Diff 视图** | 无现成组件 | ⚠️ **自研**：只读 `Editor` + `tree-sitter-diff` 高亮 unified diff 文本起步；后续用 decorations 画增删行背景 |
| **终端模拟器** | 无 | ⚠️ 绕行：初期命令输出渲染为工具卡片；后续可用仓库内 `gpui-wry`（webview，macOS/Windows）嵌 xterm.js，或 `alacritty_terminal` 自绘 |

其他关键事实：

- `gpui-kit = "0.6"` 是 umbrella crate，`gpui_kit::*` re-export gpui 本体（zed gpui 的 Apache-2.0 快照 `gpui-pre`）。**务必走 crates.io，不要 git 依赖 zed 的 gpui**（会拉入 GPL-3.0 crate）。
- 异步：gpui 自带 smol 系执行器（`cx.spawn` / `cx.background_executor()`）；依赖树已含 reqwest/tokio，官方内置 `gpui-pre-reqwest-client`。
- Windows 构建要求：Win10+、VS 2022 C++ 工作负载、cmake；图形后端 wgpu（Windows 上走 DX12，无需 Vulkan SDK）。
- dev profile 需给 gpui 系 crate 开 `opt-level = 3`，否则 debug 渲染明显慢。
- 0.x 阶段 API 仍会变动 → 锁版本、跟进 changelog。
- Zed 的 `crates/agent_ui` 是最好的交互参考，但**只能读设计不能抄代码**（agent_ui 是 GPL-3.0）。

### 1.2 参考架构（来自 codex-rs / opencode / kimi-code）

三套实现殊途同归的架构共识：

1. **契约先行**：把「UI 可见的一切」收敛为独立的 protocol/schema 包。
   - codex-rs：`codex-protocol`（`Op`/`EventMsg` 信封 + ~90 个事件变体），UI 与 core 之间还有一层稳定的 app-server JSON-RPC v2 投影。
   - opencode：`packages/schema`（Effect Schema），依赖方向被 AGENTS.md 明文约束「Client 永不依赖 Core/Server」。
   - kimi-code：`packages/transcript`（同构渲染数据层，`transcript.reset` 全量快照 + `transcript.ops` 幂等增量 + seq）。
2. **命令走请求-响应，状态走 append-only 事件流**；事件带 seq，断线/重放用游标恢复。
3. **delta 事件与 durable 事件分离**：`text.delta` 这类流式碎片只用于即时渲染不落盘，`text.ended` 携带全量最终值作为可回放边界（opencode `session-event.ts` 的成熟设计）。
4. **权限 = 事件流里的挂起请求**：core 发出审批请求事件后阻塞等待，UI 用带相同 id 的回复命令解除阻塞（codex 的 `ExecApprovalRequest ↔ Op::ExecApproval`；opencode 的 Deferred + reply 端点）。
5. **会话持久化 = JSONL**：codex rollout（首行 SessionMeta，后续每行一个 item）+ sqlite 索引，resume 时重建历史。

**为什么不直接复用 codex-rs core**：虽然 Apache-2.0 且有 `InProcessAppServerClient` 嵌入路径，但
(a) 该版本 `WireApi` 只剩 Responses 协议，**不支持 Chat Completions**，对接 GLM/DeepSeek/Kimi 等 OpenAI 兼容端点不便；
(b) ~100 个未发布到 crates.io 的 path 依赖 crate，只能整个仓库做 git 依赖，跟随成本极高；
(c) 自研 core 本身是首要学习目标。结论：**借鉴设计，自研引擎**。

### 1.3 ZCode 界面规格（对标目标）

```
┌──────────────────────────────────────────────────────────────────────┐
│ 标题栏: 工作区名/Git分支 · 搜索(命令中心) · 设置                       │
├────────────┬───────────────────────────────────────┬─────────────────┤
│ 左侧边栏    │  会话区(消息流)                        │ 右侧面板(可切)   │
│ ┌────────┐ │  用户消息(含@文件/附件chip)            │ ┌─────────────┐ │
│ │新建任务 │ │  助手回复(流式Markdown)               │ │ 文件变更列表 │ │
│ ├────────┤ │  ▶ 思考轨迹(可折叠)                   │ │ +12/-3 diff │ │
│ │ 工作区   +│ │  ┌工具调用卡片──────┐                │ │ 打开/撤销   │ │
│ ├────────┤ │  │ $ 命令 / 读写文件  │                │ ├─────────────┤ │
│ │ 任务   +│ │  │ 输出(过长截断)    │                │ │ 终端(后置)   │ │
│ │ 视图:  │ │  └──────────────────┘                │ └─────────────┘ │
│ │ 分组/  │ │  ┌权限确认卡────────┐                │                 │
│ │ 时间线 │ │  │ 允许|始终允许|拒绝 │                │                 │
│ │ ├任务A ●│ │  └──────────────────┘                │                 │
│ │ │ +12/-3│ │  (回合结束: 摘要+耗时)                 │                 │
│ │ └已归档▸│ │                                       │                 │
│ ├────────┤ │ ┌─输入框──────────────────────────────┐ │                 │
│ │⚙设置  │ │ │ [附件/引用 chip 预览区]              │ │                 │
│ └────────┘ │ │ 输入文本…                            │ │                 │
│            │ │ [+] [执行模式▾][模型▾][思考▾]  [发送▶]│ │                 │
│            │ └──────────────────────────────────────┘ │                 │
└────────────┴───────────────────────────────────────┴─────────────────┘
```

P0 骨架（1:1 对标）：任务制侧栏（置顶/分组/归档/状态点/+−变更数）、四档执行模式（变更前确认/自动编辑/计划模式/完全访问，`Shift+Tab` 切换）、审批卡（允许/始终允许/拒绝）、输入框符号体系（`+`附件 `@`文件 `#`会话 `/`命令 `$`技能）、消息流（用户消息/流式 Markdown/折叠思考/工具卡片/回合作结）、Diff Review 面板、模型选择器（供应商+Base URL+API Key+手填模型 ID）、上下文水位显示。

P1 差异化：计划模式、AGENTS.md 双层注入（`~/.pigcode/AGENTS.md` + 工作区根）、子智能体、Skill/Command（`~/.pigcode/skills|commands/`）、MCP、"不在工作区中工作"模式。

P2 后置：内置终端、内置浏览器、远程开发(SSH/WSL)、Hooks、插件市场、工作区记忆、使用统计。

> 警示：ZCode 2026-09 因「仓库 Wiki 默认上传工作区」发生舆情事件。我们所有索引/上传类功能**默认关闭 + 明示开关**。

---

## 二、总体架构

### 2.1 进程模型

**单进程、双执行域**（对标 kimi-code 的「transport 同构」思路，但桌面端不需要 HTTP）：

```
┌─────────────────────── pig-code (单进程) ───────────────────────┐
│  UI 域 (gpui 主线程 + smol executor)   │   Agent 域 (tokio runtime 线程) │
│                                        │                            │
│  pig-app                               │   pig-core                 │
│  · 三栏布局 / 消息流 / 输入框            │   · Session / Turn 循环      │
│  · 事件 → 视图状态 reduce              │   · Provider(SSE 流式)        │
│  · 审批弹窗 → 回复 Op                   │   · 工具执行 / 审批闸门        │
│       ▲                                │   · rollout JSONL 持久化     │
│       │  Event 流 (async-channel)      │        ▲                   │
│       └────────────┬───────────────────┘        │ Op (mpsc)          │
│                    └────────────────────────────┘                   │
│                    契约 = pig-protocol (纯 serde 类型，双向依赖它)      │
└────────────────────────────────────────────────────────────────────┘
```

- **pig-protocol**（无依赖逻辑，只有类型）：`Op`（UI→core 命令）、`Event`（core→UI 事件）、消息/item 模型。UI 和 core 都只依赖它，core 不 import 任何 gpui 类型。
- **pig-core**：跑在独立 tokio runtime 线程（reqwest/SSE/工具进程都需要 tokio；gpui 的 smol executor 跑不了 tokio 生态）。通过 `async-channel`（smol/tokio 双侧兼容）与 UI 交换 `Op`/`Event`。
- **pig-app**：gpui-kit GUI。`cx.spawn` 里循环读 Event channel，`update` 进各 Entity 的视图状态。
- 这个边界将来免费获得「远程 core」能力（把 channel 换成 WebSocket 即可），也天然支持「一个 GUI 管多个并发任务」。

### 2.2 协议设计（第一版草案）

```rust
// pig-protocol: UI → core
enum Op {
    NewSession { cwd: PathBuf },
    SendMessage { session_id, content: Vec<UserContent>, mode: ExecMode },
    Interrupt { session_id },
    ApprovalReply { request_id, decision: ApprovalDecision }, // 允许/始终允许/拒绝
    SetModel { provider, model },
    Compact { session_id },
}

// pig-protocol: core → UI（seq 单调递增，支持重放）
enum Event {
    SessionConfigured { session_id, cwd, model },
    TurnStarted { turn_id },
    TextDelta { item_id, delta },            // live-only
    TextDone { item_id, full_text },         // durable 边界
    ReasoningDelta / ReasoningDone,
    ToolCallBegin { item_id, tool, input_summary },
    ToolCallOutputDelta { item_id, delta },  // live-only
    ToolCallEnd { item_id, result, is_error },
    PatchBegin { item_id, files },
    PatchEnd { item_id, diff: FileDiff },
    ApprovalRequested { request_id, kind: Exec|Patch, detail }, // core 阻塞等 ApprovalReply
    ContextUsage { used, total },
    TurnComplete { usage, duration },
    TurnAborted,
    Error { message },
}
```

要点：delta 与 Done 分离（opencode 模式）；审批请求带 `request_id` 与回复命令配对（codex 模式）；所有事件带 `session_id + seq`，将来 UI 断线重放用。

### 2.3 crate 划分（workspace）

```
pig-code/
├── Cargo.toml               # [workspace]
├── crates/
│   ├── pig-protocol/        # Op/Event/消息模型，serde，零业务逻辑
│   ├── pig-core/            # agent 引擎
│   │   ├── src/session.rs       # Session：历史、turn 循环、打断(CancellationToken)
│   │   ├── src/provider/        # ModelProvider trait + openai_compat(SSE) 实现
│   │   ├── src/tool/            # Tool trait + 内置工具 + 审批闸门
│   │   ├── src/prompt/          # system prompt 模板、AGENTS.md 注入、env 块
│   │   └── src/rollout.rs       # JSONL 持久化 + resume
│   └── pig-app/             # gpui-kit GUI（现有 src/main.rs 迁此）
│       ├── src/main.rs          # 窗口/标题栏/主题
│       ├── src/sidebar.rs       # 任务列表
│       ├── src/thread_view.rs   # 消息流(MessageScroller+TextView)
│       ├── src/composer.rs      # 输入框(InlineToken/Command popover/附件)
│       ├── src/review_panel.rs  # 文件变更 + diff
│       └── src/agent_client.rs  # channel 桥接、Event→Entity reduce
```

### 2.4 关键技术决策

| 决策点 | 选择 | 理由 |
|---|---|---|
| 模型协议 | **OpenAI Chat Completions + SSE**（自建 `ModelProvider` trait，预留 Anthropic 实现） | GLM/DeepSeek/Kimi/Qwen 全部兼容；codex 的 Responses-only 是反面教材 |
| core↔UI 传输 | 进程内 `async-channel`（非 HTTP） | 桌面单进程零开销；契约已由 pig-protocol 保证，将来可换 WS |
| 流式 Markdown | `TextViewState::push_str` + `stream_fade` | 官方现成，不用自己修未闭合语法 |
| 文件编辑工具 | **search/replace 式 `Edit`（old_string/new_string）起步**，后期可选 codex 的 `apply_patch` 格式 | Claude Code 系模型对 Edit 格式适应最好；apply_patch 解析器可后抄 codex（Apache-2.0） |
| diff 视图 | 只读 `Editor` + `tree-sitter-diff` 高亮 unified diff → 后期 decorations 行级背景 | gpui-kit 无 diff 组件，这是最大自研件 |
| 终端 | 不做真终端；命令执行=工具卡片（命令+输出+退出码） | ZCode 类产品的 agent 面板本质如此；后期再评估 webview+xterm.js |
| 会话持久化 | JSONL（codex rollout 式：首行 meta + 每行一 item） | 简单、可追加、天然支持 resume |
| 审批模型 | core 发事件后**阻塞等待** UI 回复；执行模式四档决定哪些工具免审批 | codex/opencode 共同验证的模型 |
| License 红线 | 只用 crates.io 的 `gpui-kit`；zed `agent_ui` 只读不抄 | 避免 GPL-3.0 传染 |

---

## 三、里程碑路线

> 进度（2026-09-20）：**M0–M5 已全部完成**（`cargo test -p pig-core` 25/25 全绿，GUI 自测 `PIG_SELFTEST=1` 全通过）。实施偏差记录：输入框芯片为纯文本 `@path` 降级方案（`InlineToken` 未包含在 gpui-kit crates.io 0.6.4 中，仅 git HEAD 有）；diff 渲染为等宽逐行着色+行号双列（Editor+tree-sitter-diff 方案未采用）；逐 hunk 接受/拒绝移至 M6+。
>
> 打磨（2026-09-22）：消息流工具调用块 1:1 对齐 ZCode —— 摘要行中文化 + 悬停才显示箭头 + 成功小勾/失败状态词、终端类展开为圆角描边卡片（`$` 命令 + 限高输出）；Write/Edit 经 `ToolCallEnd.edit`（协议可选字段，rollout 同步持久化）携带**本次编辑** diff（与 review 面板的会话累计口径分离），展开为 ZCode LightweightDiffPreview 同款代码卡（行号 gutter + 增删行淡底色/左缘色条，限 400 行截断）。
>
> 打磨（2026-09-22 二轮）：改动展示对齐 ZCode 双口径 —— ① 消息流每轮 turn 末尾新增**本轮改动**折叠面板（`ChangeTracker.turn_originals` 每轮首写前快照，回合结束 `Event::TurnFileChanges` 净额 diff，rollout `TurnChanges` 记录持久化可回放）；② 右侧 Review 面板改为纯 git 工作区口径（`git status --porcelain -z` + `numstat`，未暂存/已暂存 tab，untracked 逐文件数行 ≤1MB，单文件 git diff 原文，untracked 拼 /dev/null 合成 diff）；③ 输入框上方改动 chip 同步为 git 数据（未暂存+已暂存合并）。侧栏会话行的 +N/-N 徽章已去除（2026-09-23），ChangeTracker 会话级累计不再驱动任何 UI。
>
> 打磨（2026-09-23）：消息流左缘新增 **turn 导航条**（ZCode ConversationTurnNavigator 同款）—— 一条用户消息一根小横条，悬停时目标/相邻横条山峰式加宽（2.6x/1.7x/1.25x）并弹出该轮预览卡（悬停稳定 120ms 开 / 离开 80ms 关；`deferred` + `Positioner::side(Right)` 锚定横条右侧——gpui-kit HoverCard 只支持 corner 锚定、弹不到触发器右侧，故自绘；内容为用户消息前 2 行 + 助手 Markdown 摘要前 3 行，段落归一 + 220 字符截断对齐 `conversationTurnNavigatorHelpers`），点击 `scroll_to_top_of_item` 跳转（`nav_jump` 抑制一帧「回底自动恢复跟随」误判）；活动项取离视口顶最近的可见用户消息（对齐 `resolveConversationTurnNavigatorActiveQueryRowId`，长回复尾巴不会把高亮钉在上一轮），无悬停时 0.9 亮度强调，流式中最后一根最低 0.72；rail 超高内部滚动（独立 ScrollHandle + 滚轮不穿透），活动项变化自动滚到可见。面板够宽（≥720px）时内容列对称内缩 48px×2 给 rail 让位（对齐 ZCode `w-[calc(100%-6rem)]` 断点行为；gutter 只由面板宽度决定、与 turn 数无关——先占住位置，第 2 条消息发出导航条出现时内容列不抖动），面板宽 <720px 或 turn 数 <2 时隐藏；首帧 paint 前面板宽度为零值，render 补一帧使其出现。消息列表重构为每条消息一个直接子行（滚动定位只记录直接子元素）。
>
> 打磨（2026-09-23 二轮）：右侧面板改为**可收缩的标签页容器**（ZCode 同款）—— 默认收起（进会话不再自动显示改动）；标题栏右侧面板按钮（`PanelRight`/`PanelRightClose` 随态切换）直接展开/收起面板；顶部标签页栏（tab = 图标 + 名称 + × 关闭，关尽 tab 回到面板首页；末尾 `+` 与收起按钮）。**面板首页（菜单页）**：展开且无激活 tab 时内容区显示 改动（`Ctrl+Shift+G`，可用）/ 浏览器（`Ctrl+T`）/ 终端 / 侧边聊天（`Alt+Ctrl+B`）四项——后三项占位禁用，快捷键经 action 绑定先行展示（`Kbd::format` 拆键成单键芯片）；`+` 弹同款菜单（自绘弹层：`deferred` + `Positioner::side(Bottom)` 锚定按钮正下方，`on_mouse_down_out` 收起 + 按下位置吞 click 防收起又弹开，composer 弹层同款处理）——gpui-kit 的 `dropdown_menu` 走 corner 锚定，`BottomRight` 会把菜单弹到触发器上方超出窗口顶部，且弹层盖住标题栏 HTCAPTION 拖拽区时点击会被系统窗口移动模态循环吞掉，故不用。面板内容仍是 git 口径 ReviewPanel，无会话时显示空态。
>
> 打磨（2026-09-23 三轮）：修「面板开合后内容区抖动」—— 根因是内容列宽/gutter 由 paint 时测得的面板宽度驱动（`scroll_handle.bounds()` 滞后一帧），而绘制中的 notify 只标脏不排帧，错排帧会挂到下一次输入。改为：① 内容列纯布局驱动——gutter（两侧各 48px）只看 turn 数（≥2 轮即预留），内容列恒 min(860, 剩余宽度)，面板开合时内容列零重排；导航条本体显隐仍看测量宽度（小横条晚一帧不可感知）；② 面板开合（左右两侧）后 `cx.defer` 连补两帧，让 paint 时测量立即收敛（`schedule_layout_settle`）；③ 三栏面板改为**始终挂载 + `visible()` 切显隐**（原来条件增删子面板会让 resizable 按位置索引记录的尺寸簿错位、truncate 从尾部误删、adjust 按比例误缩放固定宽侧栏，表现为宽度漂移/错排残留）；④ 固定宽面板（侧栏/右面板）必须 `.flex_none()`——面板内部只在未测量时 flex_none，测量后恢复 flex_grow，兄弟面板展开时缺口会按比例分摊收缩到固定宽面板上（右面板一开侧栏 220 被压到 180）；⑤ 补渲帧改由 render 里 `request_animation_frame` 驱动（事件里 `cx.defer` 可能赶在绘制前执行，notify 被合并进当前帧，修正帧会残留到下一次鼠标输入）。resize 把手闲置不画线（侧栏 border_r、右面板 border_l 自带分隔），仅拖拽时显示（`with_handle_appearance`）。代价：turn 1→2 时内容列会收一次 96px（原设计用宽度断点规避了它，但那个方案才是面板开合抖动的根因）。
>
> 打磨（2026-09-23 四轮）：① 移除 `schedule_layout_settle`/`layout_settle_frames` 补渲帧（三轮 ②⑤）——内容列纯布局驱动后，开合面板实测无抖动，导航条显隐一帧滞后不可感知，兜底不再需要；② 修「贴底时导航条高亮钉在中间轮」——底部视口可同时可见多条用户消息气泡，「离视口顶最近」会选中更早的轮次；改为 `at_bottom()` 时活动项恒为最后一条用户消息（贴底 = 在读最新一轮），其余滚动位置维持「离顶最近的可见用户消息」不变。自测新增贴底活动项断言（`debug_nav_active_detail`）。③ 三栏最小宽度：侧栏 200 / 中心区 480 / 右面板 280（三者之和 = 窗口最小宽 960，钳制区间恒非空）——gpui-base 拖拽只钳 `PANEL_MIN_SIZE`(100)、无自定义区间 API，render 里用纯函数 `clamp_dock_widths` 补钳（paint 前修正，越界帧不可见）；收起的栏不占预算，顺序钳制（左先右后、右用钳后的左值）保证单侧越界只拉回单侧、两侧越界一遍收敛。④ 右侧面板首页菜单行改为整列居中（行宽上限 280、py_2、图标 size_4、text_xs；名称贴左、键帽贴右，键帽带边框、macOS 修饰键逐键拆帽）；「+」下拉菜单保持紧凑行 + muted 小芯片——同一行渲染函数加 `page` 参数区分。⑤ 输入框上方「改动」chip 点击改为直接打开右侧面板的改动 tab（新增 `ComposerEvent::OpenChanges`），删除芯片上方的改动弹层（`render_changes_panel` 与 `Popup::Changes` 一并移除）。⑥ 改动面板点开文件改为**整面板 diff 视图**（顶部返回栏：← 返回列表 + 头部截断路径 `…/段边界` + 重新拉取按钮），不再与文件列表上下堆叠；选中文件从列表消失（set_git_status）时自动回列表。diff 行号列加 `flex_shrink_0` 定宽——长行溢出时 flex 收缩曾把双行号列压窄，各行行号错位（有的靠前有的靠中）；列宽按本 diff 最大行号位数自适应（`10 + 位数×8`px），纯新增/纯删除文件的空列收成 4px 窄缝，不再固定 36px×2 留大块空白。⑦ 修 dock 拖宽把手「压不住线」：gpui-base 把手 `w(HANDLE_SIZE=1)` 是 border-box，4px padding 把内容区吃没了——命中区只有 1px 宽，左 dock（`Side::Left` 特例）还整体左偏 1px，线上不可拖、得压线左侧 1px。侧栏 `border_r` / 右面板 `border_l` 已去除（分隔线由把手自带线绘制），并**自绘 8px 透明热区骑跨分界线**：`on_mouse_down` 标记 `AppView::dock_resizing`，根容器 `on_mouse_move` 按指针位置驱动 `set_dock_size`（松手后首个未按键 move 兜底清除）；热区挡住上游 1px 把手，宽度仍过 render 里的 `clamp_dock_widths`。热区内置 1px 分隔线画在分界线正上（静止 border / hover ring 0.7 / 拖拽 ring 高亮；暗色下 accent 比 border 还暗，不能用）——上游左把手线被 dock 框架 overflow_hidden 裁掉、右把手线恰在分界线上，是一开始「左右不对称、无高亮」的根因。分隔线与拖动写入的 dock 宽度都取整到整像素，小数位置会让 1px 线抗锯齿发虚显粗。官方 dock 示例（examples/dock）对把手零定制，即默认皮肤直用；0.6.7 把手渲染重做（#3175/#3200）后复核移除热区。另：上游左把手自带线会跑偏（落在缝旁的侧栏/中心区里），与自绘线并存显粗——热区用两侧面板底色铺满 ±4px 把它整个盖住（中心区 render_center 同步补了不透明底），只留自绘的 1px 线。
>
> 上游跟踪（2026-09-23 调研）：官方对同类问题的答复是用 Dock（issue #1998），但 Dock 自带整套 chrome，与定制的 ZCode 式标签页栏/菜单页冲突，暂不迁移。**待办：gpui-kit 0.6.7 发布后升级**——其中 #3175/#3200 重做了把手渲染（Idle/Hovered/Pressed/Dragging 状态 + `h_resizable` 默认安装把手外观），升级后移除 main.rs 里的 `with_handle_appearance` workaround；另关注 #2597（动态面板增删）若合入可再评估。红线不变：只走 crates.io，不切 git 依赖。
>
> Dock 迁移评估（2026-09-23，`dock-spike` 分支 spike）：dock 体系原生支持「只调宽、不重排」——`DockArea::set_locked(true)` 官方注释即 "Lock the layout against rearranging. Resizing stays available."；侧 dock 开合有 `toggle_dock`/`set_dock_collapsible`，宽度有 `set_dock_size`，布局可 `dump`/`load` 序列化。面板实现成本低：Entity + Render 之外只需 `Focusable` + `EventEmitter<PanelEvent>` + `panel_name()`，组件层 `Panel` 扩展（title/tab bar/工具栏/缩放）全有默认且可关（`title_bar()=false` 等）。chrome 两条路：默认皮肤（省事、样式是它的）或自绘 `DockAreaRenderer`（官方 showcase 示例全自绘约 480 行，样式全控）。布局引擎在 gpui-base，跟主线可持续吃官方修复。**Spike 验收**：① 开合/拖宽宽度守恒不抖；② 外观能否压回 ZCode 式；③ 自测全过。
>
> Dock spike 实施（2026-09-23，`dock-spike` 分支）：三栏换 dock —— 左 dock=Sidebar（实现 Panel：`title_bar/inner_padding=false` 关 chrome），center=新 `DockCenterPanel`（回读 AppView 渲染 hero/会话列），右 dock=新 `DockRightPanel`（自绘 tab 栏+菜单页/改动内容）；`set_locked(true)` 只留调宽；dock 开合状态以 AppView 标志为准、render 时同步 `toggle_dock`；`DockSkin::set_toggle_button_visible(false)`。坑：组件皮肤用 `cached()` 包面板视图，缓存只在面板自身 notify 时失效——子实体的 notify 会沿 dispatch 树把祖先面板标脏自动失效，但纯 AppView 状态变化传不下来，面板用 `cx.observe(AppView)` 桥接。dock 尺寸钳制只有 `PANEL_MIN_SIZE..容器`，没有 180..360 那样的自定义区间。自测全绿。

> 子代理（2026-09-27，对齐 ZCode 体系）：**Agent 工具全链路**。档案体系（`agent.rs`）：内置 `general-purpose`（全工具）/ `explore`（只读 7 工具）双代理 + Markdown frontmatter 自定义（`~/.pigcode/agents/`、工作区 `.pigcode/agents/`，按名覆盖：项目>用户>内置）+ `agents-state.json` 内置代理模型覆盖；`model: inherit` 继承父模型、`providerId/modelId` 严格指定（解析失败即报错不回落）。执行：子代理独立 history（零父上下文），门控执行下沉 `exec_tool_gated_ctx(GateCtx)` 父子共用（危险黑名单/permissions/审批门全继承），工具收窄天然防嵌套（Agent/计划工具/AskUserQuestion 强制剔除），步数上限（档案 maxTurns/默认 20）、32K 结果预算落盘、结果=最后一条 assistant 消息 + agent_id/resume_hint；子上下文逐条落 `{session}.agents/{id}.jsonl`（resume 数据基础）。**后台运行**（run_in_background）：注册进任务表（TaskList/Output/Stop 统一管控，TaskStop 走 cancel 令牌），完成经 `<task-notification>` 合成消息唤醒父会话（忙入队/闲起新 turn）；**resume** 按 agent_id 重建上下文续跑。UI：Agent 卡片独立进度行（Spinner+muted，不覆盖摘要）、子工具调用不进父时间线（ZCode 单卡设计）、后台通知渲染为 info 通知卡而非用户气泡。管理设置页（列表/模型下拉/新建表单）与 AgentSwarm 并行未做。
>
### M0 — 应用骨架（GUI 先行，mock 数据）
- workspace 化（`pig-protocol`/`pig-core`/`pig-app`），`pig-app` 引入 `gpui-kit = "0.6"`，`gpui_kit::init` + 无边框窗口 + `TitleBar` + 亮暗主题。
- 三栏布局：`Sidebar`（任务列表，静态数据）+ 中央消息区 + 右侧 Review 面板（`Resizable`）。
- dev profile 开 `opt-level = 3`。
- **验收**：`cargo run` 出现 ZCode 式三栏窗口，可切主题。

### M1 — 聊天界面纯前端闭环（仍是 mock）
- `MessageScroller` + `Message`/`Bubble` 渲染消息流；`TextView` 流式 Markdown（先用定时器喂假 token 复刻 `examples/stream-markdown`）。
- 输入框：`Textarea` + `InlineToken`（@文件芯片）+ `Command` popover（/命令）+ 附件芯片区 + 发送/停止按钮 + 执行模式/模型下拉。
- **验收**：纯 UI 演示模式可"对话"（echo/假流式），输入框芯片、折叠思考块、工具卡片样式齐备。

### M2 — Agent core MVP（端到端打通）
- `pig-protocol` 第一版（§2.2）。
- `pig-core`：Session + turn 循环（采样→工具调用→回写→再采样，直到无工具调用）；OpenAI 兼容 provider（reqwest + SSE，流式 delta → `TextDelta` 事件）；工具先只实现 `Read` / `Bash`；tokio 线程 + channel 桥接。
- `pig-app` 接真 core：发送 → 流式渲染 → 工具卡片实时状态；停止按钮 → `Op::Interrupt`。
- 设置页最小版：Base URL / API Key / 模型 ID（配置文件 `~/.pigcode/config.toml`）。
- **验收**：配上 GLM/DeepSeek 任意 OpenAI 兼容端点，能完成「读个文件并总结」的真实多轮工具调用，流式渲染、可打断。

### M3 — 写能力与权限
- 工具补齐：`Write`、`Edit`(search/replace)、`Glob`、`Grep`；统一 `Tool` trait（schema 自动生成 JSON Schema 给模型）。
- 内置工具补充：`TodoList`（会话级待办，整体替换语义，状态挂 `ToolContext`）、`FetchURL`（scraper 提取正文，SSRF 私网字面量拦截）；`cap_web_search` 开启时按端点注入原生搜索——Anthropic 缺省 `web_search_20250305`，OpenAI 兼容端点用 TOML `web_search_tool` 自定义（如智谱 `web_search_tool = {"type":"web_search","web_search":{"enable":true,"search_result":true}}`）。
- 后台 Bash 任务：`Bash` 加 `run_in_background`（会话级注册表 + watcher 收输出 + 完成经 channel 推 `TaskListChanged`），配套 `TaskList`/`TaskOutput`/`TaskStop` 三工具；composer 上方「当前进度（TodoList）+ 后台 Bash」chip，点击在芯片上方弹出只读面板（点外部/再点 chip 收起、任务输出尾部展开、状态过滤 tab）。
- `AskUserQuestion` 结构化提问：工具注册 schema（1-4 题 × 2-4 选项，read_only），会话层拦截执行走 `QuestionRequested`/`QuestionReply`（独立 pending map，oneshot 阻塞；Esc 跳过回复 None，非错误）；composer 问题条复用审批条槽位（与审批互斥、问题优先），选项按钮 + 每题「其他」自由输入。
- 四档执行模式 + 审批闸门：core 发 `ApprovalRequested` 阻塞 → UI 审批卡（允许/始终允许/拒绝）→ `ApprovalReply` 解除；「始终允许」按规则缓存。
- diff 视图 V1：`PatchEnd` 携带 unified diff → 右侧 Review 面板用只读 `Editor` + `tree-sitter-diff` 渲染；文件变更列表（+x/−y）。
- **验收**：让 agent 改一个真实工作区文件，审批卡弹出、diff 正确渲染、可拒绝。

### M4 — 任务管理与持久化
- JSONL rollout（meta + item 每行一条）+ 会话列表从磁盘加载；resume（重建历史继续对话）。
- 侧栏任务体系：新建/置顶/分组/归档、运行中状态点、+x/−y 统计；多任务并行（每 session 一个 core 侧 Session，UI 切换订阅）。
- `@` 文件引用落地（工作区文件模糊搜索 → InlineToken → 发送时展开为内容）；`/` 命令（`/compact` 等内置）；AGENTS.md 双层注入。
- **验收**：重启应用会话还在；两个任务并行跑互不干扰。

### M5 — 长任务与打磨
- 上下文管理：token 计数 + 水位显示 + 自动/手动 compact（摘要压缩）。
- 计划模式（先出计划、确认后执行）；思考轨迹折叠块。
- diff 视图 V2（decorations 行级增删背景、逐 hunk 接受/拒绝）。
- system prompt 打磨（参考 opencode `prompt/*.txt` 与 kimi `system.md`：环境块 + AGENTS.md + 工具说明）。
- **验收**：连续 30+ 轮长任务不炸上下文；计划模式全流程。

### M6+ — 差异化（按兴趣择取）
子智能体（spawn_agent）、Skill/Command 目录（`~/.pigcode/skills/`）、MCP（`rmcp` crate）、工作区记忆、"不在工作区中工作"模式、webview 终端、使用统计。

### 建议节奏
M0→M2 是最陡的学习曲线（gpui 心智模型 + tokio/smol 桥接 + SSE 解析），建议**先各做一个 spike**：
1. `examples/stream-markdown` 跑起来改成读 SSE（半天，验证流式链路）；
2. 最小 tokio 线程 + channel + gpui 更新的 demo（半天，验证双执行域）。
两个 spike 通过后再正式开工 M0。

---

## 四、主要风险

| 风险 | 缓解 |
|---|---|
| gpui-kit 0.x API 变动 | 锁死 `=0.6.x` 小版本；升级前看 changelog；组件用法尽量照抄官方 story |
| Windows 构建链（VS2022 C++/cmake/wgpu-DX12） | 先跑通官方 `story` gallery 再开工；`script/install-window.ps1` |
| tokio ↔ smol 双运行时桥接出错（死锁/丢事件） | 单方向 channel、UI 侧只 `try_recv`/`await` 不阻塞主线程；M2 spike 先验证 |
| 流式 Markdown 长会话性能 | `TextView` 增量重解析已是官方方案；消息分块成多 item，避免单 TextView 过长 |
| 上下文窗口管理 | M5 前只做截断告警；compact 照抄 opencode/codex 的摘要提示词 |
| 范围失控（ZCode 功能面极大） | 严格按里程碑；P2 全部后置；先做出「能用的 M3」再谈差异化 |

## 五、参考资料

- gpui-kit：https://gpui-kit.com/docs/getting-started 、组件目录 https://gpui-kit.com/component 、仓库 https://github.com/longbridge/gpui-kit （重点看 `crates/story`、`examples/stream-markdown`、`examples/ai_recipes`）
- ZCode 文档：https://zcode.z.ai/cn/docs （agents / ADE-tools / safety-confirm / configuration 四页最重要）
- 本地参考实现：`agent-workspaces/codex/codex-rs`（protocol、turn 循环、rollout、apply-patch）、`agent-workspaces/opencode`（schema/事件模型/权限）、`agent-workspaces/kimi-code`（transcript 渲染契约、工具组织）
- Zed agent UI（设计参考，GPL 勿抄）：https://github.com/zed-industries/zed/tree/main/crates/agent_ui
