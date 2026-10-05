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
| **终端模拟器** | 无现成组件 | ✅ 已自研落地（2026-10-04）：`portable-pty` + `alacritty_terminal` + 自绘 Element，见文末当日记录 |

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

P2 后置：内置浏览器、远程开发(SSH/WSL)、Hooks、插件市场、工作区记忆、使用统计。（内置终端已于 2026-10-04 落地，见文末当日记录）

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
| 终端 | **真终端已落地**（2026-10-04）：GUI 进程内 portable-pty + alacritty_terminal 自绘，不经 pig-core；webview+xterm.js 弃选（渲染/输入链路不可控、体积大） | agent 面板内嵌本地 shell 是硬需求；命令执行仍渲染为工具卡片（两条链路并存） |
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
> 上游跟踪（2026-09-23 调研）：官方对同类问题的答复是用 Dock（issue #1998），但 Dock 自带整套 chrome，与定制的 ZCode 式标签页栏/菜单页冲突，暂不迁移。**已完成（2026-09-28）：升级 gpui-kit 0.7.0**——原计划的把手重做（#3175 指针 engagement 驱动指示器 Idle/Hovered/Pressed/Dragging、#3200 贴边把手的发线画在缝上、#3221 dock 发线不再压弹层）随 0.7.0 发布；0.6 把手四大病（命中区仅 1px、左把手左偏压不住线、左线被 overflow_hidden 裁掉致左右不对称、无 hover/拖拽高亮）逐一核实已修（gpui-base 0.7.0 `resizable/resize_handle.rs`：hug 带宽 5px 全在 dock 内侧、发线 = 容器最外像素即分界线、附回归测试）。据此移除了 main.rs 的自绘 8px 热区 workaround（`render_dock_resize_strip`/画线/底色遮盖），改回上游把手拖宽。之后试过补一条中心区侧 4px 透明热区（`render_dock_resize_grip`）让缝两侧都能起手，已回滚：上游的 pill 指示器由把手自身 hitbox 驱动（`SharedHandleState` 私有），中心区热区点不亮它，复刻指示器又不值得——**就用官方默认**（panel 侧 5px 带 + 指示器）。`sidebar_w`/`right_w` 目标宽副本由稳态 render 从 `dock_size()` 对齐（补间/边缘段期间实宽是过渡值不同步），`clamp_dock_widths` 补钳不变。另关注 #2597（动态面板增删）若合入可再评估。红线不变：只走 crates.io，不切 git 依赖。
>
> Dock 迁移评估（2026-09-23，`dock-spike` 分支 spike）：dock 体系原生支持「只调宽、不重排」——`DockArea::set_locked(true)` 官方注释即 "Lock the layout against rearranging. Resizing stays available."；侧 dock 开合有 `toggle_dock`/`set_dock_collapsible`，宽度有 `set_dock_size`，布局可 `dump`/`load` 序列化。面板实现成本低：Entity + Render 之外只需 `Focusable` + `EventEmitter<PanelEvent>` + `panel_name()`，组件层 `Panel` 扩展（title/tab bar/工具栏/缩放）全有默认且可关（`title_bar()=false` 等）。chrome 两条路：默认皮肤（省事、样式是它的）或自绘 `DockAreaRenderer`（官方 showcase 示例全自绘约 480 行，样式全控）。布局引擎在 gpui-base，跟主线可持续吃官方修复。**Spike 验收**：① 开合/拖宽宽度守恒不抖；② 外观能否压回 ZCode 式；③ 自测全过。
>
> Dock spike 实施（2026-09-23，`dock-spike` 分支）：三栏换 dock —— 左 dock=Sidebar（实现 Panel：`title_bar/inner_padding=false` 关 chrome），center=新 `DockCenterPanel`（回读 AppView 渲染 hero/会话列），右 dock=新 `DockRightPanel`（自绘 tab 栏+菜单页/改动内容）；`set_locked(true)` 只留调宽；dock 开合状态以 AppView 标志为准、render 时同步 `toggle_dock`；`DockSkin::set_toggle_button_visible(false)`。坑：组件皮肤用 `cached()` 包面板视图，缓存只在面板自身 notify 时失效——子实体的 notify 会沿 dispatch 树把祖先面板标脏自动失效，但纯 AppView 状态变化传不下来，面板用 `cx.observe(AppView)` 桥接。dock 尺寸钳制只有 `PANEL_MIN_SIZE..容器`，没有 180..360 那样的自定义区间。自测全绿。
>
> gpui-kit 0.7 新 API 落地（2026-09-28）：① **@提及 InlineToken 化**（composer.rs）——`insert_file` 改 `replace_range_with_token`（text=`@完整路径` 保协议、label=`@文件名` 短显、`InputToken`+FileText 图标渲染），光标/删除/撤销对提及原子化；`send` 的 files 改从 `tokens()` 收集（替代空白切词，顺带修了路径含空格被截断、普通 @词 误判）；token 校验失败回落旧文本路径。② **图片附件 Attachment 化**——`render_pasted_images` 换官方 `AttachmentGroup`+`Attachment`（缩略图/标题/描述/tooltip/悬停删除钮，`with_edge_fade`），删除 `attachments` 死代码三处。③ **问题条 Questionnaire 化**——`set_question` 存协议数据，render 里惰性 `ensure_questionnaire` 建 `QuestionnaireState`（每题 required+choices+「其他」input+header→description+Numbers 快捷键），Submit/Completed 事件映射回 `Vec<Vec<String>>` 发 QuestionReply；「放弃 Esc」自绘（协议只有整卷 None，官方 Skip 是逐题）；删 question_page/selected/other 等 ~200 行自绘状态机；四个 `debug_question*` 自测钩子签名不变、内部改写。注意：⏎ 确认需焦点在答案上（官方语义），submit 校验失败会跳首个未答题。④ **会话内搜索新建**（thread_view.rs+main.rs）——`ctrl-f` 开搜索条（`FocusThreadSearch`/`CloseThreadSearch`@thread-search），遍历 Markdown 段 `rendered_text()` 小写化 `match_indices`，普通命中 accent 0.25/活动 0.5 `set_range_highlights`，Enter/Shift+Enter 回绕跳转；**`reveal_range` 不会滚 v_flex 滚动容器**（上游只认自身 scrollable 和 gpui::list），靠 `TextView::on_reveal` 回调拿行 bounds 手动 `scroll_handle.set_offset`；每段缓存 RenderedText（owner+revision 判等）防重搜；Enter 用 `InputEvent::PressEnter` 订阅（键绑定先于冒泡，on_key_down 收不到）。用户气泡是 SelectableText 不参与。**不做**：Toolbar（底行 chip 非 Sizable 控件无收益且抢方向键）、TimeField（无场景）、`Theme::update`（只切模式不改内容，`Theme::change` 即可）。四个 `debug_question*` 钩子签名不变故 selftest 无需改动，全量自测全绿（含 AskUserQuestion 三项与 @搜索）。
>
> 表格行尾吞字（2026-09-28 定位）：现象是 Markdown 表格行尾被裁 1~2 字符（全角标点多的行必现）。**根因在上游 inline flow 换行测量**：`LineWrapper` 逐字量宽，CoreText 把孤立全角标点（，。：（））按半宽（14px 时 7px）计，行内实为全宽（14px），每处全角标点少算半 em，行被判定放得下、实际画超 → 单元格 `overflow_hidden` 裁掉行尾（gpui-kit **#3293**，main 已修：按上下文整形后按绘制宽度收紧重排；**在 0.7.0 之后合入，等下个版本升级**）。处理：保留 PR #2 的 `table.overflow.x=Scroll` scroll 表格（wrap 表格按字符数比例分列，「前四 slot」类短文本列会被压到折行，观感差，2026-09-28 恢复）。**不要动 `table_cell` 的 padding 做缓解**——列宽测量含 CELL_PAD_PX(16)，padding 改大会让所有列内容盒比测量窄、短列反而折行（实测「HUD 追加」折行即此因）。wrap/scroll 两种布局都受 #3293 影响，根治靠升级。纯文本段落不受影响（走 `layout_wrapped_line`，按整形字形位置换行）。
>
> 思考进行中样式对齐 ZCode（2026-09-28，参考 ~/开发/GhProjects/ZCode `packages/ui/src/components/ai-elements/reasoning.tsx`）：header 在进行中且折叠时显示「正在思考」（扫光，秒数只在完成态出现）+ `·` + **滚动输出行**——取累计思考全文的最后一个非空 trimmed 行，单行钉尾（`ticker_scroll` 每帧 `set_offset(-max.x)`，等效 ZCode 的 scrollLeft=scrollWidth，旧内容向左移出）。**钉尾的前提是给内容显式量宽**（`measure_ticker_width`，sidebar 跑马灯 `measure_title_width` 同款）：不显式给宽时滚动容器内的文本宽度会被布局钳进可用空间，`max_offset` 恒 0、内容停在开头（2026-09-30 纵滚改造时实测复现，`thread_view/tests.rs` 的 `ticker_roll_content_overflows_viewport` 用 headless 窗口把这个布局回归钉死）。纵向裁切不要给纵滚容器设 overflow——外层 viewport 的滚动 mask 已按 bounds 双轴裁剪（gpui `style::overflow_mask`：overflow 任一轴非 visible 即生效）。左缘 16px 线性渐变渐隐（`linear_gradient` 抄 AttachmentGroup 的 fade 写法，颜色 = 背景色），右缘渐隐按 offset 判定（钉尾态恒不显示）；行色比标签亮一档（muted 0.85 vs 0.6）。纵向滚轮经 `ScrollableMask` 冒泡给外层消息列表（横向才消费，否则滚轮悬在思考行上会把消息列表滚动吃掉）。换行纵滚动画于 2026-09-30 补上（ZCode `ToolCallBlocks/QueuedSummaryContent.tsx` 同款）：段内 `TickerRoll` 状态机（model.rs）以**行号为 key**——行号不变原位刷新；行号变 → 纵滚 300ms（旧行向上 -0.8em 淡出、新行自下方 +0.8em 淡入，cubic-bezier(0.4,0,0.2,1) 本地 Newton-Raphson 实现，gpui 无内置），promote 后 800ms（300 滚动+500 停留）内的新行排队（最多 2 条：同 key 覆盖 / 保第一条 / 新条占或换第二格），定时器漂移 >250ms 跳过中间条直接播最新；首行不播入场（ZCode `initial={false}`），展开/收起 `reset_to` 重置到最新行（ZCode 摘要随展开卸载、回折叠重新挂载）。渲染用 `with_animation`（AnimationElement 按 ElementId 键控、自动逐帧、自带 reduce_motion 静态终态），退场行 absolute 脱离布局（popLayout 同款）。肉眼验证走 mock 的 `TICKER_SCENARIO`（2026-09-30 加，mock.rs 注释有节奏表）：无工具调用，变速多行思考流——慢速长行（看钉尾横滚/渐隐）→ 快速连发六短行（看排队/跳播）→ 匀速三行收尾（看逐行纵滚）；因每片自带延迟，不走统一 50ms 写循环，在 handle_connection 里单独分支写出。选项卡指示器/角标经 `QuestionnaireChoice::items_center()` 整行垂直居中（官方默认对齐首行文本）；放弃/审批/Yolo 确认框按钮统一 `.small()` 对齐官方问卷动作按钮。

> 子代理（2026-09-27，对齐 ZCode 体系）：**Agent 工具全链路**。档案体系（`agent.rs`）：内置 `general-purpose`（全工具）/ `explore`（只读 7 工具）双代理 + Markdown frontmatter 自定义（`~/.pigcode/agents/`、工作区 `.pigcode/agents/`，按名覆盖：项目>用户>内置）+ `agents-state.json` 内置代理模型覆盖；`model: inherit` 继承父模型、`providerId/modelId` 严格指定（解析失败即报错不回落）。执行：子代理独立 history（零父上下文），门控执行下沉 `exec_tool_gated_ctx(GateCtx)` 父子共用（危险黑名单/permissions/审批门全继承），工具收窄天然防嵌套（Agent/计划工具/AskUserQuestion 强制剔除），步数上限（档案 maxTurns/默认 20）、32K 结果预算落盘、结果=最后一条 assistant 消息 + agent_id/resume_hint；子上下文逐条落 `{session}.agents/{id}.jsonl`（resume 数据基础）。**后台运行**（run_in_background）：注册进任务表（TaskList/Output/Stop 统一管控，TaskStop 走 cancel 令牌），完成经 `<task-notification>` 合成消息唤醒父会话（忙入队/闲起新 turn）；**resume** 按 agent_id 重建上下文续跑。UI：Agent 卡片独立进度行（Spinner+muted，不覆盖摘要）、子工具调用不进父时间线（ZCode 单卡设计）、后台通知渲染为 info 通知卡而非用户气泡。AgentSwarm/只读并发/MCP/WebSearch 于 2026-09-29 落地，见下条。
>
> 工具体系对齐 kimi-code/ZCode（2026-09-29）：**①turn 内只读工具并发**（`turn.rs` parallel_mask/run_parallel_group）：连续「read_only 且当前模式免审批」的调用切成并发组（JoinSet 补位式 spawn，上限 8），写/壳/拦截工具是同步点，ToolCallBegin 按原序发、End 随完成发（item_id 寻址容忍乱序）、history/rollout 按原 index 补齐，取消即 abort_all + 逐卡落定——零审批弹窗并发（结构上不可能）。**②MCP v1**（`mcp/`）：stdio JSON-RPC 2.0（8MB 行上限/pending drain/kill_on_drop），Claude 兼容配置（项目 `.pigcode/mcp.json` 覆盖用户 `<data_dir>/mcp.json`），工具名 `mcp__<server>__<tool>`（64 字符截断带 hash），**annotations 解析**（readOnlyHint → read_only，无标注保守非只读），Session 首 step 懒连接 `McpManager::connect_all`，schema 注入采样 + 门控经 `execute_with_extra`/`GateCtx.extra_tools` 兜底查找，MCP 工具 v1 一律串行走完整门控（不并发）。**③AgentSwarm**（`misc.rs` schema-only + `agent/swarm.rs` 准备管线 + `run_swarm`）：prompt_template × items（{{item}} 占位）/resume_agent_ids 续跑，校验拒绝重复 prompt/超 128 项；**子代理全局并发槽**（`task.rs` SUBAGENT_SLOTS 信号量=8，swarm 与后台 Agent 共享，超限排队且 command 带「排队中 · 」前缀、槽到手摘除）；聚合结果 32K 预算、溢出段降级为 result.md 指针。注意 serde_json Map 迭代序受 preserve_order feature 影响（workspace 统一编译会翻成插入序）——resume 键序显式 sort。**④WebSearch 工具**（`tool/websearch.rs`）：TAVILY_API_KEY/BRAVE_API_KEY 环境变量直连，与 provider 原生 cap_web_search 互补。
>
> MCP v2（2026-09-29）：**①streamable HTTP 传输**（`mcp/http.rs`，MCP 2025-03-26+ 单端点 POST）：配置双形态——stdio `{command,args,env,timeoutMs}` 与远程 `{url,headers,timeoutMs}`（`type` 可省略按 url 推断；`"type":"sse"` legacy 双端点形态不支持，config 层记录后跳过）。Accept 双形态响应（直接 JSON / SSE 流回包，SSE 边读边解码按 id 配对、流内 server→client 请求另开 POST 回 -32601）、`Mcp-Session-Id` 协商与后续携带、`MCP-Protocol-Version` 按协商结果携带、DELETE 尽力终止会话、响应体 8MB 上限、超时与 stdio 同口径。client 层抽 Transport（enum 分发：Stdio/Http），握手/tools/list 分页在 `McpClient` 统一。**②只读 MCP 工具进并发组**：`Session.mcp` 改 `Option<Arc<McpManager>>`（并发任务/后台子代理 clone owned 句柄现取）；并发安全核实——stdio 全 &self（AtomicU64 id + Mutex pending map + stdin 写锁多路复用，无需修正）、http 每请求独立 POST；`parallel_safe` 去掉 mcp__ 一刀切，改为 read_only + 免审批 + 项目 deny 预检（subject=工具全名，与串行门控同口径），非只读/被 deny 的仍是串行同步点。**③子代理继承 MCP**：收窄后内置 + MCP——全工具档案（收窄后含 Write/Edit，如 general-purpose）继承全部已连接 MCP 工具，只读档案（如 explore）只继承 readOnlyHint 的（Bash 不作判别：explore 含 Bash 但属只读档案）；schemas 同步进子代理采样，GateCtx.extra_tools 三处（前台/后台/swarm 闭包）传继承子集，审批/permissions 规则对子代理内 MCP 调用同效。
>
> Swarm 后台 + 回放卡 + 设置页（2026-09-29）：**①AgentSwarm `run_in_background`**（`dispatch_swarm_background`）：逐子代理发 SubagentCard(background=true) + 注册排队任务 + spawn 后即返聚合回执（逐项 agent_id/task_id/status + 「完成逐个 <task-notification> 送达，不要轮询」），每个子代理完成各自唤醒父会话（与后台 Agent 同链路）；usage 不进父回合（与后台 Agent 同口径）；抽出 `drive_subagent_detached` 供单/批后台共用。**②回放卡**：`RolloutRecord::ToolCall` 加 `#[serde(default)] agent_cards: Vec<AgentCardRecord>`（向后兼容），回放臂对单卡+批量卡逐张重发 SubagentCard，pig-app 段内卡列表多卡叠放、后台卡回放补 finished 终态。**③设置页**：MCP 服务器页落地（配置源展示=用户级/项目级 mcp.json 合并、状态点/stdio·远程 chip/超时、空配置引导含示例 JSON；连接状态经新增 `Op::ListMcpServers`→`Event::McpServerList` 查活动会话 manager，回合收尾刷新缓存）+ 新增「网络搜索」页（TAVILY_API_KEY/BRAVE_API_KEY 只显示已配置与否与优先级，配置引导）。遗留：legacy SSE 双端点、OAuth、MCP resources/prompts、roots/elicitation（server→client 请求暂回 -32601）、MCP 重连 Op（页面按钮已就位）。
>
> MCP 设置页编辑能力（2026-09-30，对齐 ZCode `packages/ui/src/settings/McpSettingsSection`）：**①新建/编辑/删除**——模态对话框（`settings/mcp.rs` render_mcp_dialog），表单模式（名称[编辑锁定]/作用域[用户级|项目级，编辑锁定、无会话时项目级不可选]/类型[stdio|HTTP 切换段]/命令+参数[空格分隔]/URL/超时 ms/高级折叠的环境变量或请求头 JSON textarea[env/headers 各留草稿，切类型交换回填]）⇄ JSON 模式（兼容粘贴 `{name:{…}}` 与 Claude 风格 `{"mcpServers":{…}}`，一次一条；双向切换重新推导对侧，JSON→表单解析失败留在 JSON 模式报错）双向切换；保存以原条目为底覆盖表单字段（oauth 等未知字段保真回写、type 键清除靠 command/url 推断）；删除两步确认，从条目来源文件删（项目级删除后用户级同名自然生效）。**②启停开关**：条目级 `disabled: true`（缺省=启用），core `config.rs` 解析保留、**覆盖合并后过滤**（项目级可停用用户级同名），`McpManager::connect_all` 只连启用条目；UI 行内 Switch 直写来源文件。**③状态升级**：`Event::McpServerList` 改载 `Vec<McpServerStatus>`（name/connected/tool_count/error），`McpManager` 收集连接失败原因（原来只 eprintln）+ `statuses()` 快照，runner 缓存同步改型；行内绿点=已连接+「N 个工具」chip、红点=连接失败+错误首行、灰点=已停用/未连接。**④列表/搜索**：页头搜索框（名称/命令/URL 过滤）+「新建服务器」按钮 + 空态新建入口；**非法条目保留**并列出原因（原解析静默丢弃 → 现在标红「配置无效」可进对话框修复，UI 校验口径与 core parse_server 对齐：type 显式校验、url scheme、sse 拒绝）。**⑤写入安全**：`upsert/delete/set_disabled` 保留 mcpServers 之外的文件级字段，既有文件非法时拒绝改写（不静默覆盖）、父目录缺失自动创建、pretty + 换行结尾。配置文件路径两行挪到列表下方。遗留不变：legacy SSE、OAuth、resources/prompts、MCP 重连 Op（连接仍会话级懒建立，改配置对之后新建会话生效）。
>
> Windows release 无终端（2026-09-30）：① main.rs 加 `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]`——release 双击不弹主控制台，debug 保留看日志；前置核实：GUI 子系统无控制台时 stdout/stderr 写入被 std 静默忽略（最小工程 Start-Process 脱离控制台实测 catch_unwind 不 panic），pig-core 的 eprintln 安全。②光有 ① 不够——**GUI 父进程 spawn 控制台子程序（git/cmd/bash/npx.cmd…）时每个子进程各弹一个新控制台**，审查面板高频刷 git 表现为疯狂闪窗：lib.rs 加 `NoConsoleExt`（Windows 给 std/tokio 两种 Command 统一 `CREATE_NO_WINDOW(0x08000000)`，非 Windows no-op），接入全部子进程 spawn 点（git.rs×4 / prompt.rs×3 / task.rs taskkill / task/shell.rs git 探测+三种 shell / mcp stdio）。坑：子模块 `use super::*` 会把父模块的 `as _` trait 导入一并带进来，shell.rs 自己的导入反而成 unused。PE 头复核 release subsystem=2（GUI）。
>
> MCP stdio Windows 启动修复（2026-09-30）：`Command::new("npx")` 在 Windows 报「program not found」——CreateProcess 对无扩展名程序只自动补 `.exe`，而 npm 系工具（npx/pnpm/bunx…）实为 `.cmd` 垫片；且 fnm/scoop 垫片目录里同名无扩展文件是 POSIX sh 脚本（直接启动会「不是有效的 Win32 应用程序」）。`StdioTransport::spawn` 前先 `resolve_program`（PATH 目录列表 × 候选扩展 exe→cmd→bat，优先于无扩展原名；显式路径同样补扩展，带扩展名不存在不猜），解析为全路径后 std 自动经 cmd.exe 启动并转义参数（CVE-2024-24576 修复后行为）。非 Windows 不编译该分支。同批补 stdio 子进程工作目录：spawn 原先不设 `current_dir`（继承应用进程启动目录），args 相对路径（如 dbhub 的 `--config .dbhub/dbhub.toml`）解析落错位置——现 `McpClient::connect` 增带 workspace_root（`connect_all` 透传会话 cwd），stdio spawn `current_dir(workspace_root)`，与 Claude Code 同语义。
>
> MCP 设置页多工作区切换（2026-09-30 二轮，对齐 ZCode `PluginScopeMenu`）：列表上方新增**作用域选择器**（pill 按钮 + deferred/Positioner 下拉，outside-close 按下位置吞 click 同 API 格式弹层）——下拉即「用户级 + 全部工作区」两项一类（截图同款）：**用户级**（默认，只列/只写 `<data_dir>/mcp.json`，对所有工作区生效）或**任一工作区**（清单与侧栏同口径：`compute_workspaces()` 可见 ∪ 会话 cwd，别名优先显示名；会话所在工作区带「当前会话」chip），选中后查看/编辑/启停/新建都指向用户级 + 该工作区 `.pigcode/mcp.json` 的合并视图（同名项目级覆盖）——**无会话也能编辑项目级**（原来必须打开会话才能定位项目文件）。`AppView::refresh_mcp` 改按 `settings.mcp_scope()` 决定快照的工作区参数（用户级 → 不合并项目），`set_mcp_config` 增带 `session_cwd`；连接状态：用户级条目参与会话连接照常显示，工作区视图仅当该工作区 == 会话工作区时显示（否则提示「正在查看其他工作区的项目级配置」——core 连的只会话工作区的配置，名字巧合匹配会误导）。工作区清单经 `sync_mcp_workspaces` 喂入（WorkspaceList 事件 + 打开设置时），对话框「项目级」按钮带工作区名（`项目级（pig-code）`）。
>
> Glob 匹配引擎换 `ignore::overrides`（2026-09-30 三轮）：起因是 `**/*.{rs,toml}` 静默无匹配——glob crate 不支持花括号（`{}` 当字面量，不报错），且其 `*` 默认跨 `/` 与两家参考实现相反（ZCode 手写 glob→RegExp matcher `*`→`[^/]*` 不跨层、kimi-code 直接 spawn `rg --glob` 全量继承 globset 语义）。先手写 `expand_braces` 补了一天，随后整体换成 `build_overrides`（`OverrideBuilder`，ripgrep `-g`/gitignore 语义：花括号可嵌套、`*` 不跨 `/`、`**` 跨层级、`!` 前缀黑名单、未闭合 `{`/`[` 显式报「无效 glob 模式」而非静默无匹配），`walk_workspace` 加 `Option<Override>` 参数把过滤前移到遍历期（顺带只有命中文件才 stat mtime）；原「含 / 按路径、不含 / 比文件名」双模式分支消融在 gitignore 语义里（无 / 模式天然匹配任意深度 basename）。Grep `include` 同换（仍只比文件名：`ov.matched(文件名)` 即 basename 语义）。权限规则（`permissions.rs`）仍用 glob crate 不换——规则形如 `Bash(cargo *)` 无花括号需求。同批：工具卡/思考块的中文行内标签（「终端」「等待批准」等）加 `flex_shrink_0 + whitespace_nowrap`——flex 收缩按基准宽比例分摊，长命令行会把标签挤窄几 px，中文任意字间断行（min-content 仅 1 字宽）导致「终端」折成两行；截断只应发生在摘要（`min_w_0 + text_ellipsis`）上。
>
> 字体设置（2026-09-30，外观页）：项目不自嵌字体，全走 gpui-kit `Theme` 全局（`font_family` 默认 `.SystemUIFont`；`mono_font_family` 平台默认 macOS Menlo / Windows Consolas / Linux DejaVu Sans Mono，代码块/diff/命令行/任务名显式引用）。**①协议**：`AppConfig` 加 `#[serde(default)] ui_font/mono_font: Option<String>`（None=默认，向后兼容旧 config.toml）。**②应用层**（新模块 `pig-app/src/font.rs`）：GPUI 对未安装家族首次排版即 panic（`Font::fallbacks` 只补缺字形不救缺家族），故配置字体名必须对 `cx.text_system().all_font_names()` 校验（已排序去重、枚举 ~百毫秒进程内 OnceLock 缓存，对齐上游 mono_font 探测），未安装降级默认并 eprintln 告警；`.SystemUIFont` 是虚拟家族不在列表需放行。默认值恢复：`gpui_kit::init` 后 `capture_defaults` 记录实际生效默认（等宽默认可能已被上游换成备选，不能写死平台名）到 `FontDefaults` global。应用点两处收敛到 `apply_config_fonts`（幂等、值有变化才 `cx.refresh_windows()`）：`Event::ConfigSnapshot`（启动 `GetConfig` 与每次 `SaveConfig` 后都会发）+ 设置页选中即时生效。**③设置 UI**：外观页加「界面字体 / 等宽字体」两个可搜索 `Select`（`SearchableVec<String>`，选项=「系统默认」哨兵首项+已装字体；哨兵映射 None，回填经 `sync_form` 的 `set_selected_value`）。已核实亮/暗切换不重置用户字体：`Theme::change→apply_config` 只在主题文件显式给 `font.family` 时覆盖，内置默认主题 JSON 无该键。字体大小未做（`Theme.font_size` 存在，后续可加档位）。坑：手写测试 config.toml 时顶层键放在 `[[providers]]` 之后会被 TOML 归进最后一个数组元素，配置静默失效——排障先核对键的实际归属表。
>
> 设置页视觉对齐 ZCode（2026-09-30，参照 `packages/ui/src/settings/SettingsPageParts.tsx` 与 `SettingsPage.tsx`，Apache-2.0 可参照）：**①通用构建器**（settings.rs 底部 `section_header/settings_group_card/settings_row`）——ZCode 的核心设计语言是「小节标题+描述 → 圆角边框分组卡片 → 行式设置项（左 label+描述、右 260px 控件右对齐、行间 border_t 首行无）」，gpui 无 card 色用 `secondary` 当卡面。**②外观页重写**：三个主题大卡片 → 两张分组卡（界面：主题模式+界面字体；代码：等宽字体），主题模式改 `Select` 下拉（跟随系统/暗色/亮色，对齐 ZCode THEME_MODES；主题不落盘仍是会话级）。主题下拉回填走 `appearance_dirty`（构造/打开设置页置位，render 前 `sync_appearance` 对齐全局态——设置页关闭期间系统外观可能已切）。**③侧栏**（240px）：按钮 h_8/px_2/gap_2、图标改正常色（原 muted 偏灰）、组标签 text_sm font_medium、选中态 bg(accent) 不变（ZCode surface-hover 同语义）。**④内容列**限宽 896px 居中（ZCode max-w-4xl + mx-auto）。模型/MCP 等页暂未卡片化（布局各异，后续逐页迁到 settings_row）。
>
> 模型设置页对齐 ZCode（2026-09-30 二轮，参照 `model-provider-section/SectionLayout.tsx`、`ProviderCardSections.tsx`；**智谱定制全部不搬**：Coding Plan 卡/BigModel 注册提示/预置供应商/OAuth 登录态——pig 只有自定义供应商）：**①页级布局**：大标题 → 「描述 + 添加供应商按钮」一行 → **一张大卡片**（min-h 576px、rounded_lg border secondary overflow_hidden）内左右分栏——左 224px 导航列（border_r、组标签「供应商」）+ 右 p_6 详情。**②供应商行**对齐 ZCode 导航行：去每行边框、h_8 圆角、选中 bg(accent) 高亮（原 primary 描边）、启停状态点保留、名称 truncate。**③详情**：空态改居中图标+提示；表单 label text_xs→text_sm（ZCode text-ui-base subtle）；**模型列表容器化**——单张圆角卡（bg background 抬升层次）内多行 border_t 分隔（原每行独立边框），空态虚线框 + Info 图标；添加按钮改 secondary。未搬的通用能力（后续可加）：供应商拖拽排序、名称行内编辑（pig 在表单改）、模型行内编辑（pig 用弹窗，保留）。
>
> 设置页选中态/色阶修正（2026-09-30 三轮，截图反馈「选中没标识、颜色不统一」）：**①选中/hover 换官方 token**——设置导航与供应商行原用 `accent`（暗色下太淡几乎不可见）、hover `accent.opacity(0.6)`，全换成官方 ListItem 同款 `list_active`/`list_hover`（语义即 ZCode surface-hover，暗色下亮度足够）。**②模型列表容器底色** `background` → `input_background()`（Input 组件官方底色，dark=input.mix(transparent,0.3)）——消除「模型条目色块与输入框不同色阶」断层。经验：设置页类「列表选中」一律用 list_active/list_hover，accent 只留给 chip 点缀与主界面既有视觉（composer/title_bar 等不动）。
>
> 设置页整体换官方 Settings 组件（2026-09-30 四轮，放弃自仿 ZCode）：手搭的两轮 ZCode 风格（分组卡片/行式条目/侧栏）在与官方组件视觉拼合上反复返工（选中态 token、色阶断层），决定框架直接用 `gpui-component::setting::Settings`——macOS 式「侧栏（内置搜索+分组导航，`window.use_keyed_state` 按 id 持久选中页）+ 页面（标题/描述/GroupBox 分组/虚拟滚动 List）」。**①外观页全内置字段**：主题模式 `dropdown`（getter 直读全局 ThemeFollowSystem+mode，天然与外部状态同步——删掉整套 appearance_dirty 回填；跟随系统分支用 `Theme::change(cx.window_appearance(), None, cx)` 免 window）、界面/等宽字体 `scrollable_dropdown`（数百项弹层滚动；牺牲了自仿版的可搜索，选字体靠滚动）——三个 SelectState 实体与订阅全删。**②复杂页走 `SettingItem::render` 自定义**（模型/MCP/技能/网络搜索）：整页现有渲染塞单个条目，闭包 `Fn(&RenderOptions, &mut Window, &mut App)` 拿不到 Context——捕获 `WeakEntity<SettingsView>`，`weak.update(cx, content)` 回到视图内调 `cx.listener`/发 SettingsEvent（getter 同理 `weak.upgrade().read(cx)`）；自定义条目无标题，须 `.keywords()` 否则侧栏搜索搜不到。**③删除**：SettingsPage 枚举/NAV/page_title/render_nav/render_appearance/pages.rs、底部三个 ZCode 式构建器。占位页（常规/浏览器控制等「即将推出」）不再注册即自动从导航消失。**④切页刷新缺口**：官方导航无切换回调，MCP/技能的「进页即刷新」退化为 open_settings 时统一刷新 + 页内刷新按钮。教训：能在官方组件语义内表达的字段（dropdown 等）优先内置，getter 直读事实源比「本地状态+回填同步」少一整层脏标记机制。
>
> 返回按钮终版：移入标题栏（2026-09-30 十轮）：侧栏叠加方案经三轮微调仍难对齐——absolute 猜不了侧栏实际宽度（菜单项高亮块接近全宽，返回按钮块到文字就截止，宽度天然不一致）、header 自带留白使间隙比设计值大。终版：设置页打开时窗口标题栏（pig 自有 `render_title_bar`）左区从「侧栏开关 + 会话标题」换成「← 返回工作区 + 『设置』标题」——与其他标题栏按钮同款 ghost small 样式、位置标准（macOS 应用内设置惯例），设置侧栏的叠加按钮与 header_style 留白全部删除，侧栏回归官方原始布局。
>
> 设置页补返回入口（2026-09-30 九轮）：换官方 Settings 后「返回工作区」按钮随自制侧栏被删，只剩 escape。第一版放独立工具栏被否（整页下移一段）——终版：`header_style` 给侧栏搜索框上方垫 44px（`div().pt(px(44.)).style().clone()` 从 builder 取 StyleRefinement 的手法），返回按钮（ghost small）`absolute` 叠在左上留白区，零布局侵入；按钮 left 固定 10px 在侧栏最小宽 160 内，拖宽侧栏不受影响。
>
> API 格式改 Select（2026-09-30 八轮，反馈「选项字体比输入框大、对齐不同」）：根因是 Button 默认 medium = text_base(16px) 且 label 容器 justify_center（content_style 是 pub(crate) 不可覆盖），而 Input 默认 = input_text_size(Medium)=text_sm(14px) 左对齐。正解不是调按钮而是换组件：API 格式行改 `Select`（`SearchSelectField` 通用化：加 width/menu_width 字段，字体 220/菜单 300、主题 220/300、API 格式 w_full/360），两选项不可搜索，Confirm → set_api_format 写回选中供应商；回填挂 sync_form（select_provider 置 form_dirty → render 前 set_selected_value）。顺带删掉自制 format_popup 三件套（deferred Positioner 弹层 + outside-close 按下位置吞 click）与 render_format_popup——Select 自带菜单，约 80 行弹层代码退役。类型 FontSelectState 更名 TextSelectState（三处复用）。
>
> 供应商行选中样式改回原版（2026-09-30 七轮）：用户确认 ZCode 式「背景高亮选中」（list_active/list_hover）不要了，恢复 ZCode 改造前的原版——边框行卡 + 选中 primary 描边 + hover accent.opacity(0.6)。
>
> 模型页配色官方化（2026-09-30 六轮）：拆掉 ZCode 式手搭外壳（min-h 576 大卡 bg secondary + border_r 分栏 + 手动 border/rounded），左右两列各用官方 `GroupBox`（Fill 变体，`tokens.group_box` 官方卡面色）；`Settings::with_group_variant(Fill)` 全局统一（外观页分组同款卡面）。添加供应商按钮收进左列 GroupBox 顶部（w_full outline），页头描述行删（page description 已有）。供应商行选中态 list_active、模型列表 input_background 均为官方 token 保留。
>
> 字段下拉统一 Select 视觉（2026-09-30 五轮，截图反馈「下拉宽度与选中样式不一致」）：scrollable_dropdown（outline Button 触发 + PopupMenuItem 勾选菜单、宽度内容自适应）与自定义 Select（输入框风格触发 + accent 高亮菜单）并排差异扎眼。**统一到 Select**：主题模式也换 `FontSettingField`（三选项不可搜索），三下拉同宽 220、同款触发器与菜单选中样式；主题模式回填恢复 appearance_dirty（构造/open_settings 置位 → render 前 sync_appearance，`current_theme_mode` 返回 String 直配 Select 条目）。**有状态字段要点**：SelectState 是 Entity，`render_field` 每帧调用不能现场建——状态实体由 SettingsView 持有，选中事件经 `cx.subscribe_in` 订阅（字体走 set_font、主题走关联函数 apply_theme_mode），config 回填走 sync_form 的 `set_selected_value`。经验：官方 Settings 的内置 dropdown（Button+DropdownMenu）与 Select 是两套视觉体系，混用必违和——一个分组里要么全内置字段、要么统一自定义 Select。
>
> 调用轨迹面板对齐 ZCode（2026-10-04，参考 ZCode `packages/ui/src/ModelTrajectory*.tsx`）：右侧「调用轨迹」tab 从「整卡手风琴」重写为 ZCode 信息结构——副标题 `N 次调用 · 48,442 tok · 模型`（千分位、模型保序去重）；调用卡片**正序**排列（原倒序），分组头 = `01` mono 序号 + 来源（主会话）+ 结束原因中性胶囊（仅失败用 danger 色）+ 右侧 mono `IN 23,953 · OUT 187 · 4.79s · 02:41:48 PM`（IN = input+cache_read，OpenAI 口径 input 已扣缓存命中；耗时 <1s 毫秒 / <10s 两位小数秒 / 否则一位，时刻 12 小时制）；「输入」「输出」各为圆角描边卡（标题条 accent 0.4 底 + 行间 1px 半透分隔线），逐消息一行 = 定宽 72px 彩色角色标签 + mono 单行预览（空白折叠为一格）+ chevron，行间斑马纹（accent 0.3，奇偶跨两卡连续）。**各行独立展开**（`TrajectoryState.expanded: HashSet<"{turn}:{row_ix}">`，turn 唯一标识一次调用，刷新/回合完成后展开态保留），展开行头部带 `耗时 · 10/4/2026, 2:42:08 PM` 与复制按钮（`write_to_clipboard` + `stop_propagation` 防触发行折叠）。角色颜色按 ZCode 深浅两套 hex（用户蓝/助手青/思考紫/工具调用橙/工具结果天蓝，80% 不透明度；system 用主题灰），随 `is_dark()` 切换。行默认收起（ZCode 默认全展开靠 react-virtual 虚拟化撑着；gpui 全量渲染下 4K 字符系统提示词全展开不实用）。selftest 补「轨迹面板加载 + 展开态渲染」断言（会话 A 场景 B 落 ≥2 条 model-io 后驱动）。
>
> turn_reminder 对齐两家变更触发口径（2026-10-04）：对照 ZCode（`system-reminder/source.ts`：执行模式仅 plan 模式且 ≥5 用户回合节流，date_change 跨天才发）与 kimi-code（`reminderService.ts`：每 step 评估但全部条件触发，permission_mode 切换才发）后，执行模式行从**每回合无条件**改为**首轮一次 + 切换后下一回合一次**（`Session.mode_reminded: Option<ExecMode>` 去重，不持久化、resume 后首轮自愈，plan 进出经 `self.mode` 恢复也自然覆盖）；无可提醒内容时 `turn_reminder` 返回 `Option<String>` 的 None——用户消息不再每回合顶一个只含模式行的空 reminder。日期/AGENTS.md 的变更触发 + 去重不变。另修日期口径：`prompt::today()` 从 UTC 改**本地时区**（pig-core 新增 `time` 依赖 local-offset，失败回退 UTC；原实现本地 0:00–8:00 间冻结日期会比本地晚一天，对齐 ZCode `lastEmittedLocalDate`）。`context_stability` 集成测试加回合 3（模式切换后 reminder 只带新模式行、AGENTS.md 不重复、旧前缀逐字节稳定）。**连带修复**：用户消息变短后自测挂在子代理场景——selftest 数据目录原本在工作区内部（`{ws}/data`），子代理 Grep 工作区会搜到历轮 model-io 落盘里的用户消息原文（含 `ECHO_HISTORY` 等 mock 触发词）；原来触发词前有 ~110 字符 reminder 把它顶过 Grep 的 500 字符行截断（`MAX_GREP_LINE_CHARS`），变短后触发词进入截断窗口回流请求体，抢先命中 mock 的内容路由。根治：selftest 数据目录移到工作区**外**（`pig-app-selftest-data-{pid}`，对齐生产 `~/.pigcode` 不在工作区内的布局）。
>
> 标题栏「⋯」会话菜单补齐置顶/归档/重命名（2026-10-04）：与侧栏右键菜单同链路（`agent.set_pinned/set_archived` + core 回 SessionList 刷新）；菜单对齐从 `Align::End` 改 `Align::Start`（gpui-base Positioner：Bottom+End = 菜单右缘对齐按钮、向左展开，Start 反之向右展开）。**重命名可见性兜底**：行内输入框画在侧栏会话行上，目标行可能被藏——归档会话在分组视图收在「已归档」折叠区、工作区视图不渲染归档，普通会话也会被工作区折叠/分页/搜索词挡住；`start_session_rename` 前跑 `session_row_visible` 检查，不可见则退回分组视图 + 展开归档区 + 清空拦路搜索词。侧栏右键重命名同函数受益。重命名图标用 `AssetsIconName::SquarePen`——`gpui_component::IconName` 是裁剪子集（无笔类图标），完整 Lucide 目录走 `gpui_kit::assets::IconName`。
>
> 标题栏「在文件管理器中打开」split 按钮（2026-10-04，对齐 ZCode 标题栏）：分支 chip 左侧，官方 `DropdownButton`（outline small）——主钮直接打开当前会话工作区，chevron 出 PopupMenu（默认 `Anchor::TopRight` 右对齐下弹），后续「在终端/编辑器打开」挂同一菜单。打开逻辑在 pig-core 新模块 `files.rs`（core 独占子进程 spawn，`NoConsoleExt` 不外泄）：macOS `open` / Windows `explorer` / Linux `xdg-open`，detached 不等退出码（explorer 成功也常返回非零码）；`file_manager_name()` 出平台称呼（访达/文件资源管理器/文件管理器）供菜单文案。仅会话态显示（hero 无 cwd 不显示，与分支 chip 同条件）。**图标走真图不走字形**：gpui 的 `Icon`/`svg()` 会把 SVG 按文字色渲成单色（自绘 finder.svg 曾因此整体发白被否），ZCode 的做法是平台层取真实 App 图标以 `<img>` 渲染（`WorkspaceEditorButtonGroup` + EditorInfo.iconDataUrl）；对应实现 = pig-core `finder_icon_png()`（`NSWorkspace.iconForFile(Finder.app)` → TIFF → NSBitmapImageRep → PNG，objc2 系 crate 本来就在 gpui 依赖树内，直引零体积成本；macOS-only，AppView::new 后台 executor 取一次存 `fm_icon: Option<Arc<Image>>`），按钮侧 `Button.child(img(Arc<Image>))`（icon 槽只收 Icon/ButtonIcon，children 才能放 img），取到前与其余平台回退 Lucide `FolderOpen`。菜单项用 `PopupMenuItem::element` 自绘「彩图+文字」行（`icon` 槽只收单色 Icon，ElementItem 才能放 img；无真图时退化纯文字项）。**糊度坑**：1024px 原图直接上屏，gpui 无 mipmap、GPU 双线性抽 texel 缩到 ~32px 必糊——用 `image` crate Lanczos3 预缩放到 `FM_ICON_PX=64`（16pt×4x 上限），测试断言输出尺寸钉死该口径。
>
> 内嵌终端面板（2026-10-04，参考 `~/开发/GhProjects/tty7` 的 `src/terminal/*`，Apache-2.0 可借鉴；**只抄终端代码，不抄它 zed-gpui git fork 的依赖方式**）：**①形态**——标题栏加 `SquareTerminal` 按钮 + `ctrl-\`` 快捷键，composer 下方展开 300px 底部终端（`AppView.terminal_open` 开关仿 `right_open`，懒创建、收起仅隐藏，tab 与 shell 进程保留）；开合走匀速高度补间（dock 侧栏同体系 200ms 按路程比例 + on_next_frame 链：外壳 `overflow_hidden` + 内容固定 300px `absolute bottom_0` 锚底——滑动揭幕而非压缩重排，终端网格全程零 resize，防逐帧 SIGWINCH + shell 重绘风暴；中途反向从当前高度重出发，reduce_motion 直落终态）；上缘 6px 热区拖拽调高（`cursor_ns_resize`，移动在根容器 on_mouse_move 统一跟踪、松手后首个未按键 move 兜底结束——同旧 dock 热区模式；钳制 [120px, 视口高 70%]，拖拽绕开补间、显示高与实高同步写实时重排，补间中途起拖从当前显示高续拖防跳变）。**②全部在 pig-app**：GUI 进程内直接 spawn PTY，不经 pig-core Op/Event（终端是用户本地 shell，与 agent 引擎无关；tty7 的 daemon/SSH/session 持久化/搜索/图片协议/boxdraw 全砍）。**③依赖全走 crates.io**：`portable-pty = "0.9"` + `alacritty_terminal = "0.26"`（官方版 API 与 tty7 的 zed-fork 0.26.1-dev 一致：`Term::new`/`Config{kitty_keyboard}`/`FairMutex`/`renderable_content`/`selection_to_string` 逐一核过）+ `unicode-segmentation = "1"`。**④模块**（`pig-app/src/terminal/` ~3000 行）：`pty.rs`（spawn/读写线程/resize/Drop kill；shell 用 portable-pty `new_default_prog` 自动 $SHELL→passwd + login argv，免 libc）→ `term.rs`（`FairMutex<Term>` + EventProxy 把 alacritty Event 转进 async-channel + reader 64KB 分批放锁）→ `colors.rs`（ANSI 16/256 亮暗两套 + 主题 fg/bg）→ `element.rs`（自绘 Element：快照 `try_lock` 拿不到画上一帧、背景 run 合并、分段 `shape_line` 带 force_width 等宽对齐、block 光标反色 530ms 闪烁、选区高亮、IME 预编辑下划线）→ `input.rs`（kitty CSI-u + legacy 键映射、bracketed paste、`InputHandler` IME）→ `view.rs`（事件泵/键鼠/滚轮 display_offset/退出标记）→ `mod.rs`（TerminalPanel 标签页栏，样式照 `render_right_tab_bar`）。**⑤gpui 适配要点**：`shape_line` 的 force_width 参数 gpui-pre 恰好保留（等宽对齐零改造）；tty7 fork 的 `prefers_ime_for_printable_keys` patch gpui-kit 没有 → Option-as-Meta 不做；portable-pty 的 ExitStatus 是自有类型 → 子进程退出统一发 `AlacEvent::Exit`；字体字号直读 `cx.theme().mono_font_*`。**⑥焦点**：终端自带 FocusHandle + `key_context("terminal")` 点击抢焦；收起时焦点还 composer 输入框（经 `Entity::update` 调 `focus_input`，避开 `read` 借用与 `&mut App` 冲突）。**⑦右面板菜单的「终端」占位项随之移除**（`right_menu_items` 5→4）。**⑧测试**：`terminal/term.rs` 两个真 PTY 端到端测试（输出→grid、写入→stdin、退出→事件）+ 键映射/粘贴单测，不进 PIG_SELFTEST。已知遗留：鼠标上报（MOUSE_MODE 点击/滚动编码）未实现、Windows ConPTY Ctrl+J APC 未做、tab 标签不跟随 OSC title。
>
> 终端面板迁移到官方 bottom dock（2026-10-04 二轮，用户指出官网 dock 自带 bottom 放置与拖动条）：**①上游能力核实**——gpui-base 0.7.0 的 bottom dock 是一等公民：布局上嵌在**中央列内部**（center 内容之下、左右 dock 之间，`dock_area.rs` 渲染树即「行[左 dock, 列[center, bottom dock], 右 dock]」），DockSkin 对所有 placement 自动画把手（`resize-handle-bottom`，吸顶 5px 带+pill 指示器），拖拽钳制 `[PANEL_MIN_SIZE(100), 区域高-100]`（`DockSizing`，底部无对侧 dock）。**②全隐语义**（与上游默认不同）：关闭 = `remove_dock`（上游 toggle 关闭会留 29px `CLOSED_BOTTOM_STRIP` 收起条，与标题栏按钮「开/关」模型不符）；同理禁用上游「拖到最小即收起」手势（`set_dock_collapsible(false)`，否则拖小后 dock 进入 closed 态、与标志位打架被动画重新拉开）。**③动画保留**：`step_dock_anim`/`apply_dock_flags`/`step_dock_anims_frame`/`mount_dock_edge` 全链路加 Bottom 分支（与左右 dock 同一匀速体系）；边缘段加垂直版 `render_bottom_dock_edge`——容器 = 中央列底部 100px 横带（让开左右 dock 当前占位），内容（固定目标高）从窗底垂直滑入，交接帧与 dock@100 帧像素一致（内容均锚顶）。**④面板实体常驻 AppView**（`dock_bottom_panel`）：dock 移除/挂回不随实体生灭，重挂同一 `panel_handle`。**⑤内容渲染双模式**（`render_bottom_dock_content`）：补间/边缘段固定目标高锚顶、dock 帧只裁剪（终端网格零 resize，防逐帧 SIGWINCH 重绘——与侧栏「内容锚定+裁剪」同手法）；稳态（含官方把手拖拽）填满 dock 帧实时重排（真实终端拖拽语义）。**⑥高度同步**：`terminal_h` 由稳态 render 从 dock 实高对齐（同 sidebar_w 模式）+ 关闭前同步一次（拖过的高度重开不丢）。**⑦删除手卷全套**：6px 调高热区、根容器 on_mouse_move 调高分支、`terminal_display_h` 幕布补间（`TermPanelAnim`/`step_terminal_anim`/`terminal_panel_h`）、render_center 挂载——中心区 v_flex 回归「消息流 + composer」两段。面板只在会话态可见（`terminal_visible = !hero && current.is_some()`）：切 hero 自动收起、回会话自动展开（各走一遍动画）。自测「终端面板开合 OK」在迁移后链路原样通过。
>
> bottom dock 把手拖不动修复（2026-10-05）：现象是终端面板能展开、官方把手拖不动。**根因不是把手**（headless 复现测试 `dock::tests::bottom_dock_handle_drags*` 以与 App 相同的 locked+collapsible(false)+懒挂载参数验证官方把手拖拽改高通过），而是 `mount_dock_edge` 展开收尾的「标志位仍在」判定用了 `_ => this.right_open` 通配臂——Bottom 落到右面板标志位上：右面板关闭时（默认）收尾判定「标志位已反向」提前返回，**底部 dock 永不挂载**，只剩边缘段覆盖层循环滑入——用户看到的「终端」其实是覆盖层里那 100px 内容条，自然没有把手可拖。修正：Bottom 臂用 `terminal_open && terminal_visible(cx)`。排查副产品：① `use super::*` 链会把 gpui 的 `test` 宏导进子模块遮蔽内置 `#[test]`，展开无限递归（thread_view/tests.rs 全是显式导入故无此坑）——dock.rs 测试模块已改显式导入并留注释；② gpui-kit 的 `TestWindowExt`（`window.drag`/`find`/`render_frame`）可对 headless 窗口做真实指针派发，观测注册表不含 resize-handle 系 id 是 test 模式现象，功能本身正常。
>
> 终端 shell 可配置（2026-10-05）：`AppConfig` 加 `#[serde(default)] terminal_shell: Option<String>`（向后兼容旧 config.toml；None = 系统默认，语义同原行为：unix $SHELL→passwd 登录 shell、Windows pwsh→powershell→cmd）。自定义路径以 login shell 启动（unix 显式追加 `-l`——`new_default_prog` 的 argv[0] `-` 前缀是 portable-pty 内部行为、自定义命令够不到；Windows 不盲加参数）；tab 标签取程序 basename。链路：`Pty::spawn(cwd, size, shell)` ← `TerminalPanel.shell`（`set_shell` 只影响之后新建 tab）← AppView 三处注入（toggle 创建/重开、`Event::ConfigSnapshot` 同步——设置页改完经 Save→snapshot 回流即对后续新 tab 生效）。设置页新增「终端」页（官方 `SettingField::input` getter 直读 config 免回填通道、setter 经 WeakEntity 写配置 + 独立 500ms 防抖代次——与供应商表单防抖分开；留空即 None）。pty.rs 补 2 测试（自定义标签/空白回退、login 启动回显）。**同日修正**：unix 默认 shell 的 tab 标签从「粗取 $SHELL」改为 `resolve_login_shell()`（pig-app 新引 `libc` 依赖，`access(X_OK)` 校验走 libc 与上游同口径）——与 portable-pty `new_default_prog` 内部 `get_shell()` 完全同链（$SHELL 可执行校验 → `getpwuid` 的 `pw_shell` 可执行校验 → `/bin/sh`），标签与真实起进程结果保证一致（原简化版在 $SHELL 失效时会分叉：进程回退 passwd、标签仍显 $SHELL）。再补一致性测试（$SHELL 有效时解析必须等于 $SHELL、结果必可执行、标签=解析 basename）。顺带答生命周期问题：终端 shell 是 GUI 进程的子进程，继承 App 环境（另注入 `TERM`/`COLORTERM`/`TERM_PROGRAM`/`PWD`），App 退出或关 tab 时 PTY 关闭 + 进程被 kill——跟随父进程。
>
> Read 工具卡重做 + 右侧文件查看器（2026-10-05，对齐 ZCode 截图）：**①语法高亮落地**——pig-app 的 gpui-kit 依赖开 `tree-sitter` + `tree-sitter-languages` 全语言包（之前 lock 里没有 tree-sitter，TextView 的 Markdown 代码块一直是纯文本，本次顺带点亮）；`tree-sitter-sequel` 锁 `cc ~1.2` 与既有 lock 的 cc 1.4 冲突，`cargo update -p cc --precise 1.2.67` 降级共存（rusqlite 的 `cc ^1.1.6` 兼容）。新模块 `code_view.rs`：`lang_name_for_path`（扩展名→gpui-kit `Language::from_str` 别名表，无扩展名按整文件名如 Makefile，不认识回落 "text" 惰性高亮器）+ `highlight_code`（`SyntaxHighlighter` 线程级按语言缓存，`update(None,&rope)` 全量 parse + `styles()` 出 `(字节区间, HighlightStyle)`，颜色取自 `cx.theme().highlight_theme` 随亮暗切换）+ `code_line_row`（行号 gutter + `StyledText::with_highlights` 延迟高亮行，wrap/nowrap 两态）。**②Read 卡**（thread_view/read.rs）：摘要行路径保持原有单一全文展示、可点击（`ThreadEvent::OpenFile{path,line}`，line=输出首行号；悬停高亮+下划线，tooltip 提示），完成后追加「N 行」计数；展开正文从通用输入+输出卡升级为代码卡——头部文件名 + 常显的换行/复制按钮，正文真实文件行号（解析 `{行号}\t{内容}` 输出格式，尾部 `[已截断]`/`[文件信息]` 标注行原样附后），限高 320px 内部滚动、超 600 行截断提示去右侧面板看全文；空文件/「未变化」/报错等非内容输出靠首行 `^\d+\t` 判定回落通用卡。解析+高亮结果缓存在段级 `ReadCardUi`（RefCell，主题 Arc 指针判等重算），复制走 `cx.write_to_clipboard` + 勾号反馈。**③右侧「文件」tab**（file_panel.rs）：`RightTab::File{path}`（canonical 绝对路径去重），再点同路径 = 重读刷新；后台线程读盘（≤8MB，`pig_core::text::decode` 自动转码 UTF-16/GBK）+ 后台线程高亮；不折行 = uniform_list 虚拟化（整文件）+ 横向滚动，折行 = 普通列表（行高不定，超 2000 行截断提示）；打开时滚到 Read 首行（`scroll_to_item(.., ScrollStrategy::Top)`）。**横向滚动坑（已用 headless 测试钉死）**：x 滚动容器内子元素不显式给宽会被布局钳进可用空间（max_offset.x 恒 0，ticker 第三次同款）——`measure_max_line_width` 按估计权重（tab=4/非 ASCII=2/其余=1）取前 3 行精确 shape 取最大（字重/字形经 TextRun 叠加），内容列显式设宽。uniform_list 的 `Unconstrained` 横向模式只量首行定内容宽，行级也得显式给宽。**滚轮轴锁定**：gpui 滚轮处理器默认把纵向 delta 映射到仅 x 可滚容器（y→x）、横向 delta 映射到仅 y 可滚容器（x→y）——Read 卡这种 x/y 嵌套滚动结构里滚轮一动两轴同滚；两层容器都加 `restrict_scroll_to_axis()`（另对 precise 触控板手势做轴向锁定）后各管各轴，回归测试 `read_card_scroll_wheel_is_axis_locked` 用 dispatch_event 直接派滚轮事件钉死（`window.scroll(id)` 依赖观测注册表、只认 test_support 包裹的元素，普通 div 用不了；滚动偏移 x/y 同号约定：向右/向下滚 = 负值）。文件面板不折行模式补了横向滚动条（`Scrollbar::horizontal` 直接收 UniformListScrollHandle）。**横向滚动条必须常显**（`.mode(ScrollbarMode::Always)`）：gpui-base Scrollbar 默认 `Scrolling` 模式只在滚动中/滚动后短暂显示、闲时淡出——轴锁定后鼠标用户失去「滚轮纵走顺带横滚」的（意外）入口，横向只剩拖条/Shift+滚轮/触控板，滚动条再隐身就等于没有横滚。**常显就要让位**：滚动条轨道高 16px（gpui-base WIDTH=4×2+8），不折行模式的内容底部预留 `CODE_SCROLLBAR_LANE` 车道（v_flex 末尾 pb / uniform_list 自身 `.pb()`——padding 只缩内容视口不进内容高度，末行可完整滚入），否则滚动条盖住末行（用户实测截图反馈）。滚动条在内容不超出时不渲染（scroll_area ≤ container 即跳过），预留车道在无需横滚时只是卡片底部一段空白。**④Bash 卡**（thread_view/bash.rs）：展开从「$命令 + 输出」通用卡升级为两张堆叠代码卡——命令卡（头部「Bash」，bash 语法高亮）+ 输出卡（头部「输出」，"text" 纯文本、失败红色），各带常显换行/复制按钮与横向滚动条；`code_view.rs` 为此补了 `code_line`（无行号行）与 `PreparedCode`（高亮+量宽打包，输出用 "text" 惰性高亮器只付量宽开销）。运行中/等审批仍走通用卡（实时输出），收尾才切代码卡。卡内容/高亮/量宽缓存挂段级 `BashCardUi`（与 ReadCardUi 同构）。自测坑：expand 卡片会加高列表内容把视口顶离底部，后续「贴底活动项」断言超时——`debug_expand_tool` 展开后须 `scroll_to_bottom` 回底。**滚动链坑**：卡的「能滚吞轮/不能滚穿透」由 `consume_scroll(handle)` 兜底（handle.max_offset>0 才 stop_propagation）——Bash 卡的子卡用独立句柄（cmd_scroll/out_scroll）而非段级 body_scroll，导致共享兜底恒不生效、滚轮穿透到消息列表双滚；修法是子卡框架各自 `on_scroll_wheel(consume_scroll(v_scroll))`。回归测试 `bash_card_scroll_traps_and_chains`：真实 ThreadView + reduce 事件构造展开卡，`dispatch_event` 往句柄 bounds 中心派滚轮——可滚输出卡滚动且外层列表纹丝不动；不可滚命令卡穿透给列表。注意：滚轮命中间接看 `mouse_position`（dispatch_event 只给 MouseMove/Down/Up 更新它，滚轮事件不更新）——派发滚轮前须先派 MouseMove 到目标点；滚动方向：delta.y 负 = 向下滚（贴底的列表再向下滚无位移，穿透测试要用正值向上滚）；append 消息含 deferred 贴底标记，测试里归零列表 offset 要先渲一帧消费标记。自测：解析/语言探测/行样式切分单测 + headless 横滚回归 + PIG_SELFTEST 文件 tab 加载断言（`debug_file_tab`）。

> 折叠/展开动画（2026-10-05 二轮）：消息流所有折叠点统一接入——工具卡（通用/diff/Read/Bash/Swarm）、思考块、turn 改动面板（汇总行 + 文件行两级）。段级状态 `ExpandAnim{generation, collapsing, measured_h}`（model.rs）：generation 每次开合 +1 作动画元素 id 后缀驱动重播；collapsing = 收起动画期间内容保持挂载（`drive_expand_anim` 落 250ms 计时器到期卸载，期间又展开的代次不符作废）。`expand_anim_wrap`（thread_view.rs）：外层 `overflow_hidden` + `with_animation`（200ms ease_out_quint）播 `max_h = 实测高×delta` + 透明度——内容高度用内层 `on_prepaint` 实测（clip/高度帽只作用在外层，内层始终按自然高布局；首帧未测到先 opacity(0) 挂一帧量高），**动画结束帧（delta 恰为 1.0，oneshot 停在那里）摘掉 max_h 帽**，超高内容不受残留限制；`cx.reduce_motion()` 由上游直接落到终态。测试坑：`bash_card_scroll_traps_and_chains` 在动画接入后挂了——headless 下动画按真实墙钟走，派发滚轮时 max_h 收在 ~0 把命中区裁没；须先渲一帧（量高）+ `thread::sleep` 睡过动画 + 再渲，且列表归零 offset 前要先渲一帧消费 append 期间攒下的 deferred 贴底标记。
>
> 侧栏去除分组视图、对齐 ZCode 头部按钮（2026-10-05 三轮）：**①结构**——删除「分组 | 工作区」分段控件与分组视图（置顶/任务两节），`SidebarView` 改为 `Flat`/`Workspace`（默认 Workspace）；列表上方新增固定「会话」标题行：**折叠/展开全部工作区**（`FoldVertical`/`UnfoldVertical` 随态切换 + tooltip，仅分组视图渲染；判定 = 任一工作区已展开则折叠全部）、**列表管理**（`SlidersHorizontal`，`dropdown_menu_with_anchor(Anchor::TopRight)` + `check_side(Side::Right)` 对齐 ZCode 右侧勾选）：视图二选一（平铺列表 `List` / 按工作区分组 `FolderKanban`）。「新建任务」保持顶部整行原样（曾并入头部 + 按钮，用户反馈后改回）。**②平铺列表**（`render_flat_view`）：跨工作区单一时间线，置顶在前（无小节头）、各按 updated 倒序；行 = 双行详情行（标题+时间 / 文件夹+工作区名）——原「置顶区行」通用化为 `render_detailed_session_row`（置顶区/平铺复用，跑马灯键仍带 `pinned-` 前缀与单行键隔离）。**③连带**：`render_session_row` 删 `show_time` 参数（分组视图删除后恒 true）；工作区视图删「工作区」小节头（截图直贴文件夹）；行内重命名兜底 `ensure_session_row_visible` 改退平铺列表。**遗留**：ZCode 菜单的「排序：手动排序/按最近活动」本期不做（手动排序需拖拽交互 + 顺序持久化到 core store，跨 crate 大功能；当前恒按最近活动），视图选择不持久化（重启回按工作区分组）。
>
> 已归档会话迁入设置页（2026-10-05 四轮，对齐 ZCode 设置页「已归档的会话」）：**①侧栏归档区移除**——三轮刚下沉到两视图底部的「已归档」折叠区整体删除，归档会话不再在侧栏渲染；行内重命名兜底改为「归档会话恒不可见 → 返回 false 放弃」（`ensure_session_row_visible` 改返 bool，输入框无行可承载），标题栏「⋯」菜单对归档的当前会话隐藏「重命名」入口。**②设置页新页**（`settings/archived.rs`，官方 Settings 组件 content_page 模式）：搜索框（按标题过滤）+ 工作区过滤 `Select`（「所有工作区」哨兵 + scope_workspaces 显示名；`set_items` 需 Window，选项重建走 `archived_ws_dirty` 脏标记 render 前同步——同 appearance_dirty 手法，选中项消失回退哨兵）+ 排序分段 pill（归档时间/创建时间/按字母顺序；**「归档时间」直接用 updated_at**——core `update_session` 归档时会刷新它，免协议改动；时间列显示跟随排序口径）+ 行卡（标题+相对时间 / 文件夹+工作区名 + 恢复 `Undo2` / 删除 `Delete` 按钮）。**③数据链路**：`AppView::sync_archived_page`（metas 过滤 archived + `workspace_display_name` 别名）在 `open_settings` 与 `refresh_sidebar`（settings_open 时）推送，`set_archived_sessions` 值变才 notify；动作经新增 `SettingsEvent::RestoreSession/DeleteSession` 回 AppView 走既有 `agent.set_archived` / `delete_session` 链路（删除同样不可恢复，与侧栏右键菜单同口径不二次确认）。空态区分「还没有归档的会话」/「没有匹配的归档会话」。
>
> 侧栏工作区开合动画（2026-10-05 五轮）：**①动画体系抽共享模块 `anim.rs`**——`ExpandAnim`（generation/collapsing/measured_h）、`EXPAND_ANIM_DUR`(200ms) 与自由函数 `expand_anim_wrap`（滑开/滑收 + 淡入淡出，内容实测自然高、结束帧摘 max_h 帽；`on_prepaint` 需 `gpui_kit::base::ElementExt`）从 thread_view 上移；thread_view 的同名方法改为委托，`model.rs` 经 `pub(crate) use crate::anim::{...}` 再导出（`use model::*` 不断链）。**②侧栏接入**：`Sidebar.expand_anims: HashMap<工作区路径, ExpandAnim>`（set_state 随工作区清单 retain）；点击文件夹行——展开 = `expanded` 插入 + gen+1 播滑开；收起 = 不立即移除，置 collapsing 播滑收，`EXPAND_ANIM_DUR+50ms` 计时器到期才从 `expanded` 卸载（期间再点开 = 「展开中」判定为 `expanded && !collapsing`，走展开分支 gen+1 作废旧计时器）。文件夹图标用 open（= expanded && !collapsing）即时反馈。会话块整块包进动画容器（v_flex gap_1 保持行距与原直排一致），元素 id `ws-expand:{path}:{gen}`。**③「折叠/展开全部」**：折叠 = 即时清 `expanded` + 取消所有进行中收起态（批量操作不播动画）；展开 = 逐个入组 + gen+1（顺带作废其卸载计时器）。**④分页「展开更多/收起」动画**（同日补，三易其稿后定型）：**根因**——分页收起把内容先瞬换成 5 行（多出的行瞬间消失），容器高度怎么渐变都不可见；正确形态是「旧行保持挂载、由收缩容器裁掉」（同文件夹收起）。**终案**：基础页（5 条）直排，「多出的页」独立子动画块（`Sidebar.paginate_anims`，同一 `ExpandAnim` 体系）——展开更多 = 子块滑开淡入（gen+1），收起 = 不立即改页数，子块 collapsing 播滑收淡出、`EXPAND_ANIM_DUR+50ms` 计时器到期才 `workspace_shown` 回一页（期间再展开代次不符作废）；外层工作区容器高度随内容自然跟随，无需任何 resize 状态（`ExpandAnim` 保持 generation/collapsing/measured_h 三字段，曾加的 resize_from/resize_gen 与 `h()` 固定高分支全部回退）。**失败方案存档**：resize_from 容器插值（max_h 帽 → h() 固定高）——高度确实渐变了但「行瞬换」依旧，肉眼仍读作无动画。**测试**：`sidebar/tests.rs` 集成测试（真实 Sidebar + `test_support` 观测点（正式构建无 feature 透传零成本，`window.click` 真点分页钮、`find(("ws-block",p_ix))` 量容器实高）断言收/放全程渐变与终态回退）。**测试环境三坑**：①`run_until_parked` 对挂起定时器视为 parked 立即返回，不能等墙钟；②`background_executor().timer` 走 **TestDispatcher 假时钟**，真 sleep 不推进，须 `cx.dispatcher.advance_clock(dur)` + `run_until_parked`；③`AnimationElement` 动画走真实 `Instant::now()`，视觉落定才用真 sleep——同进程两种时钟别混。**收起「最后顿一下」**（同日用户视频反馈）：根因是缓动方向——`expand_anim_wrap` 原先开合都用 ease-out（`1-(1-delta)^5`），收起时动作被压在前 40% 完成、剩余时间近静止爬尾，卸载计时器到期时读作「收完了干等再一顿」。修正：缓动改在闭包内按方向取（`Animation::new` 保持线性 delta）——展开 = ease-out（`1-(1-d)^5`，不变），收起 = ease-in-quad（`1-d²`，慢起步加速收尽、结束帧恰好归零无尾巴）；thread_view 卡片收起同受益于标准退出曲线。
>> turn 导航条横条接弹簧动画（2026-10-05 六轮）：横条的宽度（悬停山峰加宽 2.6x/1.7x/1.25x）与透明度（活动项/悬停/运行态强调）从瞬时切换改为 `with_spring` 弹簧过渡——每根横条内层透明度弹簧 `("turn-nav-bar-o", ix)`、外层宽度弹簧 `("turn-nav-bar-w", ix)`，元素 id 保持弹簧位置与速度，悬停移动/滚动换活动项时目标变化平滑接力（首次挂载直落目标值不空播，`reduce_motion` 由上游直落终态）。弹簧参数 `NAV_BAR_SPRING = SpringConfig(260, 30, 1)`（ζ≈0.93 近临界，无拖尾无明显过冲）；颜色仍是离散两档瞬切。坑：`with_spring` 的 animator 收到的是**调用点元素本身**（Div 直接 `.opacity()`），链式第二个弹簧收到的是 `SpringAnimationElement`（不实现 `Styled`），要 `.map_element(|d| ...)` 透到内层 Div 再设样式。
>
> 导航预览卡进出场动画（2026-10-05 六轮二）：**入场淡入**（160ms ease-out，仅「从关闭态新开」播——判据 `freshly_opened = nav_card.is_some() && nav_card_last.is_none()`，横条间切换不重播防扫过闪烁）+ **出场快照淡出**（160ms 线性）：关闭时把渲染数据快照（`NavCardData = (ix, bounds, 用户预览, 助手预览, is_text)`，`nav_card_last` 打开期间逐帧刷新）移入 `nav_card_exit`，按 `nav_card_exit_gen` 代次播一次淡出 + `DUR+40ms` 计时器到期丢弃。卡体抽 `nav_card_body`（打开卡/出场卡共用）。不做位移/缩放：Positioner 自绘层里动 margin/宽度会打架或重排文本，fade 已够用。连带：`render_turn_nav` 改 `&mut self`（快照要逐帧回写）；`gen` 是 Rust 2024 保留字，变量命名避开。**开合语义对齐 ZCode**（同日用户指出）：ZCode 每根横条是**独立 HoverCard**（`ConversationTurnNavigator.tsx`：openDelay 120 / closeDelay 80 在各实例上）——移到相邻横条 = 旧卡 80ms 关闭 + 新卡重新等 120ms 开卡，快速扫过永不弹卡；我们原是「一张共享卡跟手切换不关闭」。改为：80ms 关闭计时器的条件从 `nav_hover.is_none()` 改为 `nav_card.is_some_and(|open| nav_hover != Some(open))`——离开导航条与移到别的横条都走关闭（抽 `close_nav_card`），打开卡与出场淡出可短暂共存（不同横条位置，对齐独立实例观感），同一条快速回悬不会关（hover 复原即不满足关闭条件）。
>
> 升级 gpui-kit 0.7.0 → 0.7.1（2026-10-05）：patch 升级零代码改动，`cargo update -p gpui-kit` 直过（gpui-pre 快照 0.3.7→0.3.8、notify 7→8 随上游）。**核心收益是 #3293 落地**——表格行尾吞字（inline flow 全角标点量宽少算）上游按「整形后绘制宽度收紧重排」根治（gpui-base 0.7.1 `inline_flow.rs` 已核实），messages.rs 里「等 0.7.1+ 根治」的注释同步更新；scroll 表格布局保留不变（它防的是短列折行，与 #3293 无关）。**行为变更核对**：① Questionnaire 单选点击 = 确认并自动进下一题/提交（#3350），与 ZCode AskUserQuestion 观感一致、属白赚体验；自测链路走 `activate_choice`（官方明确保留「只改答案不确认」语义）不受影响，PIG_SELFTEST 全绿实测；② 图表首屏绘制动画默认开（`.appear(false)` 可关）——项目无图表，无影响。**直接受益的流式/渲染修复**：`TextViewState::set_text` 增量前缀复用 + 被新 chunk 超越的后台解析结果照常提交（#3294/#3344，流式 Markdown 重解析开销下降）、CJK 标点 inline flow 折行（#3293）、填充容器内富文本继承容器文字色（#3329，气泡场景）、GFM 表格缺列行归一（#3365）、Shimmer 暗色高亮可见性（#3328，思考/工具扫光用的就是 ShimmerText）、IME 候选框定位与键入撤销合并（#3297/#3341）。**备查的新能力（本期未用）**：语音输入 `speech` feature（SpeechState/SpeechButton/SpeechWaveform，#3333）；`Input/Textarea::on_token_hover`（#3346，可给 composer 的 @提及 token 挂悬停预览）；Markdown 代码块纵向滚动+高度帽（`TextViewStyle::with_code_block` 设 `overflow.y=Scroll`+max_h，#3322，自带滚动条且滚轮不穿透父列表）；`RenderedText::source`/`range_for_source`（#3281，源码区间→渲染区间高亮，会话内搜索可改用源码口径）；`DockArea::set_split_sizes`（#3314，原位恢复分栏比例）；ColorSelect（#3289）。验证：build/fmt/clippy 干净，`cargo test -p pig-core`/`-p pig-app`（63 个 headless）全绿，PIG_SELFTEST PASS。
>
> 上下文压缩分隔条（2026-10-05，对标 ZCode 截图）：压缩在消息流里的呈现从「整段摘要平铺成 muted 居中长文本」改为两态分隔行——进行中 `—— 正在压缩上下文 ——`（分隔线 flex_grow 铺满内容列 + ShimmerText 扫光 foreground 色，无图标），完成 `—— 🗄 上下文已压缩 ——`（Archive 图标 + muted，`render_compact_divider` 两态共用骨架）。**协议新增 `Event::CompactStarted`**（additive 变体，rollout 不受影响）：core 在 `run_compact` 过短历史早退之后、摘要阻塞请求之前发射——自动压缩（采样前水位触发）发生在回合内部，没有它 UI 对「回合卡住其实在摘要」零感知；手动 /compact 同链路白得。**UI 状态机**：`ThreadView.compacting` 由 CompactStarted 置位、ContextCompacted 清除，`TurnAborted` 兜底清除（摘要请求被打断时收不到 ContextCompacted）；`ChatMessage.system_kind: SystemNoteKind::{Plain,Compacted}` 区分渲染，**摘要全文仍留在系统条 text**——`debug_system_notes` 断言与排查链路不动；`omitted=0`（历史很短）仍走平铺文本。进行条挂列表末尾（工作中指示之后 = 最新状态），两处分隔条挂 test_support 观测 id（`compacting-divider`/`("compact-note", ix)`，正常构建零成本透传）。回放不重放 Compact 记录（维持原行为，压缩条只在 live 会话出现；同日下一条已改为回放补发）。测试：headless `compact_divider_progress_then_done`（进行条可见且铺满、完成切换、全文保留、TurnAborted 清标记）；自测 compact 段追加收尾后 `debug_compacting()==false` 断言（顺带治好该 debug 钩子 dead_code 告警）。坑：gpui-pre 0.3.8 的 `flex_grow` 要显式传 `f32`（`flex_grow(1.)`），与布尔式链式调用不同。验证：fmt/clippy 干净，pig-core/pig-app（64 个）测试全绿，PIG_SELFTEST PASS。
>
> 压缩三连修（2026-10-05 二轮，用户实测反馈）：**①重开会话丢分隔条**——回放曾直接跳过 Compact 记录；改为回放时在消息流同一位置补发 `ContextCompacted`（UI 复用 live 链路重建分隔条），rollout 记录加 `#[serde(default)] automatic/used_after` 渐进演进。**②压缩后容量不变**——`last_total_tokens` 只在真实采样响应时更新，手动 compact 后 chip 旧值常驻；更糟的是下一回合开头的水位检查拿压缩前的旧高值会**立刻又触发一次自动压缩**（把刚生成的摘要再压一遍，历史 ≤5 条时早退成「历史很短」也多一条噪音）。修复：重建历史后按 `estimate_history_tokens`（~4 字符/token + 每条 4，偏低估方向安全）重置水位、随 Compact 记录落盘（回放恢复，晚于它的 step_usage 真实值仍覆盖它）、并发 `ContextUsage` 让 chip 即时回落（模型未配置无窗口可报则跳过）。**③尾部裁切不安全（真实 bug）**——原逻辑「最后 4 条 + 只裁开头孤儿 tool」：窗口以 assistant 开头时 Anthropic 端点 400（first message must be user），OpenAI 兼容端点也会拿到语义断裂开头；自动压缩在长工具链中间触发时窗口全是 A/T，**当前回合的用户消息整条丢失**只靠摘要兜底。修复 `select_tail`：窗口裁到第一个 user 边界；窗口无 user 退化为只留最后一条 user 消息（当前请求必须原样保留）；`omitted` 按裁后真实条数重算（旧值按 KEEP 算，与裁后不符）。压缩后请求形态：Anthropic = 顶层 system（原系统提示+摘要拼接）+ user 边界的尾部消息；OpenAI = 两条 system + 同尾部；工具 schema 照常。测试：`select_tail`/估算 5 个单测；`compact_usage_resets_and_replays`（估算水位立即补发、下一回合不得重复自动压缩、重启回放含分隔条且水位为压缩后真实值）；`model_summary_compact` 期望值随 user 边界修正（omitted 2→4、历史 6→5——旧断言正好踩中 assistant 开头形态）。验证：fmt/clippy 干净，pig-core 全量（含 compact 5 个 + sessions 12 个）与 pig-app 64 个全绿，PIG_SELFTEST PASS。
>
> 摘要请求缓存对齐（2026-10-05 三轮，用户指出「压缩请求缓存命中率低」并查证两家作业）：**旧形态缓存全冷**——`complete_text` 把全部历史拼成单条大 user 消息（无 system、无 tools、每条还截断 2000 字符），与任何历史请求前缀都对不上，每次压缩全价输入且长 tool 输出有损。**两家参考实现**（explore 代理代码级核实）：ZCode 摘要走与正常请求同一条投影管线（`buildProviderRequestMessages`，历史原样 + 指令追加末尾 user + tools 照带 >100 才置空），并专门 `skipCacheWrite` 把 ephemeral 断点前移到摘要 prompt 之前、不污染缓存锚点；kimi-code 直接 `messages = [...同一 history 对象数组, 指令 user]` 落回冻结 prompt + 全量 tools，遥测专门统计 compaction 的 `input_cache_read`。**改造**：新 `provider::complete_messages`（非流式、完整 ChatMsg 列表 + tools、两家构建器复用；响应含 tool_calls/tool_use 视为失败落回截断兜底——指令已加「不要调用任何工具」）；`build_summary_messages` = 冻结 system + 历史**逐字原样**（废除逐条 2000 截断）+ 末尾指令 user；超预算预收缩从头丢整条并裁到 user 边界（kimi preShrink 同款，主要护手动 compact，自动按构造不超窗）。自动命名/连通性测试仍走旧 `complete_text` 不动。Anthropic 缓存：pig-code 正常请求路径本就不设 cache_control 断点（兼容端点支持度不一），结构性对齐后 OpenAI 系自动缓存直接受益，Anthropic 断点是另一个独立优化项未做。测试：`build_summary_messages` 3 个新单测（原样包裹逐条同内容、预收缩砍头裁 user 边界、全空只剩 system+指令）；既有 compact 5 集成测试原样过（mock 靠 stream:false 与 FAIL_COMPACT 识别，新形态兼容）。验证：fmt/clippy 干净，pig-core 22 套件 + pig-app 64 全绿，PIG_SELFTEST PASS。**同日修一处引入 bug（用户真实 Kimi Anthropic 端点 422）**：`complete_messages` 的 Anthropic 分支曾把 `root_schemas()` 的 OpenAI 线格式 tools（`{"type":"function"}`）直接透传，被端点拒绝（`unknown variant 'function'`）——流式路径一直有 `to_anthropic_tools` 转换，非流式漏了。修复并把两路的「工具清单组装」抽成共享函数 `anthropic_request_tools`/`openai_request_tools`（转换/透传 + 能力开启时的服务端搜索工具注入），流式与非流式同调一处防再漂移；provider.rs 补两个形态单测（不得残留 `function` 键/custom 不带 type/搜索工具追加）。注：mock 不校验 tools 形态，这类错误只能靠形态单测与真实端点暴露。失败时的行为（用户同问）：摘要请求失败走回退截断——会话不挂，历史 = 冻结 system +「共 N 条被省略（摘要生成失败，已直接截断）」note + user 边界尾部，模型丢中间细节但被明确告知「用 Read/Grep 重新查证，不要凭印象推断」，分隔条与水位重置照常。
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
