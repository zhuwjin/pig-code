# Pig Code core 架构目标

将 Pig Code core 重建为单一、可恢复、可扩展、可持续演进的 Agent 核心，使其控制面、执行生命周期和核心子系统架构达到 Codex、Kimi Code、DeepSeek Harness 当前主分支所体现的底层能力水平。

允许 clean-slate 重写，不以工作量、内部 API/schema 兼容或保留现有实现为约束。完整产品功能可以后置，但所有已存在的 core 模块必须纳入验收；所有预留扩展点必须通过真实或 test-only 的代表性实现证明，不能只存在抽象名称。

“达到同等水平”只由本目标的固定验收矩阵判定，不使用主观综合评价，也不要求功能数量完全相同。

## 一、参考基线与验收矩阵

1. 目标启动时，将 Codex、Kimi Code、DeepSeek Harness 更新到各自最新默认分支，记录完整 commit SHA；之后固定这些 SHA，不持续追逐上游变化。
2. 盘点 crates/pig-core 中所有一级模块和公共核心接口。每个模块必须归入验收矩阵至少一行，或在本目标中被删除；最终不得存在“未分类、未审查”的 core 模块。
3. 验收矩阵至少覆盖：
   - Agent/Run/Attempt 与控制面。
   - Session、对话日志、fork/resume/replay。
   - Provider、模型能力和流式协议。
   - 工具调用、工具结果、副作用和取消。
   - 权限、审批、workspace trust、sandbox。
   - Shell task、子进程和后台任务。
   - Mailbox、通知、outbox 和恢复。
   - Scheduler、容量、reservation 和 watchdog。
   - Context、compaction、prefix/cache identity。
   - Media、附件和模型输入能力。
   - 控制事件、投影、UI/SDK 重同步。
   - Plugin/SDK、隔离存储和生命周期。
   - 配置、持久化、错误处理和可观测性。
   - 后续领域功能与 backend 的扩展边界。
4. 每行记录：参考实现的具体代码语义、Pig 对应实现、自动化测试、最终验证结果。若 Pig 的目标超过参考项目，也必须明确写出自身不变量，不能因为“参考项目没有”而跳过。
5. 每行状态只能为 unimplemented、implemented、tested、verified。只有全部 verified 才算完成；implemented 或“类型已存在”不是完成证据。

## 二、重写与交付边界

1. 允许修改或替换仓库内任何模块，包括 AgentRuntime、Session、run_step、Provider、工具、权限、sandbox、Backend、IPC、SQLite/JSONL、compaction、media、投影和必要的 UI/SDK 接入。
2. 开发期内部协议和持久化格式可以直接替换，不做旧数据迁移，不保留旧格式兼容分支，不以长期双写作为终态。
3. 不得删除用户数据；所有测试使用隔离临时目录。
4. 不得改变与 core 重构无关的产品行为；#5/#7 的已有行为不得回归。
5. 允许提交和推送专用验收分支、更新 issue #9；不得合并 main、关闭 issue #9、发布版本或部署产品。
6. 完整 ACP/Codex/Claude Code/SSH/容器 provider、Goal/Cron/Workflow/Team 产品、全量 token 持久化和性能调优可以后置，但相应扩展能力必须由代表性实现证明。

## 三、Agent Runtime 必须成立的终态

### 1. 唯一控制权威

- Agent、Run、Attempt、parent/lineage 只有一个权威来源。
- root/child、前台/后台、Agent/AgentSwarm、Local/外部执行全部经过同一 AgentRuntime 控制入口。
- 旧 Registry、AgentNode 权威状态、旧通知路由和 Agent TaskEntry 权威状态全部删除。
- command loop 串行提交控制事实，执行保持并发，不持锁跨 await。
- spawn/send/resume/stop/delete、重复 settlement、迟到事件和容量限制均返回确定结果，不遗留 running 项、writer、reservation 或执行句柄。

### 2. 身份与 ExecutionHandle

- Agent resume 创建新 Run；Attempt reconnect 保持同一 Run；detach 只断开观察；stop_run 请求终止。
- Backend 启动后返回长期 ExecutionHandle，明确 send、stop、reconnect、events/result 和 release。
- 不支持的能力在启动前拒绝。
- Run settlement 与 Execution release 分离；release 必须等待真实 quiescence，重复 dispose/release 幂等。
- 旧 attempt/epoch 不能结算或覆盖新 attempt。

### 3. 持久化与 mailbox

- SQLite 事务保存控制状态、mailbox、outbox、delivery receipts、控制事件和恢复元数据；JSONL 保存模型历史。
- 多次 Run 历史和完整 parent 关系可跨重启恢复。
- steer、follow-up、通知统一进入持久化 mailbox，具有 queued→claimed→consumed 状态。
- 消费确认对应 JSONL 中已同步的同一 message ID。
- 故障测试覆盖入队后消费前重启、JSONL 已写而 SQLite 未确认、TaskWait 竞争、父 Agent 退出/取消/恢复、root fallback 重启重复调度。
- 所有场景中模型可见消费恰好一次，不丢失、不重复唤醒。

### 4. 停止、删除与资源清理

- 正常、排队中、运行中、嵌套和后台 Run 都有确定的 graceful/force stop 结果。
- 对忽略取消的 provider/tool，删除不能永久卡住，也不能提前清理仍被使用的文件。
- pending deletion 和清理失败可恢复、可重试，错误结构化且可观测。
- 最终无孤儿进程、writer、reservation、pending deletion 或未释放 execution。
- 固定 seed 的 spawn/send/stop/delete/restart 混合 stress 连续运行 10 次通过；每轮后 Registry、mailbox、journal 有界且无未结算执行。

### 5. 控制事件、投影与重同步

- Catalog、任务面板、UI cards、SDK/plugin views 从同一控制事实和 run/sequence 派生。
- 重放、重复、乱序和旧 epoch 事件不会回滚状态或产生重复卡片。
- 快照携带 epoch/sequence，后续事件严格从该切点继续。
- cursor 可重放则按序补齐；过期则返回 ResyncRequired 和一致快照。
- 高频流式事件不做全量持久化。
- 新投影可独立注册 pure fold、snapshot、watermark 和 version，不修改权威 Registry。

### 6. 真实跨进程 backend

- 至少一个独立 worker 的真实非 Local backend 进入生产主路径，不是测试旁路。
- 真实验证 start、输入、send/steer、能力拒绝、输出和 terminal result。
- 真实网络断线、root 进程重启和 worker 进程重启后可 reconnect/reconcile，并按 cursor 去重补发。
- 断线不直接套用本地 Lost；旧 attempt 不能结算新 Run。
- 显式 stop 停止远端执行；release 等待真实资源释放。
- deterministic fake backend 合约套件覆盖故障注入、乱序、重复和旧 epoch。

## 四、整个 core 的架构验收

### 1. Session 与模型历史

- 任何进入模型请求的输入都可从持久化日志重建，做到 model-visible ⟺ logged。
- fresh/fork/resume/replay/compaction 使用稳定身份和连续事件序列。
- 崩溃后的未完成工具调用、turn usage、partial output 和 unknown outcome 有确定恢复语义。
- Session 不再是 Agent 树、后台任务、通知路由或 UI 卡片状态的第二权威。

### 2. Provider 与模型能力

- 所有已注册 Provider 通过统一合约套件：流式输出、usage、reasoning、media、structured output、取消、超时和业务错误。
- 网络重试、空响应、malformed stream、length/content-filter finish 和断连分类明确。
- 不默认重试含副作用的整个 Run；重连与重试遵守幂等和游标语义。
- 新 Provider 能通过注册与合约实现接入，不修改 Agent Runtime 或 Session 特例。

### 3. 工具、副作用与进程

- 工具具有 typed schema、结构化错误、取消、超时、输出上限和持久化结果。
- 副作用工具发生传输/崩溃故障时区分 completed、failed、unknown，不静默重复执行。
- 并发只读和串行写入策略在真正执行点强制。
- Shell task 与 AgentRun 分离；进程输出有界，spill 可恢复。
- graceful/force stop 等待真实进程树 quiescence，重启后的 Lost 语义确定。

### 4. 权限、审批与 sandbox

- 权限决策在实际执行点强制，不能仅靠 prompt、UI 或工具描述。
- workspace trust、读写根、网络、凭据和 escalation 采用 fail-closed 语义。
- ApprovalReviewer 请求包含 session/agent/run/turn/call、结构化 action、cwd 和权限快照。
- Allow、Deny、DeferToUser、Abort，以及 timeout、取消和 reviewer 不可用策略均有测试。
- User 与确定性 Policy 可用；Agent reviewer 上下文只读且禁止递归审批。
- 三平台相关路径和 sandbox 合约测试通过。

### 5. Context、compaction 与 media

- compaction 前后模型历史、工具调用和 usage 可重放，前缀/cache 身份稳定。
- context overflow 的 reactive recovery 不重复副作用，不丢失模型可见输入。
- 媒体类型、大小、能力和附件持久化经过验证；fork/resume/replay 后引用一致。
- 新 context/media 能力通过声明和投影接入，不修改控制面权威状态。

### 6. Plugin、SDK、配置与可观测性

- Plugin/SDK 使用 typed、versioned、capability-scoped 合约，资源 dispose 幂等。
- 插件存储具备原子性、作用域隔离、幂等重放和必要的数据保护。
- 插件事件和 Agent 视图来自同一投影，不建立旁路权威。
- 配置在最早边界验证，错误清晰；凭据和敏感数据不进入普通日志。
- 核心错误包含必要的 session/agent/run/attempt/call 身份；不得静默吞错。
- 日志、队列、Journal 和历史保留有界，清理错误可聚合和重试。

## 五、后续功能的同等架构门槛

1. 必须通过代表性实现证明以下扩展，而不是只定义 trait：
   - 新 Provider。
   - 新工具。
   - 新 Agent backend。
   - 新 Catalog/SDK 投影。
   - 新 mailbox 消息类型。
   - 新 initiator。
   - test-only Cron/Goal 领域触发器。
   - 一个新权限或审批策略。
2. 每个扩展原则上只新增领域模块、注册/组合、投影和测试；不得修改 Agent Runtime 内部状态，不得增加 Session/run_step 的领域特例，不得建立第二套持久化或通知路径。
3. 每个扩展必须通过相应公共合约套件，并验证适用的取消、错误、持久化、replay、quiescence、权限和投影行为。
4. 在 CI 中建立架构回归检查，至少防止：
   - 领域模块反向修改或拥有 Runtime 权威状态。
   - 新 backend/provider 的 Session 特例。
   - 新旧控制面双写。
   - 绕过权限执行点。
   - 未登记的投影或旁路通知。
   - 无界队列、未释放资源和被忽略的核心错误。
5. 后续完整产品功能可以尚未实现，但接入它们所需的扩展点和合约测试必须已经存在并通过上述代表性实现验证。不得承诺“未来任何实现自动正确”，只保证无需再次推翻核心架构，并且架构回归门槛持续生效。

## 六、最终命令与交付证明

1. #5/#7 的 fresh/fork/resume、嵌套后台、通知、恢复和删除行为全部不回归。
2. 不得通过删除、忽略、跳过或削弱测试获得通过；内部语义变化可更新测试，但必须保留等价或更强覆盖。
3. 最终提交上以下命令全部退出码 0：
   - `cargo check --all-targets`
   - `cargo test --workspace`
   - `nsh check && nsh test`
4. 最终推送的同一 commit SHA 在 macOS、Windows、Linux CI 全部成功，Rust/NSH 必需步骤不得跳过。
5. 推送专用验收分支，记录最终 commit SHA。
6. 更新 issue #9，提交：
   - 固定参考 SHA。
   - core 全模块盘点及验收矩阵。
   - 最终 commit。
   - 每项测试名称或链接。
   - 本地命令结果。
   - 三平台 CI URL。
   - 代表性扩展与未来功能准入门禁的证据。
7. 工作区最终干净，无临时测试产物；issue #9 保持打开，等待用户审查关闭。

## 七、迭代方式

按可运行垂直切片推进：
行为基线与 core 全模块盘点→新核心模型和公共合约→Agent Runtime 与 Local 全路径→mailbox/stop/delete→Provider/工具/权限/进程→context/compaction/media→统一投影与 Plugin/SDK→真实 IPC→代表性扩展与架构门禁→stress 和全平台验收。

每个切片先建立失败测试，再实现并运行相关检查。验收矩阵按行推进，只有真实测试通过才从 implemented 进入 tested，最终全量验证后才进入 verified。
发现旧控制路径仍在服务时优先迁移或删除，不叠加第三套状态。任一最终检查失败，就回到对应矩阵行修复，直到所有证据同时成立。

## 八、停止规则

1. 只有 core 全模块盘点无遗漏、验收矩阵全部 verified、最终命令退出码 0、同一最终提交三平台 CI 全绿，才可宣布目标完成。
2. 如果推送/CI 所需权限、网络或外部服务在标准排查后仍不可用，停止并报告确切命令、错误、已完成证据和剩余阻塞，不得宣称完成。
3. 如果必须修改或削弱验收矩阵、依赖未提供的凭据或付费外部服务、合并 main、关闭 issue、发布版本，或修改仓库外系统才能继续，停止并请求用户决定。
4. 技术困难、重写范围扩大、core 盘点增加矩阵行或测试反复失败本身不是停止理由。
5. 不设置 turn 或 token 上限；目标只在全部证明成立或遇到上述真实阻塞时结束。
