# Pig Code 统一插件系统架构

> 文档性质：目标架构与实现契约，不是当前实现说明。
>
> 最终方案：**签名插件包 + 按授权作用域隔离的独立插件 Engine + 版本化远程 UI/服务协议**。
>
> 本架构不依赖自定义 `cargo run` 宿主，不要求修改 NSH 源码，不手工维护 `.nsh/` 中的生成文件。插件安装、启用、禁用、更新和卸载不要求重新编译 Pig Code，也不要求重启应用。

## 1. 设计目标与最终决策

### 1.1 目标

统一插件系统必须同时支持：

- 运行时修改已有 UI：样式、图标、属性、子节点、包装、插入、替换和隐藏。
- 运行时新增 UI：页面、面板、菜单、命令、对话框、设置项和消息装饰。
- 运行时提供非 UI 能力：主题、工具、技能、MCP、配置、HTTP、凭据和宿主动作。
- 不重新编译宿主地安装、启用、禁用、更新和卸载插件。
- 插件代码与宿主代码隔离，插件异常不能破坏宿主 UI 或 Agent 会话。
- 同一插件可以通过一个包、一个身份、一个权限模型提供多种贡献。
- `nsh run` 与正式打包应用使用同一套插件实现和协议。
- 未来支持更多 UI 组件、更多宿主服务、更多执行后端，而不推翻协议和包模型。

### 1.2 最终决策

采用以下不可变架构决策：

1. **一种插件包**：所有插件都是带签名、清单、代码和资源的 `.nshpkg`。
2. **一种贡献模型**：UI、主题、工具、技能、MCP、配置和服务都是插件贡献，不再划分互不兼容的插件类型。
3. **一个权威注册表**：`pig-core` 中的 `PluginRegistry` 是安装、启用、授权、版本和贡献元数据的唯一权威来源。
4. **一个运行时协调者**：应用 crate 中的 `PluginSystemManager` 统一协调 Engine、UI、服务和生命周期。
5. **独立 Engine**：插件不进入宿主 VM，不直接访问宿主 Store 或组件对象。
6. **远程 UI**：插件不创建嵌入式 `AppView`，宿主根据远程 UI 操作创建真正的 NSH 原生组件。
7. **统一协议**：UI、事件、工具调用和宿主服务都使用带版本、实例、序号和请求标识的协议。
8. **最小权限**：签名证明来源，不等于授权；所有高风险能力按精确包摘要和作用域授予。
9. **可撤销**：禁用或卸载插件必须恢复宿主原始 UI，取消未完成请求并释放全部资源。
10. **不暴露 NSH 内部协议**：插件 API 使用 Pig Code 自己版本化的数据契约，`MountBatch` 只存在于宿主适配层。

### 1.3 非目标

明确不采用：

- 主 VM `eval`、动态修改宿主 TSX 源码或 monkey-patch 宿主组件函数。
- 通过 `testID` 模拟测试驱动来修改 UI。
- 动态加载原生动态库作为普通第三方插件。
- 把完整插件页面嵌入独立 `AppView` 作为统一 UI 方案。
- 插件任意读取宿主文件、密钥、会话、进程或组件对象。
- 插件修改权限审批、凭据、插件管理和安全提示等受保护 UI。
- 为每种贡献保留独立的安装、启用、授权和生命周期系统。

## 2. 总体架构

```text
                      签名 .nshpkg
                           │
                           ▼
              PackageVerifier / PackageInstaller
                           │
                           ▼
              pig-core :: PluginRegistry
              权威元数据、授权、摘要、作用域
                           │ PluginSnapshot + generation
                           ▼
              PluginSystemManager（应用 crate）
             ┌─────────────┼──────────────┐
             │             │              │
             ▼             ▼              ▼
      PluginEnginePool  UiCoordinator  HostServiceGateway
             │             │              │
       每实例独立 Engine   │              │
             │             │              │
       MountBatch          │              │
             │             │              │
             ▼             ▼              ▼
       MountProjector ──► 统一远程 UI 操作 ◄── BaseUiRegistry
                             │                    │
                             ▼                    │
                    RemoteRenderer / UiNodeFactory │
                             │                    │
                             └──────► NSH 原生 UI ┘
                                            │
                                            ▼
                                  用户输入与系统事件
                                            │
                                            ▼
                              EngineControl / HostChannel
```

### 2.1 核心数据流

**插件 UI 输出：**

```text
插件 TSX
→ 独立 Engine
→ MountBatch
→ MountProjector
→ Pig UI Protocol
→ UiCoordinator
→ RemoteRenderer
→ NSH 原生组件
```

**宿主 UI 被修改：**

```text
宿主 BaseUiRegistry 中的稳定语义节点
+ 插件 UiPatch 操作
→ UiCoordinator 按确定性规则合成
→ 更新响应式 UI 文档
→ 原组件逻辑 + 插件视觉/结构贡献
```

**用户输入：**

```text
NSH 事件
→ RemoteRenderer 标准化
→ UiEvent(instance, contribution, node, sequence, payload)
→ PluginSystemManager
→ EngineControl.dispatch_event
```

**工具与宿主动作：**

```text
插件 HostChannel 请求
→ HostServiceGateway
→ CapabilityBroker 校验
→ pig-core Op / 宿主服务
→ 结果校验与脱敏
→ 插件 HostReply
```

## 3. 模块职责与依赖方向

### 3.1 `pig-protocol`

只定义可序列化契约，不依赖 NSH、UI 或运行时：

- 包清单、身份、版本和摘要。
- 权限与作用域。
- 贡献描述。
- UI 文档、UI 操作和 UI 事件。
- 宿主服务请求与响应。
- 工具调用与结果。
- 生命周期和错误快照。
- 协议版本与能力协商。

协议类型必须支持向后兼容解析；新增字段使用默认值，新增必填能力不能静默通过旧宿主。

### 3.2 `pig-core`

负责与 Agent 同生命周期的权威状态和非 UI 服务：

- `PluginRegistry`：安装记录、精确摘要、启用意愿、授权、来源和作用域。
- `PackageRegistry`：内容寻址存储、安装、更新、回滚和卸载。
- `ContributionRegistry`：清单贡献与运行时贡献的统一索引。
- `CapabilityBroker`：每次服务调用的最终授权判断。
- `PluginToolset`：工具声明、筛选、调用、超时和取消。
- 配置、HTTP、凭据、技能、MCP 和工作区信任服务。
- 向 UI 发布带 `generation` 的原子 `PluginSnapshot`。

`pig-core` 不创建 NSH Engine，不持有 TSX 节点，不直接绘制 UI。

### 3.3 应用 crate：`src/plugin_system/`

负责 NSH 相关运行时：

- `PluginSystemManager`：唯一运行时协调者。
- `PluginEnginePool`：创建、复用、停止和回收 Engine。
- `PackageLoader`：在工作线程完成签名验证和解包。
- `MountProjector`：把 NSH `MountBatch` 投影为 Pig UI 操作。
- `RemoteEventRouter`：把宿主输入转换为插件事件。
- `HostServiceGateway`：连接 `HostChannel`、权限服务和 `pig-core`。
- `UiProtocolBridge`：向 TSX 发送有序 UI 批次，接收 UI 事件。
- `PluginRuntimeSnapshot`：运行中、失败、重试和资源状态，不作为安装权威状态。

### 3.4 TSX：`app/plugins/`

负责宿主侧响应式 UI 文档和渲染：

- `BaseUiRegistry`：宿主语义节点、受保护标记、原始属性和行为。
- `UiCoordinator`：合并基础 UI 与插件贡献，处理顺序、冲突和撤销。
- `RemoteRenderer`：把最终 UI 文档渲染为 NSH 组件。
- `SchemaRenderer`：渲染插件新增 UI 的受支持组件集合。
- `UiNodeFactory`：让已有 TSX 组件接入统一语义寻址，而非为每个按钮单独写插件接口。
- `PluginEventAdapter`：标准化输入、焦点、滚动、键盘和无障碍事件。

TSX 不决定安装、签名、授权和包更新结果。

### 3.5 状态所有权

| 状态 | 唯一所有者 | 其他模块持有的内容 |
|---|---|---|
| 安装、启用意愿、授权、摘要、作用域 | `pig-core::PluginRegistry` | 只读快照 |
| Engine 实例、启动纪元、运行错误、资源使用 | `PluginSystemManager` | 只读运行快照 |
| 基础 UI 节点与原始行为 | `BaseUiRegistry` | 不可变描述 |
| 当前合成 UI 文档 | `UiCoordinator` | 渲染结果 |
| 插件运行时注册 | 对应 Engine 实例 | 派生贡献，停止即删除 |
| Agent 会话与审批 | `pig-core` | 按权限投影的数据 |

运行时动态注册必须带 `(package, digest, instance, engine_epoch)`。Engine 停止后，这些注册不得残留。

## 4. 统一插件包

### 4.1 包格式

使用标准签名 `.nshpkg`，其中包含：

- NSH 应用元数据和签名信息。
- `pig-plugin.json`：Pig Code 插件清单。
- TSX 入口及其编译产物。
- 主题、技能、图标、图片、字体和其他资源。
- 可选的本地化文案与声明式配置。

包必须满足：

- `id` 同时作为 NSH app id 与 Pig 插件 id。
- NSH 与 Pig 清单中的 id、version、build 完全一致。
- 所有文件由签名和 SHA-256 文件表保护。
- 包摘要使用整个包字节的 SHA-256。
- `build` 在同一 app id 下严格递增，支持回滚保护。
- 包内不允许符号链接、绝对路径和 `..` 路径穿越。
- 主题包、工具包、UI 包不再使用不同的包格式。

### 4.2 清单示例

以下是契约示例，不代表现有代码已经实现：

```json
{
  "apiVersion": 1,
  "id": "com.example.moon",
  "name": {
    "zh-CN": "月亮界面",
    "en": "Moon UI"
  },
  "version": "1.0.0",
  "build": 1,
  "entry": "App.tsx",
  "minHostVersion": "1.0.0",
  "protocols": {
    "ui": 1,
    "services": 1
  },
  "capabilities": [
    {
      "name": "ui.patch.structure",
      "targets": ["composer.sendButton"]
    },
    {
      "name": "ui.add",
      "surfaces": ["main", "settings"]
    },
    {
      "name": "composer.submit"
    }
  ],
  "contributions": {
    "ui": [
      {
        "id": "moon-send-button",
        "kind": "replaceChildren",
        "target": "composer.sendButton",
        "export": "MoonSendButton",
        "preserve": ["onPress", "disabled", "accessibilityLabel"]
      }
    ],
    "pages": [
      {
        "id": "moon-settings",
        "surface": "settings",
        "export": "MoonSettings"
      }
    ]
  }
}
```

### 4.3 清单规则

- `apiVersion` 控制清单语义；`protocols.ui` 和 `protocols.services` 独立演进。
- 每个贡献都有插件内唯一 `id`；完整 id 为 `<plugin-id>:<contribution-id>`。
- `entry` 和所有资源路径必须是包内规范化相对路径。
- `export` 是插件 Engine 内的逻辑导出名，不是宿主文件路径或全局 JS 名称。
- 声明的能力必须同时满足 NSH capabilities、Pig capabilities 和用户授权，最终有效权限取三者交集。
- 清单声明能力不代表自动授予。安装后仍需按权限级别确认。
- 未识别的必需字段、贡献类型或协议主版本导致整个包拒绝加载。
- 未识别的可选字段可以忽略，但不能因此扩大权限。

### 4.4 来源与作用域

统一为：

- `BuiltIn`：第一方签名包，仍走同一 Engine 与协议。
- `User`：用户级安装，按全局授权运行。
- `Project`：项目级安装，必须位于项目插件目录，并同时满足签名验证和工作区信任。

实例标识：

```text
PluginInstanceId = package_id + digest + source + scope + workspace_id?
```

授权与启用都绑定实例和精确摘要。包内容变化后必须形成新实例，不能沿用旧实例权限。

## 5. 安装、启用与生命周期

### 5.1 生命周期状态

```text
Discovered
  → Verified
  → Installed
  → Enabled（启用意愿）
  → Preparing
  → Starting
  → Active
  → Stopping
  → Stopped

任意 Preparing / Starting / Active 阶段
  → Failed
  → Stopping / Retry / Disabled
```

安装和启用意愿可以持久化；`Starting`、`Active`、请求数和 Engine 地址绝不持久化。

### 5.2 原子激活

插件只有在以下步骤全部完成后才成为 `Active`：

1. 验证包签名、摘要、证书、吊销状态和构建号。
2. 校验清单、协议版本、资源和权限声明。
3. 创建实例授权上下文；需要执行代码的贡献创建独立 Engine，纯声明式贡献不创建 Engine。
4. 对需要 Engine 的贡献完成握手与协议协商。
5. 收集运行时贡献并完成 schema 校验。
6. 注册 UI、工具和服务贡献。
7. 提交同一 `generation` 的原子激活快照。

在此之前不得出现“部分 UI 已显示、部分工具已可用”的状态。

### 5.3 禁用与卸载

禁用按一个事务执行：

1. 标记实例停止接受新请求。
2. 取消工具调用、HTTP、订阅和未完成的 UI 请求。
3. 撤销该实例的 UI 修改并恢复基础 UI。
4. 删除运行时贡献。
5. 停止 Engine 和插件拥有的后台资源。
6. 发布同一 `generation` 的停止快照。

卸载在禁用之后执行。插件私有配置和私有凭据是否保留由用户显式选择；共享 Provider 凭据绝不随插件删除。

### 5.4 更新与回滚

- 新包先写入新的内容寻址位置，不覆盖运行中的旧包。
- 新实例使用新摘要、授权和 Engine epoch。
- 新实例通过健康检查后，在一个 UI 事务中切换贡献。
- 切换期间不得混用旧 UI 与新工具。
- 新实例激活失败时，旧实例保持不变或按原状态恢复。
- 已接受构建的回滚保护由信任存储保证，不能通过重装旧包绕过。
- 权限默认绑定精确摘要；更新不能静默继承高风险授权。

### 5.5 崩溃与僵死

- Engine 崩溃只影响对应实例，宿主撤销其贡献并保持其他插件运行。
- Engine 无响应时，所有调用有超时和取消路径。
- 自动重试采用指数退避和次数上限，禁止紧密重启循环。
- 达到上限后标记 `Failed`，等待用户手动重试或禁用。
- 宿主不持久化虚假的 `Running` 状态。

## 6. 独立 Engine 模型

### 6.1 Engine 创建

使用 NSH 公开能力：

- `nsh_engine::load_package` 或分离的 verify/accept API。
- `Engine::start` 启动独立 Engine。
- 自定义 `MountSink` 接收 `MountBatch`。
- `EngineControl` 转发事件、取消和生命周期控制。
- `HostChannel` 处理服务调用。

包哈希、验证和资源提取在工作线程执行；Engine 和 UI 操作按各自线程约束执行，宿主主线程不等待插件返回。

不需要 `AppView`，不需要自定义平台 `NativeComponent`，不需要修改 NSH。

### 6.2 实例隔离

- `User` 插件：每个启用实例一个 Engine。
- `Project` 插件：每个插件、每个工作区一个 Engine。
- 同一插件在不同授权作用域中不共享 VM、订阅、存储或请求状态。
- 多窗口共享同一实例的注册数据，但每个窗口有独立 UI surface 和节点映射。
- 会话数据按 `workspace_id`、`session_id` 和权限投影，禁止通过全局单例跨会话读取。

### 6.3 资源限制

每个实例都有独立的：

- VM 内存、执行时间和节点数量限制。
- UI 文档大小、单批操作数和消息队列上限。
- 请求并发数、工具超时和取消令牌。
- 网络响应、资源包和持久化存储配额。
- 日志速率和脱敏策略。

限制由宿主策略决定，插件清单不能提高硬上限。

### 6.4 声明式贡献与懒启动

纯声明式贡献在读取清单后即可注册，不必为静态资源常驻 Engine。包含代码的页面、工具或 UI 被激活时才启动 Engine。

懒启动是资源管理策略，不是另一套插件架构：所有贡献仍属于同一实例、同一权限和同一生命周期。

## 7. 远程协议

### 7.1 协议信封

所有消息使用统一信封：

```json
{
  "protocol": 1,
  "kind": "ui.commit",
  "instance": "com.example.moon:sha256:...:user:default",
  "epoch": 3,
  "stream": "ui",
  "sequence": 42,
  "requestId": null,
  "generation": 18,
  "body": {}
}
```

约束：

- `epoch` 在每次 Engine 重启时递增，旧 epoch 的消息一律丢弃。
- 每个 stream 的 `sequence` 严格递增。
- 请求和响应使用同一 `requestId`，并校验实例、摘要和服务名。
- `generation` 对应权威注册快照，防止启用/禁用竞态。
- UI 事件至多投递一次；不承诺分布式“恰好一次”。
- 请求超时、取消、实例停止后，迟到响应被忽略。

### 7.2 消息族

| 消息族 | 方向 | 用途 |
|---|---|---|
| `lifecycle.*` | 双向 | 握手、启动、停止、健康检查、失败 |
| `ui.commit` | 插件 → 宿主 | 原子 UI 操作批次 |
| `ui.ack` | 宿主 → 插件 | 接受版本、拒绝操作及原因 |
| `ui.event` | 宿主 → 插件 | 输入、焦点、滚动、键盘、无障碍 |
| `contribution.register` | 插件 → 宿主 | 动态页面、工具、主题、命令注册 |
| `contribution.remove` | 插件 → 宿主 | 删除运行时贡献 |
| `service.request` | 双向 | 类型化宿主服务调用 |
| `service.response` | 双向 | 结果、错误、取消 |
| `service.event` | 宿主 → 插件 | 状态订阅和变更通知 |
| `log.event` | 插件 → 宿主 | 结构化、脱敏日志与指标 |

### 7.3 顺序与原子性

- 每个 `ui.commit` 是原子批次，不能显示半个批次。
- UI 操作按实例和 stream 排序应用。
- `Create` 必须先于引用该节点的操作。
- `Remove` 后的节点不得再被同一批后续操作引用。
- `requestId`、`nodeId`、`contributionId` 在实例内唯一。
- 过期 revision、重复序号和非法操作返回结构化拒绝，不影响上一帧有效 UI。
- 激活、更新、禁用与卸载使用全局 generation，禁止跨代混合。

### 7.4 背压

- 所有跨线程通道有界。
- 高频 UI 帧可以按实例合并，只保留最新完整帧。
- 生命周期、权限、停止和错误消息不可丢弃。
- 输入事件按类型合并：鼠标移动和滚动可合并，点击、文本提交和键盘不可丢失。
- 队列满时触发实例级降级或暂停，不能阻塞宿主主线程。

## 8. 远程 UI 架构

### 8.1 不共享组件对象

插件不能获得宿主 `YNode`、TSX 函数、Store 或信号对象。双方只交换：

- 语义节点描述。
- 样式与属性数据。
- UI 操作。
- 事件数据。
- 资源引用。

宿主重新计算文字布局、字体、裁剪、滚动和无障碍，插件 Engine 的内部布局不作为最终屏幕布局。

### 8.2 宿主基础 UI 注册

已有 TSX 通过统一 `UiNodeFactory` 注册稳定语义节点：

```text
composer.sendButton
message.item
message.markdown
sidebar.sessionList
settings.navigation
```

规则：

- `uiId` 是长期兼容契约，不使用列表下标、DOM 层级或测试选择器作为公开地址。
- 注册信息包含组件类型、原始 props、子节点描述、宿主行为和受保护标记。
- 原始事件仍由宿主拥有；样式修改不会自动获得原事件数据。
- 重复 id、非法层级和受保护节点修改会被拒绝。
- 未注册节点不能被远程修改，避免整个应用成为无边界攻击面。
- 这是统一的 UI 可寻址机制，不是给每个按钮编写不同的插件回调。

### 8.3 插件 UI 投影

`MountProjector` 是唯一的 NSH 适配层：

- 读取公开 `MountBatch`。
- 把支持的 NSH 元素转换为 Pig UI 节点和操作。
- 丢弃插件内部像素几何，保留内容、样式、层级和事件标识。
- 将插件本地节点 id 映射为实例命名空间下的稳定 id。
- 将不支持的元素、原生组件和平台专有操作变成明确错误，不渲染成静默空视图。
- 防止插件伪造宿主节点 id、受保护节点或其他实例的贡献。

Pig UI 协议不直接暴露 `MountItem`，因此未来 NSH 内部格式变化只影响投影适配层。

### 8.4 UI 操作

统一支持：

| 操作 | 语义 |
|---|---|
| `create` | 创建插件拥有的节点 |
| `patch` | 修改属性、样式或内容 |
| `insert` | 在目标前、后、首、尾插入节点 |
| `move` | 移动插件拥有的节点 |
| `replace` | 用插件子树替换目标内容 |
| `replaceChildren` | 保留目标容器与指定行为，替换其子内容 |
| `wrap` | 用插件容器包装原节点 |
| `remove` | 删除插件节点，或隐藏允许修改的宿主节点 |
| `restore` | 撤销指定贡献的修改 |
| `commit` | 原子提交一组操作 |

默认保留原事件、禁用状态和无障碍语义。修改行为、拦截事件或改变禁用状态需要单独的 `ui.interact` 权限。

### 8.5 修改已有 UI 的示例

“把发送按钮改成月亮，但保留发送逻辑”应表达为：

```text
target = composer.sendButton
operation = replaceChildren
content = Icon(moon)
preserve = [onPress, disabled, accessibilityLabel, testID]
```

视觉和结构由插件贡献，提交仍调用宿主原有 `submitOrStop`。插件只有获得 `composer.submit` 时才能主动触发发送；仅有 `ui.patch.*` 不能读取草稿或提交会话。

### 8.6 新增 UI

新增 UI 通过统一贡献类型挂载：

- `route`：完整页面。
- `settings`：设置页面或设置分组。
- `panel`：侧边或底部面板。
- `command`：命令及命令对应 UI。
- `inline`：在指定语义节点前后插入内容。
- `overlay`：对话框、菜单、Popover 和 Tooltip。
- `decoration`：消息、工具和 diff 等内容的装饰层。

宿主控制最终窗口、焦点、层级和安全边界。插件不能创建无法关闭的全屏覆盖层。

### 8.7 冲突与排序

同一基础 UI 上的贡献必须具有确定性顺序：

1. 受保护规则优先，直接拒绝越权操作。
2. 用户显式设置的插件顺序。
3. 来源顺序：`BuiltIn`、`User`、`Project`。
4. 插件 id 字典序。
5. 贡献 id 字典序。

样式可以按该顺序合并；结构性互斥操作只选择一个赢家，并产生可诊断冲突。加载时间和线程调度不得影响结果。

宿主基础 UI 更新时：

- 保留仍匹配稳定 id 的插件贡献。
- 目标消失时停用对应贡献并记录，不使整个插件崩溃。
- 目标重新出现时按当前注册状态重新合成。
- 基础 revision 与插件 revision 不一致时拒绝过期操作，保留最后有效帧。

### 8.8 样式与主题

主题不再是独立插件运行模型，而是以下两类贡献：

- `theme.definition`：设计 token、字体、背景和明暗模式定义。
- `ui.patch.style`：目标节点的样式修改。

主题定义由统一 `UiCoordinator` 合成，支持：

- 命名主题和针对指定主题的 style pack。
- `light`、`dark`、`theme` 三类 token。
- 包内签名资源。
- 可撤销的字体、背景、圆角和组件样式。
- 插件间确定性的覆盖顺序。

动态 class 字符串不能直接成为任意行为注入渠道。高风险样式值、外部 URL 和脚本内容一律拒绝。

## 9. 输入、焦点与无障碍

远程 UI 必须是一等原生 UI，而不是只显示位图：

- 点击、长按、双击、鼠标移动和滚轮。
- 文本输入、删除、提交、组合输入法和候选窗口定位。
- 方向键、Tab、Shift+Tab、Escape 和快捷键。
- 焦点进入、离开和焦点恢复。
- 滚动位置、内容尺寸和可见区域。
- 无障碍标签、角色、值、状态和屏幕阅读器通知。

事件包含：

```text
instance + epoch + contribution + node + eventId + kind + payload + surfaceRevision
```

规则：

- 节点已删除或 surface 已更新时，过期事件被丢弃。
- 插件替换视觉内容时，默认保留原宿主无障碍行为。
- 插件不能伪造系统审批输入或向其他插件节点注入事件。
- 禁用插件后，焦点返回最近的有效宿主元素。
- 中文输入法、宽字符和选区必须在宿主原生组件中完成，不依赖插件内部测量。

## 10. 统一贡献体系

所有贡献使用相同 `(instance, contributionId, generation, order, enabled)` 元数据。

| 贡献 | 执行位置 | 宿主职责 |
|---|---|---|
| UI patch / page / panel | 插件 Engine + 宿主渲染 | 协议投影、权限、合成、事件路由 |
| 主题与样式包 | 声明式资源或 Engine | 校验、排序、撤销、资源映射 |
| 工具 | 插件 Engine | schema 校验、筛选、审批、超时、取消 |
| 页面命令 | 插件 Engine | 导航、动作授权、焦点和路由 |
| 技能 | 包内文档 | 信任、发现、加载和禁用 |
| MCP | 宿主进程管理器 | 配置校验、进程隔离、状态与生命周期 |
| 配置 | 宿主配置服务 | 类型、路径、作用域和持久化 |
| HTTP | 宿主 HTTP 服务 | origin、方法、重定向、凭据、脱敏和限额 |
| 凭据 | 宿主凭据管理器 | 注入而不暴露 secret、授权和清理 |
| 模型上下文服务 | 插件 Engine + core | 最小化模型信息、工具选择、结果校验 |

### 10.1 工具

工具声明至少包含：

- 名称、描述、JSON Schema。
- `readOnly` 和副作用说明。
- `availability = static | plugin`。
- 超时、结果大小和所需能力。
- 可选的模型动态选择入口。

工具调用携带：

```text
requestId + sessionId + workspaceId + instance + epoch + tool + args + cancellation
```

宿主必须：

- 保留现有审批链，插件工具不能绕过权限和沙箱。
- 校验工具选择结果只能包含已声明工具。
- 校验请求、结果和取消都匹配实例与 epoch。
- 对参数、结果、日志和持久化输出设置大小限制。
- 在插件禁用、会话停止或超时后取消调用。

### 10.2 技能与 MCP

- 技能是签名包内资源，按统一信任与启停规则发现。
- MCP 配置由宿主校验并交给独立进程管理器，不在插件 Engine 中隐式启动。
- MCP 进程与插件 Engine 是两类不同资源，但归属同一实例并同步停止。
- 项目插件提供的 MCP 必须满足项目信任和额外进程授权。

### 10.3 配置

- 插件私有配置使用声明的 key、默认值和 JSON 类型。
- 应用配置读写按精确路径授权，不提供任意对象遍历。
- 项目配置只作用于对应工作区。
- 配置变更通过宿主服务提交，并带 generation 和原子持久化。
- 密钥不是普通配置值，必须走凭据服务。

## 11. 权限、安全与信任

### 11.1 权限模型

最终授权：

```text
有效权限
= 包声明
∩ NSH capabilities
∩ Pig capabilities
∩ 用户/工作区授权
∩ 当前实例与 digest
∩ 当前服务及目标范围
```

建议权限族：

| 权限 | 含义 |
|---|---|
| `ui.patch.style` | 修改允许节点的视觉属性 |
| `ui.patch.structure` | 插入、替换、包装、隐藏结构 |
| `ui.interact` | 修改事件、禁用状态或交互行为 |
| `ui.add` | 新增页面、面板、命令和覆盖层 |
| `session.read` / `session.write` | 读取或修改指定会话投影 |
| `workspace.read` / `workspace.write` | 工作区数据和动作 |
| `composer.read` / `composer.submit` | 草稿读取和发送 |
| `tools.provide` | 注册和调用工具 |
| `config.read` / `config.write` | 指定配置路径访问 |
| `http` | 指定 origin、方法和凭据使用 |
| `process` | 指定的受限进程能力 |
| `fs.read` / `fs.write` | 指定虚拟路径或授权目录 |
| `credentials.own` | 管理插件私有凭据 |
| `credentials.provider` | 使用指定 Provider 凭据注入请求 |

### 11.2 受保护 UI

以下区域默认禁止第三方插件修改、替换、遮挡或伪造：

- 权限审批和沙箱扩大提示。
- 凭据输入、显示和授权页面。
- 插件安装、启用、权限和卸载管理器。
- 工作区信任和安全设置。
- 删除、卸载和危险操作确认。
- 宿主用于报告插件错误的安全兜底 UI。

第一方安全 UI 不通过普通插件协议实现，避免权限自举。

### 11.3 签名与授权分离

- 签名证明包来自受信任发布密钥，不代表插件内容安全。
- 根密钥、发布证书、吊销列表和构建号由 NSH 信任存储管理。
- 每个授权绑定 `id + digest + source + scope + capability + target`。
- 包内容变化后授权失效。
- 已禁用、已吊销或摘要不匹配的实例不能启动。

### 11.4 数据与密钥

- Provider 和插件 secret 只存在宿主凭据管理器。
- 插件只能请求“将 secret 注入指定请求”，不能读取 secret 值。
- 请求、响应、日志和错误中的 secret 必须脱敏。
- 禁止通过重定向把凭据带到其他 origin。
- 日志不得记录授权头、完整请求体或私有会话内容。

### 11.5 项目插件

项目插件同时需要：

- 签名信任。
- 工作区信任。
- 项目作用域授权。
- 对进程、MCP、文件和网络分别授权。

项目插件不能覆盖用户级安全策略，也不能通过 UI 修改诱导用户扩大权限。

## 12. 安装、市场与包管理

### 12.1 安装流程

1. 从用户选择、市场或受信任更新源获取包。
2. 限制目录、包、文件数量和单文件大小。
3. 在临时内容寻址区域完成下载和哈希校验。
4. 验证证书、签名、吊销、构建号和清单。
5. 校验包 id、版本、文件摘要与市场元数据一致。
6. 原子移动到内容寻址存储。
7. 写入安装记录，默认不启用高风险能力。
8. 发布新的原子 `PluginSnapshot`。

失败时删除临时文件，旧安装不受影响。

### 12.2 存储布局

逻辑布局：

```text
<data>/plugins/
  packages/<plugin-id>/<sha256>.nshpkg
  extracted/<plugin-id>/<sha256>/...
  instances/<instance-id>/storage.json
  instances/<instance-id>/cache/
  trust/
  registry.json
```

路径不是插件可访问 API。插件只能通过资源代理和受限存储服务访问数据。

### 12.3 市场

市场只提供发现与分发，不参与运行信任：

- 目录有独立 API 版本。
- 包元数据必须与实际签名包一致。
- 下载域名、重定向和大小受到限制。
- 市场不能绕过根证书、签名、吊销或用户授权。
- 自动更新默认关闭；启用后仍按权限变化要求确认。

## 13. 资源、存储与国际化

### 13.1 资源引用

使用逻辑 URI：

```text
plugin://<plugin-id>/<digest>/<normalized-path>
```

资源代理解析到已验证的解包目录，禁止：

- 任意绝对路径。
- 路径穿越和符号链接。
- 未声明的网络资源。
- 可执行 HTML、脚本或未经校验的原生内容。

### 13.2 私有存储

- 每个实例独立 KV 存储和缓存目录。
- 数据带有 schema 版本，迁移由插件自己负责。
- 项目实例与用户实例隔离。
- 缓存可删除，权威状态不能依赖缓存。
- 禁用插件保留数据，卸载时由用户决定是否清除。

### 13.3 国际化

- 清单名称、描述和 UI 文案支持本地化映射。
- 运行时使用数据而非长期保存翻译结果。
- 宿主区域设置变化通过服务事件通知插件。
- 缺失语言回退到 `en`，再回退到稳定贡献 id。

## 14. 性能与可靠性

### 14.1 线程约束

- 包验证和大文件处理不占用 UI 主线程。
- Engine 回调不得阻塞等待宿主回答。
- 宿主服务异步响应，支持超时和取消。
- UI 操作按帧批量提交，避免逐节点跨线程调用。

### 14.2 成本控制

- 静态贡献不启动 Engine。
- 不可见页面可以暂停绘制，但生命周期状态保持明确。
- UI diff 只提交变化部分。
- 大数据工具结果采用分页、截断或受控落盘引用。
- 日志、网络、订阅和节点数量均有实例配额。

### 14.3 故障隔离

以下故障不能影响宿主和其他插件：

- TSX 异常。
- Engine 崩溃。
- 非法 MountBatch 或协议操作。
- 工具超时。
- HTTP 失败。
- 存储损坏。
- 高频消息和内存超限。

宿主显示统一、不可被插件伪造的错误状态，并撤销失效实例的贡献。

## 15. 版本与兼容策略

### 15.1 三种版本分离

- 包 `version/build`：插件自身发布版本。
- `apiVersion`：清单和贡献语义版本。
- `protocols.ui/services`：运行时协议版本。

### 15.2 协商规则

- 主版本不兼容时拒绝启动，不尝试猜测执行。
- 次版本只增加可选能力。
- 插件和宿主在握手时交换能力位图和版本范围。
- 插件请求未支持能力时得到明确错误。
- 未知权限不会被默认授予。
- 过期实例和过期 revision 的消息一律拒绝。

### 15.3 UI 兼容

- `uiId` 是语义契约，重命名视为破坏性变更。
- 基础组件属性有显式 schema 和版本。
- 不支持的操作必须报错，不能静默变为空 UI。
- `MountBatch` 变化由 `MountProjector` 处理，不直接泄露给插件作者。

### 15.4 未来扩展

以下扩展不需要改变包和协议的基本模型：

- 新 NSH 组件适配器。
- 新宿主服务命名空间。
- 新主题 token 和样式属性。
- 新的签名市场与更新源。
- 将 Engine  transport 从进程内通道替换为进程外通道。
- 增加 WASM 等其他执行后端。
- 多窗口和远程工作区。

执行后端与 UI/服务协议分离，插件不需要知道宿主最终如何绘制。

## 16. 可观测性与调试

每个实例提供：

- 包 id、digest、来源、作用域和 Engine epoch。
- 生命周期、失败原因和最近重试。
- UI 节点数、操作数、拒绝操作和冲突赢家。
- 请求数量、超时、取消和队列深度。
- 内存、CPU、存储和网络指标。
- 经脱敏的结构化日志。

调试界面必须能够回答：

- 某个 UI 修改来自哪个插件和贡献。
- 多个插件修改同一节点时谁生效、为什么。
- 某个服务调用使用了哪项权限。
- 禁用后是否还有节点、监听器、请求或进程残留。
- 当前运行包是否与磁盘摘要一致。

日志和错误文本遵循结构化错误类型，UI 负责本地化，不把 secret 或私有会话内容写入日志。

## 17. 建议的代码组织

```text
crates/pig-protocol/src/plugin/
  mod.rs
  manifest.rs
  contributions.rs
  capabilities.rs
  lifecycle.rs
  ui.rs
  services.rs
  tools.rs

crates/pig-core/src/plugins/
  mod.rs
  registry.rs
  package.rs
  contributions.rs
  capabilities.rs
  marketplace.rs
  config.rs
  credentials.rs
  http.rs
  services.rs
  tools.rs

src/plugin_system/
  mod.rs
  manager.rs
  loader.rs
  engine.rs
  protocol.rs
  events.rs
  services.rs
  ui/
    mod.rs
    bridge.rs
    mount_projector.rs
    validation.rs

app/plugins/
  runtime.ts
  base-ui-registry.ts
  ui-coordinator.ts
  ui-protocol.ts
  ui-node-factory.tsx
  remote-renderer.tsx
  schema-renderer.tsx
  event-adapter.ts
```

依赖方向保持：

```text
pig-protocol
   ↑
pig-core
   ↑
应用 crate 的 PluginSystemManager
   ↑
TSX UI 协调与渲染
```

`app/store.ts` 只保存插件快照和 UI 状态，不再分别实现主题插件、工具插件和 UI 插件的生命周期。

## 18. 验证要求

### 18.1 包与安全

- 有效签名可以安装，错误根证书、篡改文件、吊销包和旧构建被拒绝。
- 路径穿越、符号链接、重复文件和超限包被拒绝。
- 摘要变化后旧授权失效。
- 项目插件在未信任工作区不能激活。
- 受保护 UI 的任何修改都被拒绝。
- secret 不出现在响应、日志或错误中。

### 18.2 生命周期

- 安装、启用、禁用、更新和卸载无需重新编译或重启。
- 激活不出现部分 UI、部分工具的混合状态。
- 禁用后基础 UI 完整恢复，运行贡献全部消失。
- 更新失败不影响旧实例。
- 崩溃、僵死和重启不会遗留旧 epoch 的 UI、请求或事件。
- 应用重启后不出现伪造的 Running 状态。

### 18.3 远程 UI

至少完整验证：

- 发送按钮替换为月亮图标，原发送、禁用和无障碍行为不变。
- 修改样式、插入节点、替换子节点、包装节点和隐藏节点。
- 新增设置页、主面板、菜单命令和消息装饰。
- 多插件修改同一节点时顺序确定，禁用其中一个后其余按规则恢复。
- 基础 UI 更新后插件修改仍能正确合成或明确停用。
- 中文输入、IME、滚动、快捷键、焦点和屏幕阅读器正常。
- 多窗口、多工作区和多会话的事件不会串线。

### 18.4 工具与服务

- 工具声明、动态选择、参数、结果和取消通过 schema 校验。
- 请求结果必须匹配 session、instance、epoch 和 requestId。
- 插件工具不能绕过现有审批和沙箱。
- HTTP 的 origin、方法、重定向、凭据和响应大小受到限制。
- MCP、技能和配置与插件生命周期一致。

### 18.5 性能与故障

- 高频 UI 更新不阻塞宿主主线程。
- 队列满时背压有效，生命周期消息不丢失。
- 插件内存、节点和输出超限时仅实例失败。
- 大输出和大量日志不会耗尽宿主资源。

标准检查包括 `nsh check && nsh test`、相关 Rust 测试和 macOS 真实窗口端到端验证。测试不得依赖 `.nsh/` 中的手工修改。

## 19. 最终验收场景

同一个签名插件在运行时完成以下行为：

1. 安装后通过权限确认并启动独立 Engine。
2. 把 `composer.sendButton` 的图标改为月亮，保留原发送逻辑。
3. 新增一个设置页和一个主界面面板。
4. 注册一个 Agent 工具、一个主题、一个技能和一个配置项。
5. 点击插件 UI，事件返回正确的 Engine，宿主状态按权限更新。
6. 禁用插件后，发送按钮恢复，页面和工具消失，后台请求取消。
7. 再次启用后，按已持久化的授权和配置重新激活。
8. 卸载后不再残留 Engine、UI、监听器、进程和运行时注册。
9. 在 `nsh run` 与正式打包应用中得到相同结果。
10. 全程不修改 NSH 源码、不重新编译宿主、不编辑 `.nsh/` 生成文件。

## 20. 架构底线

实现过程中不得重新引入以下隐性分支：

- 主题一套注册表、工具一套注册表、UI 另一套注册表。
- 插件生命周期同时由 core、Rust UI 和 TSX 分别决定。
- 插件直接操作宿主 Store、YNode 或 TSX 组件。
- 用加载先后顺序决定 UI 覆盖结果。
- 用包名而不校验 digest、实例和 Engine epoch。
- 用“签名通过”代替具体能力授权。
- 禁用只隐藏 UI，却不取消请求和释放资源。
- 为兼容旧实现而在主 VM 中执行插件代码。
- 为支持新组件而把 NSH 内部类型直接暴露成稳定插件 API。

这套架构的统一性来自：**同一包、同一注册表、同一实例模型、同一权限判断、同一生命周期，以及同一套版本化远程协议。**
