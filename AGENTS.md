# pig-code 工作区说明

图形化 AI Code Agent 桌面应用：Rust + gpui-kit（crates.io 版本）单进程双执行域，界面对标 ZCode。**动手前先读 `docs/PLAN.md`**——它是唯一的设计决策与实施记录（含历次「打磨」笔记与上游 bug 跟踪），改敏感区域（dock 布局、MCP、Windows 子进程、流式渲染）前必看相关段落。

## 常用命令

```bash
cargo build                          # 全 workspace 构建（dev profile 对依赖开 opt-level=3，勿删）
cargo run -p pig-app                 # 启动 GUI（二进制名 pig-code）
cargo test -p pig-core               # 引擎集成测试（tests/ 目录，全绿才收工）
cargo test -p pig-core --test <name> # 单个测试文件，如 --test bash、--test mcp（按文件名）
cargo fmt && cargo clippy            # 无自定义 rustfmt/clippy 配置，用默认
PIG_SELFTEST=1 cargo run -p pig-app  # GUI 全链路自测（内置 mock provider + 临时隔离数据目录）
cargo run -p pig-core --example mock_provider  # 手动测试用独立 mock 服务，按打印的 base_url 配 ~/.pigcode/config.toml
```

## 架构与分层（红线）

三个 crate，依赖方向单向：`pig-app → pig-core → pig-protocol`。

- **pig-protocol**：纯 serde 类型（`Op` UI→core 命令、`Event` core→UI 事件、配置模型），零业务逻辑、零重依赖。改协议时保持向后兼容（rollout 里存着旧记录，用 `#[serde(default)]` 渐进演进）。
- **pig-core**：agent 引擎，跑在独立线程的 tokio runtime（入口 `spawn_agent`），与 UI 通过 `async-channel` 交换 Op/Event。**core 严禁 import 任何 gpui 类型**。核心设计：delta 事件（TextDelta 等）仅用于即时渲染不落盘，Done 事件携带全量值是 durable 边界；审批 = core 发 `ApprovalRequested`（带 request_id）后阻塞等 UI 回 `ApprovalReply`；会话持久化为 JSONL rollout（首行 meta + 每行一 item）。
- **pig-app**：gpui-kit GUI，smol executor，`cx.spawn` 循环读 Event channel 后 reduce 进各 Entity 视图状态。

## 代码约定

- **实现 UI 前先查官方组件**：gpui-kit 组件目录见 https://gpui-kit.com/component （全部 77 个组件的用途速查表：[docs/gpui-kit-components.md](docs/gpui-kit-components.md)）。有现成组件不自研、用法照抄官方 story；确需自研的（如 diff 视图），先在 `docs/PLAN.md` 记录原因与上游跟踪。
- **全仓中文**：注释、提交信息（`feat:`/`fix:`/`refactor:`/`docs:` 前缀 + 中文描述）、文档均用中文。
- **提交前 fmt/clippy 必须干净**：`cargo fmt --check` 无 diff、`cargo clippy --all-targets -- -D warnings` 零警告（2026-09-30 已全量清零，保持住）。结构性 lint（如 `too_many_arguments`）按既有惯例 `#[allow]`，不为 lint 做伤筋动骨的重构。
- **模块拆分惯例**：单文件超 ~1000 行即拆为同名目录 + 子模块（见 provider/、agent/、session/、thread_view/、composer/、sidebar/、settings/、task/、terminal/）。拆分坑：`use super::*` 会连带父模块的 `as _` trait 导入（子模块自己的导入反而 unused）；`pub(crate)` glob 会把可见性压到 crate 内——原 pub 项需显式 `pub use` 恢复。
- 日志用 `eprintln!`（无 log crate；Windows GUI 子系统下写入被静默忽略，安全）。
- 数据目录 `~/.pigcode`（`PIG_DATA_DIR` 环境变量可覆盖，自测靠它隔离）；配置 `~/.pigcode/config.toml`；MCP 配置用户级 `<data_dir>/mcp.json` + 项目级 `.pigcode/mcp.json`（项目覆盖用户同名条目）。

## 平台与上游坑

- **License 红线**：gpui-kit 只走 crates.io，绝不切 zed gpui 的 git 依赖（会拉入 GPL-3.0）；zed `agent_ui` 只能读设计不能抄代码。
- **Windows**：release 是 GUI 子系统（`windows_subsystem = "windows"`），**所有子进程 spawn 必须经 `NoConsoleExt`（CREATE_NO_WINDOW）**，否则 git/cmd 每次调用都弹新控制台窗口；MCP stdio 启动必须走 `resolve_program`（npx/pnpm 等是 .cmd 垫片，裸 `Command::new` 找不到）。
- **gpui-kit 0.x API 会变**：升级前看 changelog，组件用法照抄官方 story；已知上游渲染 bug（如 #3293 表格全角标点行尾吞字）记录在 PLAN.md——遇到渲染异常先查是否上游问题，别在下游打补丁（例：表格 CELL_PAD_PX 参与列宽测量，改 padding 治标会引发别处折行）。
- serde_json Map 迭代序受 preserve_order feature 影响，需要稳定顺序（如 resume 键）时显式 sort。
