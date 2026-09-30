# gpui-kit 官方组件速查表

> 依据 https://gpui-kit.com/component 逐页核对整理（2026-09-30，对应 gpui-kit 0.7.0）。
>
> **规则：实现任何 UI 之前，先在此表（或官方目录）确认没有现成组件；有就直接用、用法照抄官方 story。确需自研的（如 diff 视图），在 `docs/PLAN.md` 记录原因与上游跟踪。**

## 聊天 / Agent 界面专用

| 组件 | 用途 | 关键特性 |
|---|---|---|
| [Message](https://gpui-kit.com/component/message) | 单条聊天消息的行级布局原语：对齐 + 头像/头部/内容/底部插槽；发送者、时间戳等数据由应用组合填充 | Start/End 对齐；类型化插槽；MessageGroup 连续消息堆叠；任意富内容；官方明确覆盖助手响应、流式生成与系统通知 |
| [Bubble](https://gpui-kit.com/component/bubble) | 消息气泡外观基元：对齐、最大宽度（默认父级 80%）、反应区；不持有消息数据 | 七种变体（Filled/Secondary/Muted/Tinted/Outline/Ghost/Destructive）；BubbleGroup 按发送者堆叠；BubbleReactions 附着点赞/复制/回复；折叠 Show more、加载态 |
| [MessageScroller](https://gpui-kit.com/component/message-scroller) | AI 聊天会话滚动容器：跟随最新消息、插入历史时保持锚点、跳转未读、暴露是否离底 | append/prepend/splice 与流式行高重测；is_following_tail 供自定义「回到底部」提示；无障碍实时播报新消息 |
| [TextView](https://gpui-kit.com/component/text-view) | 格式化文本渲染（Markdown + 简单 HTML），内置文本选择 | `push_str` 增量追加 + `stream_fade` 流式渐显；max_lines 截断展开；范围高亮与滚动定位（搜索/引用）；代码块自定义操作；插件扩展 |
| [Attachment](https://gpui-kit.com/component/attachment) | 聊天/输入区单附件展示（图片或图标+名称+大小+操作），文件状态由应用持有 | 五种附件状态（待传/上传中/处理中/失败/完成）驱动视觉；AttachmentGroup 横向滚动 + 边缘渐隐 |
| [Marker](https://gpui-kit.com/component/marker) | 聊天/Agent 状态行原语：状态文本、时间线分隔符（Today）、未读标签、系统通知 | Plain/Separator/Border 变体；loading 态 spinner 或文字 shimmer；可放交互子元素 |
| [Shimmer](https://gpui-kit.com/component/shimmer) | 文字流动高光加载反馈，适合「正在思考…」等短暂等待态（是活动提示而非进度） | 扫光时长/颜色/方向可配；跟随明暗主题；reduced motion 自动退化为静态 |
| [Questionnaire](https://gpui-kit.com/component/questionnaire) | 一次一题的有序问答流程：当前题、答案、校验、进度与导航 | 单选/多选/自由输入；必填校验、可跳过；键盘快捷键；QuestionnaireEvent 供宿主持久化 |
| [Clipboard](https://gpui-kit.com/component/clipboard) | 复制到剪贴板按钮，点击后图标由复制变对勾，常作 Input 后缀 | 静态 value 或惰性 value_fn；on_copied 回调；跨平台 UTF-8 纯文本 |

## 基础控件

| 组件 | 用途 | 关键特性 |
|---|---|---|
| [Button](https://gpui-kit.com/component/button) | 核心可点击按钮 | primary/danger/warning/success/ghost/link/text 变体 + outline 修饰；四档尺寸；禁用/加载/选中；图标可为 Spinner 或进度环；ButtonGroup 单选/多选 |
| [DropdownButton](https://gpui-kit.com/component/dropdown_button) | 主按钮 + 触发按钮组合（「保存」+「另存为…」） | 变体/尺寸整体设置；主按钮独立点击处理；菜单锚点可自定义 |
| [Icon](https://gpui-kit.com/component/icon) | SVG 图标渲染，内置 Lucide 图标集 | 预设/自定义尺寸；默认仅内嵌 101 个图标（可注册全部 1830 个，注意二进制体积）；自定义 SVG 字节；含 Bot/User 等助手 UI 图标 |
| [Image](https://gpui-kit.com/component/image) | 图片展示：re-export gpui 的 `img()`（彩色图）/`svg()`（单色图），补齐加载态、回退与缓存 | 来源支持资产键/文件路径/URL（经应用 HTTP client）；object_fit Contain/Cover + 宽高比控制（加载前防布局抖动）；with_loading/with_fallback 回调；解码缓存 + HTTP 下载缓存跨视图/重启复用 |
| [Avatar](https://gpui-kit.com/component/avatar) | 用户头像，无图回退首字母缩写或占位图标 | 首字母背景色按名字确定性生成（WCAG AA 对比度）；四档尺寸；AvatarGroup 重叠排布 + 数量省略 |
| [Badge](https://gpui-kit.com/component/badge) | 叠加在头像/图标上的角标：通知数、状态点、图标 | 数字（0 自动隐藏、超限 99+）/圆点/图标三模式；自动定位；支持嵌套 |
| [Tag](https://gpui-kit.com/component/tag) | 紧凑标签：分类、筛选、元数据/状态展示 | 语义变体 + 描边；两档尺寸；自定义 HSLA 色；纯展示无交互 |
| [Kbd](https://gpui-kit.com/component/kbd) | 键盘快捷键展示，按平台自动格式化（macOS ⌘⌥⇧⌃ / 其他 Ctrl+Shift+A） | 从 Keystroke 或已注册 GPUI action 构建；appearance(false) 去样式背景 |
| [Label](https://gpui-kit.com/component/label) | 通用文本标签/说明文字 | optional/required 次级文本；全文/前缀高亮匹配（搜索过滤）；敏感值打点遮码 |
| [Checkbox](https://gpui-kit.com/component/checkbox) | 二选一复选框，可带标签 | 受控状态；Sizable/Disableable；自定义 tab 顺序 |
| [Radio](https://gpui-kit.com/component/radio) | 互斥单选按钮 | 推荐用 RadioGroup（水平/垂直）；受控；自定义子内容（如描述） |
| [Switch](https://gpui-kit.com/component/switch) | 布尔开/关开关，用于设置面板与表单 | 标签可左可右；禁用、tooltip、自定义选中色 |
| [Toggle](https://gpui-kit.com/component/toggle) | 按钮式开/关或选中切换，适合工具栏、筛选器 | Ghost/描边样式；ToggleGroup 分组与 segmented 分段；受控/非受控 |
| [Slider](https://gpui-kit.com/component/slider) | 拖动选值（单值或范围） | 线性/对数刻度；步长；拖动 Change、松手 Release 分离；键盘操作 |
| [Rating](https://gpui-kit.com/component/rating) | 星级评分 | 点击已选星可减 1 分；max 自定义星数；只读/禁用 |
| [Progress](https://gpui-kit.com/component/progress) | 任务完成百分比可视化（上传/下载/多步安装） | 线性条 + 圆形两种；确定值 0–100 钳制；不确定（加载中）模式；圆形可内嵌百分比 |
| [Spinner](https://gpui-kit.com/component/spinner) | 旋转动画加载指示器 | xsmall–large 及自定义大小；图标可换；转速可调 |
| [Skeleton](https://gpui-kit.com/component/skeleton) | 加载期动画占位块，保持布局稳定 | 自由定尺寸/圆角；2 秒脉冲动画；.secondary() 半透明变体 |
| [Accordion](https://gpui-kit.com/component/accordion) | 折叠面板，组织设置项或内容分区 | 多项同时展开；带边框样式；四档尺寸；可嵌套；toggle 回调 |
| [Collapsible](https://gpui-kit.com/component/collapsible) | 单段内容展开/隐藏，open() 受控 | motion_id 高度测量动画（内容保持挂载）；不动画时直接挂载/卸载 |
| [Alert](https://gpui-kit.com/component/alert) | 重要消息横幅（校验错误、保存确认、公告） | info/success/warning/error 变体；banner 全宽模式；可关闭；支持 Markdown |
| [Empty](https://gpui-kit.com/component/empty) | 空状态（无内容/无结果/首次使用） | EmptyHeader/EmptyContent 插槽；媒体可为图标/Avatar；可内嵌按钮、链接、带状态输入 |
| [Tooltip](https://gpui-kit.com/component/tooltip) | 悬停/聚焦提示 | 纯文本、自定义元素、快捷键（action/key_binding）三模式；多数组件内置 .tooltip() |
| [Pagination](https://gpui-kit.com/component/pagination) | 分页导航 | 紧凑模式（仅前后按钮）；可见页数可配（默认 5）；on_click 返回新页码 |
| [Stepper](https://gpui-kit.com/component/stepper) | 分步引导（分步表单/流程） | 水平/垂直布局；每步图标或自定义；当前步跟踪、整体/单步禁用 |

## 表单与输入

| 组件 | 用途 | 关键特性 |
|---|---|---|
| [Input](https://gpui-kit.com/component/input) | 单行文本输入 | 闭包/正则校验、输入掩码；原子内联 token（@提及/文件引用整体选删）；密码遮码防剪贴板泄露；on_paste 可拦截粘贴转附件 |
| [Input Group](https://gpui-kit.com/component/input-group) | 把 Input/Textarea 与文本、图标、按钮、工具栏组合进带边框框架，适合消息输入区 | 4 种 addon 位置；字符计数、清除按钮；textarea 自动增高；支持 token 内联引用 |
| [Textarea](https://gpui-kit.com/component/textarea) | 多行文本输入 | 自动增高至 max_rows 后滚动；回车提交；内联 token（提及/文件引用/命令）适合聊天输入框 |
| [Editor](https://gpui-kit.com/component/editor) | 代码编辑器：语法高亮、行号、折叠（单行文本用 Input，纯多行用 Textarea） | tree-sitter 高亮；多光标与列选择；内置查找替换；装饰与跟随编辑的几何区间高亮（适合代码评审标注） |
| [Select](https://gpui-kit.com/component/select) | 下拉单选（旧名 Dropdown） | 可搜索；分组选项；自定义条目渲染与匹配（SelectItem trait）；更复杂场景换 Combobox |
| [Combobox](https://gpui-kit.com/component/combobox) | 可搜索单选/多选下拉 | .multiple(true)；分组、按索引预选、自定义触发器、底部操作区、可清空；完整键盘支持 |
| [NumberInput](https://gpui-kit.com/component/number-input) | 数值输入框 | 步进按钮 + ↑/↓ 键；min/max 钳制、千分位格式化；CJK 全角数字自动转半角 |
| [OtpInput](https://gpui-kit.com/component/otp-input) | 一次性验证码（OTP/PIN）网格输入 | 4–8 位可配；自动跳格/退格回退/填满自动完成；掩码显示 |
| [ColorPicker](https://gpui-kit.com/component/color-picker) | 颜色选择：按色系分组的预设调色板 + 带校验的十六进制输入 | HSL/十六进制解析、alpha；Featured Colors 常用色；可用图标替代色块 |
| [DatePicker](https://gpui-kit.com/component/date-picker) | 日期选择（日历弹层），单日期或区间 | matcher 限制可选日期（周几/区间/闭包）；预设区间（最近 7 天等）；可选时间编辑；.cleanable() |
| [TimeField](https://gpui-kit.com/component/time-field) | 分段式时间输入（时/分/秒/AM-PM 分别编辑） | 12/24 小时制；方向键调步、a/p 切换上下午；被 DatePicker 内部复用 |
| [Calendar](https://gpui-kit.com/component/calendar) | 独立日历：单日/区间选择、月年导航 | 多月并排；自定义年份范围；Matcher 灵活限制可选日期 |
| [Form](https://gpui-kit.com/component/form) | 类型化标签的表单布局 + footer（值/校验/提交由应用管理） | v_form/h_form；多列网格 + 响应式列数；必填星号、动态描述、条件显隐 |

## 弹层与反馈

| 组件 | 用途 | 关键特性 |
|---|---|---|
| [Dialog](https://gpui-kit.com/component/dialog) | 模态对话框（确认/警告/表单弹窗），由窗口 Root 渲染 | 声明式 + 命令式（window.open_dialog）API；遮罩可配；长内容自动滚动限高；支持嵌套 |
| [AlertDialog](https://gpui-kit.com/component/alert-dialog) | 打断式确认模态（如删除确认） | 默认不点遮罩关闭、无关闭按钮；Danger 样式；DialogAction/DialogClose 自动触发回调；ESC 支持 |
| [Popover](https://gpui-kit.com/component/popover) | 触发元素交互时弹出的浮动内容 | 9 种锚定位置 + 窗口边缘钳制、箭头；左键/右键触发；受控开关；可嵌表单 |
| [HoverCard](https://gpui-kit.com/component/hover-card) | 悬停弹出富内容卡片（资料/链接预览，无需点击） | 开关延迟可配（默认 600/300ms）；防抖防闪烁；6 种锚点；移动端改点击 |
| [Menu](https://gpui-kit.com/component/menu) | 右键上下文菜单与下拉菜单 | action/链接/分隔符/可勾选项；子菜单；自动显示 action 绑定的快捷键；完整键盘导航 |
| [Notification](https://gpui-kit.com/component/notification) | 应用内 toast 通知 | Info/Success/Warning/Error 四型；标题 + 操作按钮（如 Retry）；5 秒自动隐藏悬停暂停；9 种位置；可转发系统通知中心 |
| [Focus Trap](https://gpui-kit.com/component/focus-trap) | 把 Tab/Shift-Tab 圈定在容器内循环（WCAG 无障碍） | 多陷阱与嵌套（最内层优先）；.focus_trap(id, &handle) 施加到任意容器；Dialog/Sheet 已内置 |

## 布局与容器

| 组件 | 用途 | 关键特性 |
|---|---|---|
| [Root View](https://gpui-kit.com/component/root) | 每个窗口必需的根视图，持有内容层与浮层宿主、键盘遍历与选择复制 | 自动挂载 dialog/sheet/notification 浮层；open_dialog/open_sheet/push_notification 直接调用；应用需自定义退出快捷键 |
| [Theme](https://gpui-kit.com/component/theme) | 集中主题系统（ActiveTheme trait 统一配色） | 20+ 内置明暗主题（Ayu、Catppuccin 等）；背景支持线性渐变；JSON 配置加载；ThemeRegistry 热重载 |
| [TitleBar](https://gpui-kit.com/component/title-bar) | 替代系统标题栏，自带最小化/最大化/关闭 | 平台自适应（macOS 用原生红绿灯按钮）；child() 嵌自定义内容（菜单栏/面包屑/搜索框）；自定义关闭行为 |
| [StatusBar](https://gpui-kit.com/component/status-bar) | 窗口底部状态栏，左/中/右三区 | 三区可放任意元素（文本/幽灵按钮/进度条）；中区对齐自适应两端；专用主题 token |
| [Toolbar](https://gpui-kit.com/component/toolbar) | 透明无边框水平操作容器（面板头/标签栏/命令面板） | 三种尺寸（28/32/48px）；ToolbarGroup 分组；ARIA toolbar 模式方向键导航 |
| [Sidebar](https://gpui-kit.com/component/sidebar) | 应用侧边导航面板 | Header/Footer/Group/Menu 可组合子组件；可折叠收起、左/右放置；嵌套菜单、徽章后缀、右键菜单 |
| [Dock](https://gpui-kit.com/component/dock) | 工作区布局基础设施：可拖拽标签组、嵌套分栏、可折叠 dock | h_split/v_split/tabs 可组合可序列化（Serde 持久化恢复）；运行时增删/激活/缩放/移动面板；面板工厂按名注册兼容旧布局；DockSkin 自定义渲染 |
| [Resizable](https://gpui-kit.com/component/resizable) | 可拖拽调整尺寸的分栏布局 | 水平/垂直；size_range 最小最大约束、面板可见性切换；on_resize 回调可持久化布局；手柄动画 |
| [Scrollable](https://gpui-kit.com/component/scrollable) | 带自定义滚动条、滚动追踪与虚拟化的滚动容器 | overflow_scrollbar() trait 方法；ScrollHandle 手动控制；滚动条显示模式（滚动时/悬停/常显）；scroll_to_item |
| [Sheet](https://gpui-kit.com/component/sheet) | 屏幕边缘滑出面板（导航/表单附加空间） | 左/右/上/下四方向；边缘可拖拽缩放；标题/内容/底部构建器 API；遮罩点击关闭 |
| [GroupBox](https://gpui-kit.com/component/group-box) | 内容分组容器（表单/设置面板分组） | default/fill/outline 三变体；可选标题与 muted footer |
| [DescriptionList](https://gpui-kit.com/component/description-list) | 键值对元数据展示（规格/摘要详情） | 水平/垂直/多列网格；逐项跨列；值可为纯文本或富元素 |

## 数据展示与高级

| 组件 | 用途 | 关键特性 |
|---|---|---|
| [List](https://gpui-kit.com/component/list) | delegate 模式的虚拟化、可搜索列表 | 分区头尾、多选、无限滚动加载；拖拽排序、空状态；scroll_to_item 编程式滚动 |
| [Tree](https://gpui-kit.com/component/tree) | 层级树展示（文件树/导航菜单） | TreeState/TreeItem 状态管理；懒加载子节点；搜索过滤自动展开；多选复选框；键盘导航 |
| [VirtualList](https://gpui-kit.com/component/virtual-list) | 高性能虚拟列表，只渲染可见项，支持变高条目 | 垂直/水平；高度缓存（百万级数据恒定内存）；编程式滚动（滚底/按项定位） |
| [Table](https://gpui-kit.com/component/table) | 无状态、声明式的小型静态表格 | Table/Header/Body/Row/Cell 组合；固定宽或自适应均分；无排序/虚拟滚动（那是 DataTable） |
| [DataTable](https://gpui-kit.com/component/data-table) | 高性能表格，虚拟滚动流畅展示数千行 | 行/列/单元格三种选择模式；排序、拖拽列宽、列固定、右键菜单、无限加载/分页 |
| [Tabs](https://gpui-kit.com/component/tabs) | 标签页内容分区切换 | 五种样式（边框/下划线/胶囊/轮廓/分段）+ 四尺寸；溢出下拉；自定义徽章（未读数） |
| [Command](https://gpui-kit.com/component/command) | 命令面板（⌘K 风格），分组命令过滤列表，可内联或进对话框 | 虚拟化渲染；可关搜索成快捷操作面板；set_loading 异步远程搜索；绑定真实 GPUI Action 显示快捷键；自定义行（可变高度） |
| [Settings](https://gpui-kit.com/component/settings) | macOS/iOS 风格设置界面（页面→分组→条目） | 内置 Switch/Checkbox/Input/Dropdown/NumberInput 字段及完全自定义；按标题/描述/关键词搜索；尺寸可调 |
| [Chart](https://gpui-kit.com/component/chart) | 数据可视化图表（仪表盘/行情） | 折线/柱状/面积/饼图/雷达/K 线/桑基七种；悬停 tooltip、十字线、弹簧动画；跨帧几何缓存保性能 |
| [Plot](https://gpui-kit.com/component/plot) | 底层图表构建原语（比例尺/图形/坐标轴），组合自定义图表 | 4 种比例尺；Bar（堆叠）/Line/Area/Pie 图形；Tooltip/CrossLine 悬浮层；实现 Plot trait 自由扩展 |
| [Carousel](https://gpui-kit.com/component/carousel) | 滚动吸附式轮播，视口内展示一个或多个条目 | 横/纵向；循环导航；受控选中索引；分页指示器；键盘与手势 |
