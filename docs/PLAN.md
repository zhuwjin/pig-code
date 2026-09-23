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
