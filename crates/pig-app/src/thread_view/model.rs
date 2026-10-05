use super::*;

pub(crate) use crate::anim::{EXPAND_ANIM_DUR, ExpandAnim};

/// 导航预览卡渲染数据（消息下标, 横条 bounds, 用户预览, 助手预览, 是否真实回复文本）
pub(crate) type NavCardData = (usize, Bounds<Pixels>, String, String, bool);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    System,
}

/// 代理卡（`Event::SubagentCard` 写入 Agent/AgentSwarm 工具卡；live 直发 +
/// rollout 持久化回放重建）：元信息 + 后台运行态。无卡时（live 中 SubagentCard
/// 事件到达前的瞬时态）回落标准工具卡样式
#[derive(Clone)]
pub struct AgentCardMeta {
    pub agent_id: String,
    pub profile: String,
    pub description: String,
    /// "{provider_name} · {model}"（可带思考档后缀）
    pub model: String,
    /// 本次运行为后台：后台 Agent/AgentSwarm 的工具调用立即返回回执，done 不代表
    /// 子代理结束——运行态由子代理真实生命周期（SubagentActivity）驱动
    pub background: bool,
    /// 后台子代理已结束（SubagentActivity finished 置位；回放由 core 补发）；
    /// 前台卡不看它——前台运行态跟工具调用 done 走
    pub finished: bool,
    /// 完成次序号（SubagentActivity finished 到达顺序）：Swarm 面板按它把已结束的
    /// 子代理排在前面（先完成的在前）；回放没有该事件，恒 None → 保持发起序
    pub finished_seq: Option<u64>,
    /// 后台子代理的实时进度行（SubagentActivity item 写入，finished 时清空）；
    /// 前台卡的进度走 SubagentProgress 写在段级 live_note，不用这个字段
    pub live_note: Option<String>,
}

pub enum Segment {
    Thinking {
        text: String,
        open: bool,
        /// 用户手动展开/收过后为 true，自动折叠不再覆盖
        pinned: bool,
        /// 秒表起点（首个 delta 到达时刻）
        started: std::time::Instant,
        /// 思考结束定格的用时；回放重建的历史段没有真实时钟，保持 None（显示「持续了几秒」）
        duration: Option<std::time::Duration>,
        /// 展开正文的滚动句柄（track_scroll 持久滚动位置）
        body_scroll: ScrollHandle,
        /// 进行中 header 滚动输出行的横向滚动句柄（钉尾显示最新内容）
        ticker_scroll: ScrollHandle,
        /// 滚动输出行的纵滚状态机（换行时旧行向上滚出、新行从下方滚入）
        ticker: TickerRoll,
        /// 展开/收起动画状态（见 ExpandAnim）
        expand_anim: ExpandAnim,
    },
    Markdown {
        state: Entity<TextViewState>,
        text: String,
    },
    ToolCall {
        tool: String,
        summary: String,
        output: String,
        is_error: bool,
        done: bool,
        expanded: bool,
        /// 写/改类工具的本次编辑 diff（内联 diff 卡片）
        edit: Option<EditDiff>,
        /// 前台子代理的实时进度行（SubagentProgress 写入、ToolCallEnd 清空）；
        /// 独立字段而非覆盖 summary：运行中原摘要（「子代理 explore: …」）要保留。
        /// 回放没有该事件，恒为 None
        live_note: Option<String>,
        /// 代理卡列表（SubagentCard 事件按 item_id 追加：Agent 一张、AgentSwarm
        /// 每个子代理一张；非空时按代理卡样式渲染，点击开右侧子代理对话 tab；
        /// live 直发 + 回放经 rollout 记录重建）
        agent_cards: Vec<AgentCardMeta>,
        /// Read 工具代码卡的 UI 态（render 前惰性创建；见 read.rs）
        read_ui: Option<ReadCardUi>,
        /// Bash 工具代码卡的 UI 态（命令卡 + 输出卡；render 前惰性创建，见 bash.rs）
        bash_ui: Option<BashCardUi>,
        /// 展开/收起动画状态（见 ExpandAnim）
        expand_anim: ExpandAnim,
        /// 展开正文的滚动句柄（track_scroll 持久滚动位置）
        body_scroll: ScrollHandle,
    },
    /// 一轮结束时的本轮文件改动面板（ZCode turn 头部文件更改同款）
    TurnChanges {
        rows: Vec<TurnFileRow>,
        open: bool,
        /// 展开/收起动画状态（见 ExpandAnim）
        expand_anim: ExpandAnim,
    },
    Approval {
        request_id: String,
        decision: Option<ApprovalDecision>,
    },
}

/// Read 工具代码卡的 UI 态（render 前惰性创建；段存活期内保留）
pub struct ReadCardUi {
    /// 自动换行（默认关：横向滚动）
    pub wrap: bool,
    /// 复制按钮反馈（换勾，应用惯例不回弹）
    pub copied: bool,
    /// 不折行模式的横向滚动句柄
    pub h_scroll: ScrollHandle,
    /// 解析 + 高亮缓存（首次展开时构建；RefCell：render 走 &self 不可变借用。
    /// 主题切换经 HighlightedCode.theme 的 Arc 指针判等重算）
    pub cache: std::cell::RefCell<Option<std::rc::Rc<ReadCardContent>>>,
}

impl ReadCardUi {
    pub fn new() -> Self {
        Self {
            wrap: false,
            copied: false,
            h_scroll: ScrollHandle::new(),
            cache: std::cell::RefCell::new(None),
        }
    }
}

/// Read 输出的解析 + 高亮结果（ReadCardUi.cache 的内容）
pub struct ReadCardContent {
    /// (文件行号, 行内容)
    pub lines: Vec<(usize, String)>,
    /// 尾部标注行（[已截断…]/[文件信息…]/[警告…]）
    pub notes: Vec<String>,
    /// 供高亮的拼接文本（行内容以 \n 相连，不含行号前缀）
    pub code: String,
    pub highlighted: crate::code_view::HighlightedCode,
    /// 不折行模式的内容显式宽度（横向滚动驱动；量宽见 code_view）
    pub max_line_width: Pixels,
}

/// Bash 工具代码卡的 UI 态（render 前惰性创建；段存活期内保留）
pub struct BashCardUi {
    /// 命令卡/输出卡各自的自动换行开关（默认关：横向滚动）
    pub cmd_wrap: bool,
    pub out_wrap: bool,
    /// 复制按钮反馈（换勾，应用惯例不回弹）
    pub cmd_copied: bool,
    pub out_copied: bool,
    /// 命令卡/输出卡各自的纵向滚动句柄
    pub cmd_scroll: ScrollHandle,
    pub out_scroll: ScrollHandle,
    /// 各自的横向滚动句柄（不折行模式）
    pub cmd_h_scroll: ScrollHandle,
    pub out_h_scroll: ScrollHandle,
    /// 命令（bash 高亮）与输出（纯文本）的内容缓存（RefCell：render 只读借用；
    /// 主题切换经 PreparedCode.highlighted.theme 的 Arc 指针判等重算）
    pub cache: std::cell::RefCell<Option<std::rc::Rc<BashCardContent>>>,
}

impl BashCardUi {
    pub fn new() -> Self {
        Self {
            cmd_wrap: false,
            out_wrap: false,
            cmd_copied: false,
            out_copied: false,
            cmd_scroll: ScrollHandle::new(),
            out_scroll: ScrollHandle::new(),
            cmd_h_scroll: ScrollHandle::new(),
            out_h_scroll: ScrollHandle::new(),
            cache: std::cell::RefCell::new(None),
        }
    }
}

/// Bash 卡内容缓存：命令（bash 语法高亮）+ 输出（"text" 纯文本，仅量宽）
pub struct BashCardContent {
    pub cmd: crate::code_view::PreparedCode,
    pub out: crate::code_view::PreparedCode,
}

/// 思考滚动行的纵滚时序（ZCode QueuedSummaryContent 常量）：300ms 滚动 + 500ms 停留
pub(crate) const TICKER_ROLL_TRANSITION: std::time::Duration =
    std::time::Duration::from_millis(300);
/// 两次滚动的最小间隔（滚动 300 + 停留 500）
pub(crate) const TICKER_ROLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(800);
/// 定时器晚到超过该值时，队列里过期的中间条被跳过，只播最新一条
pub(crate) const TICKER_ROLL_DRIFT_SKIP: std::time::Duration =
    std::time::Duration::from_millis(250);
/// 纵滚位移 ≈ 0.8em（text_sm 14px）
pub(crate) const TICKER_ROLL_OFFSET_PX: f32 = 11.0;

/// 思考滚动行的纵滚状态机（ZCode QueuedSummaryContent 同款）：
/// 滚动行 = 累计思考全文最后一个非空行，行号是滚动的 key。行号不变 → 原位刷新
/// 文本；行号变 → 纵滚（旧行向上滚出、新行从下方滚入，300ms），之后至少停留
/// 500ms 才滚下一条；停留期间连发的新行排队（最多 2 条：下一条 + 可替换的
/// 最新条），定时器漂移超阈值时跳过中间条直接播最新。
#[derive(Default)]
pub(crate) struct TickerRoll {
    /// 当前显示行（行号, 压单行的文本）
    pub displayed: Option<(usize, String)>,
    /// 退场中的上一行（滚入后 300ms 内叠渲染）
    pub exiting: Option<(usize, String)>,
    /// 待滚队列：[0] = 下一条（不可覆盖），[1] = 可插队条（新的覆盖旧的）
    pub(crate) queue: Vec<(usize, String)>,
    /// 距上次滚入不足一个间隔（800ms 定时器在跑）
    pub(crate) rolling: bool,
    /// 定时器代次：promote/reset 各 +1，作废在途旧定时器
    pub(crate) generation: u64,
    /// 上次滚入的墙钟时刻（定时器漂移检测）
    pub(crate) promoted_at: Option<std::time::Instant>,
    /// 当前显示行是否经滚动入场（首行直接出现，无动画）
    pub rolled_in: bool,
}

impl TickerRoll {
    /// 喂入最新目标行；返回 true = 发生了立即滚动（调用方需起滚动间隔定时器）
    pub(crate) fn feed(&mut self, target: (usize, String)) -> bool {
        match &mut self.displayed {
            // 首行直接显示，不播入场动画（ZCode AnimatePresence initial={false}）
            None => {
                self.displayed = Some(target);
                false
            }
            // 同一行号：同行追加，原位刷新文本
            Some((ix, text)) if *ix == target.0 => {
                *text = target.1;
                false
            }
            _ => {
                if self.rolling {
                    // 停留期内入队：同 key 覆盖；否则保第一条，新条占/换第二格
                    if let Some(slot) = self.queue.iter_mut().find(|(ix, _)| *ix == target.0) {
                        *slot = target;
                    } else if self.queue.len() < 2 {
                        self.queue.push(target);
                    } else {
                        self.queue[1] = target;
                    }
                    false
                } else {
                    self.promote(target);
                    true
                }
            }
        }
    }

    /// 滚动间隔定时器到点：清退场行，按漂移裁剪队列后滚入下一条；
    /// 返回 true = 滚了新行（调用方续期定时器）
    pub(crate) fn fire(&mut self, generation: u64, now: std::time::Instant) -> bool {
        if generation != self.generation {
            return false;
        }
        self.exiting = None;
        self.rolling = false;
        // 主线程繁忙时定时器晚到：继续逐条补播过期行会让用户在卡顿恢复后看到
        // 一串过期状态，体感更卡——跳过中间条直接播最新
        let drifted = self
            .promoted_at
            .is_some_and(|t| now.duration_since(t) > TICKER_ROLL_INTERVAL + TICKER_ROLL_DRIFT_SKIP);
        if drifted && self.queue.len() > 1 {
            let last = self.queue.pop().expect("len > 1");
            self.queue.clear();
            self.queue.push(last);
        }
        if self.queue.is_empty() {
            return false;
        }
        let next = self.queue.remove(0);
        self.promote(next);
        true
    }

    /// 展开/收起切换时重置到最新行（ZCode：滚动行随展开卸载、回折叠时以最新行
    /// 重新挂载，不重播滚动）；代次 +1 作废在途定时器
    pub(crate) fn reset_to(&mut self, target: Option<(usize, String)>) {
        self.displayed = target;
        self.exiting = None;
        self.queue.clear();
        self.rolling = false;
        self.promoted_at = None;
        self.rolled_in = false;
        self.generation += 1;
    }

    fn promote(&mut self, next: (usize, String)) {
        self.exiting = self.displayed.take();
        self.displayed = Some(next);
        self.rolled_in = true;
        self.rolling = true;
        self.promoted_at = Some(std::time::Instant::now());
        self.generation += 1;
    }
}

/// 滚动行目标行：累计思考全文的最后一个非空 trimmed 行压成单行，返回（行号, 文本）
/// ——行号是纵滚的 key（ZCode resolveReasoningStreamingSummary 同款）。
/// lines() 只按 \n 切行：裸回车 \r（后无 \n）会留在行内，渲染层却按换行断行，
/// 滚动行被拆成多行——所有制表/回车类空白压成单空格
pub(crate) fn ticker_target_line(text: &str) -> Option<(usize, String)> {
    // Lines 是双端迭代器但 enumerate 后不再是，先数总行数再从尾部找
    let total = text.lines().count();
    text.lines()
        .rev()
        .enumerate()
        .find(|(_, l)| !l.trim().is_empty())
        .map(|(back, l)| {
            (
                total - 1 - back,
                l.split_whitespace().collect::<Vec<_>>().join(" "),
            )
        })
}

/// 每轮改动面板里的单文件行
pub struct TurnFileRow {
    pub(crate) edit: EditDiff,
    pub(crate) expanded: bool,
    /// 内联 diff 卡的滚动句柄
    pub(crate) scroll: ScrollHandle,
    /// 展开/收起动画状态（见 crate::anim::ExpandAnim）
    pub(crate) expand_anim: ExpandAnim,
}

/// 用户消息的图片附件：事件文本末尾的 `pig-code-composer://attachments/mN`
/// 链接解析而来（mN 的 N 与 media 目录文件名序号一致）
pub struct UserImage {
    /// 缩略图（加载/解码失败 = None → 渲染降级文本 chip）
    pub(crate) thumb: Option<std::sync::Arc<Image>>,
    /// 原图尺寸（缩略图等比缩放用）
    pub(crate) dims: (u32, u32),
}

/// 图片灯箱（点缩略图打开的大图覆盖层）的当前态
pub(crate) struct Lightbox {
    pub(crate) image: std::sync::Arc<Image>,
    /// 顶部标签（「图片 N」）
    pub(crate) label: String,
    /// 原图尺寸（等比缩放到窗口可用区域内用）
    pub(crate) dims: (u32, u32),
    /// 来源消息下标与消息内图片下标
    pub(crate) position: (usize, usize),
    /// 相对于适配窗口尺寸的缩放倍率
    pub(crate) zoom: f32,
    /// 相对于视口中心的平移量（像素）
    pub(crate) pan: (f32, f32),
    /// 拖动开始时的鼠标位置和图片偏移
    pub(crate) drag_start: Option<((f32, f32), (f32, f32))>,
    /// 当前拖动是否由图片边框内启动
    pub(crate) drag_capture: bool,
    /// 本次手势是否已经移动，避免拖动被当作点击
    pub(crate) drag_moved: bool,
}

/// 回合工作行状态（对齐 ZCode AssistantHistoryStatus）：回合结束时落在该轮
/// assistant 消息上，工作段（思考块 + 工具卡）折叠成一行「已工作 N 秒 ›」
#[derive(Clone, Copy)]
pub enum WorkState {
    /// 正常完成；duration 为 None 的是回放里无 TurnStats 的历史回合（文案「已处理」）
    Completed {
        duration: Option<std::time::Duration>,
    },
    /// 中断或错误收尾（文案「已停止」，与 footer 用词一致）
    Stopped,
}

pub struct ChatMessage {
    pub role: Role,
    pub text: String,
    /// 系统提示条种类（仅 System 角色有意义）：普通文本 / 「上下文已压缩」分隔条
    ///（压缩条渲染成带图标的分隔行，摘要全文仍留在 text 供自断言与排查）
    pub system_kind: SystemNoteKind,
    /// 用户消息的选择 handle + 刷新订阅（拖动选择时驱动实时高亮），仅 User 角色有
    pub selection: Option<(TextSelectionHandle, Subscription)>,
    pub files: Vec<String>,
    /// 用户消息的图片附件（链接已从 text 剥出；仅 User 角色非空）
    pub images: Vec<UserImage>,
    pub segments: Vec<Segment>,
    pub footer: Option<String>,
    /// 回合工作行状态（仅 Assistant 角色有意义）：None = 回合未结束或无工作段
    pub work_state: Option<WorkState>,
    /// 工作行展开态（true = 工作段内联可见；false = 折叠成一行）
    pub work_open: bool,
    /// 操作行「复制」已点击（按钮换勾 + 成功色，1.2s 后回弹——ZCode 同款）
    pub copied: bool,
    /// 回弹计时器代次：每次点击 +1，到期代次不符作作废（连点不重提前回弹）
    pub copied_gen: u64,
    /// 后台子代理通知卡的 UI 态（render 前惰性创建；仅 <task-notification> 消息为 Some）
    pub notification_ui: Option<NotificationUi>,
}

/// 系统提示条种类
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SystemNoteKind {
    Plain,
    /// 「上下文已压缩」分隔条（分隔线 + Archive 图标 + 短文案，不渲染摘要全文）
    Compacted,
}

/// 后台子代理通知卡的 UI 态（随消息存放，clear() 随消息一并释放）
pub struct NotificationUi {
    /// 「原始 payload」折叠区展开态
    pub payload_open: bool,
    /// 「复制路径」已点击（按钮换「已复制」）
    pub copied: bool,
    /// 记录文件大小缓存：None = 无 record 属性；Some(None) = 文件已消失；
    /// Some(Some(n)) = 字节数（render 前探测一次，避免每帧 stat）
    pub record_size: Option<Option<u64>>,
    /// payload 展开区的滚动句柄
    pub payload_scroll: ScrollHandle,
}

impl ChatMessage {
    pub(crate) fn user(text: String, files: Vec<String>) -> Self {
        Self {
            role: Role::User,
            text,
            system_kind: SystemNoteKind::Plain,
            selection: None,
            files,
            images: vec![],
            segments: vec![],
            footer: None,
            work_state: None,
            work_open: false,
            copied: false,
            copied_gen: 0,
            notification_ui: None,
        }
    }

    pub(crate) fn system(text: String) -> Self {
        Self::system_with_kind(text, SystemNoteKind::Plain)
    }

    pub(crate) fn system_with_kind(text: String, kind: SystemNoteKind) -> Self {
        Self {
            role: Role::System,
            text,
            system_kind: kind,
            selection: None,
            files: vec![],
            images: vec![],
            segments: vec![],
            footer: None,
            work_state: None,
            work_open: false,
            copied: false,
            copied_gen: 0,
            notification_ui: None,
        }
    }

    pub(crate) fn assistant() -> Self {
        Self {
            role: Role::Assistant,
            text: String::new(),
            system_kind: SystemNoteKind::Plain,
            selection: None,
            files: vec![],
            images: vec![],
            segments: vec![],
            footer: None,
            work_state: None,
            work_open: false,
            copied: false,
            copied_gen: 0,
            notification_ui: None,
        }
    }
}

/// 事件文本 → (正文, 图片序号列表)：剥出末尾的附件链接行
///（core 生成形态：正文 + "\n\n" + 空格分隔的 `[图片 N](pig-code-composer://attachments/mN)`；
/// 纯图消息没有正文前缀，整段就是链接行）。
/// 注意 label「图片 N」自带空格，token 边界按 `)` 切而不是空格。
/// 尾行混入任何非链接 token（含用户手打的相似文本）→ 整体不拆、原文保留。
pub(crate) fn split_image_links(text: &str) -> (String, Vec<u32>) {
    let (body, tail) = match text.rsplit_once("\n\n") {
        Some((body, tail)) => (body, tail.trim()),
        None => ("", text.trim()),
    };
    let mut indices = Vec::new();
    for chunk in tail.split_inclusive(')') {
        let Some(n) = parse_image_link(chunk.trim()) else {
            return (text.to_string(), vec![]);
        };
        indices.push(n);
    }
    if indices.is_empty() {
        return (text.to_string(), vec![]);
    }
    (body.to_string(), indices)
}

/// `[图片 N](pig-code-composer://attachments/mN)` → N（label 与 URL 序号须一致）
pub(crate) fn parse_image_link(token: &str) -> Option<u32> {
    let inner = token.strip_prefix("[图片 ")?.strip_suffix(')')?;
    let (label, url) = inner.split_once("](")?;
    let n: u32 = url
        .strip_prefix("pig-code-composer://attachments/m")?
        .parse()
        .ok()?;
    (label.parse::<u32>().ok()? == n).then_some(n)
}

/// 后台子代理完成/失败时 core 注入的合成用户消息（live 与回放同文）的解析结果。
/// 开标签带结构化属性（`<task-notification agent_id=".." status=".." …>`）；
/// 属性逐个独立解析、缺失为 None（解析健壮性），渲染侧逐字段走缺省。
pub(crate) struct TaskNotification {
    pub(crate) agent_id: Option<String>,
    pub(crate) status: Option<String>,
    pub(crate) turns: Option<String>,
    pub(crate) description: Option<String>,
    /// 子代理实际耗时（毫秒）
    pub(crate) duration_ms: Option<u64>,
    /// 子代理上下文 JSONL 记录文件的绝对路径
    pub(crate) record: Option<String>,
    /// 子代理结果全文文件的绝对路径（{agent_id}.result.md；文件行优先指它）
    pub(crate) result: Option<String>,
}

/// 整段被 `<task-notification…>…</task-notification>` 包裹时识别为通知并解析
/// 开标签属性。只用于显示层分流：消息原文（含标签）不动，payload 折叠区也渲染原文。
pub(crate) fn as_task_notification(text: &str) -> Option<TaskNotification> {
    let rest = text.trim().strip_prefix("<task-notification")?;
    // 前缀后必须紧跟 '>' 或空白（防 <task-notification-foo> 误判）
    if !rest.starts_with('>') && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let (attrs, after) = rest.split_once('>')?;
    // 闭标签校验（strip_suffix 只剥最外层：内层同名标签不影响识别）
    after.strip_suffix("</task-notification>")?;
    Some(TaskNotification {
        agent_id: notification_attr(attrs, "agent_id"),
        status: notification_attr(attrs, "status"),
        turns: notification_attr(attrs, "turns"),
        description: notification_attr(attrs, "description"),
        duration_ms: notification_attr(attrs, "duration_ms").and_then(|v| v.parse().ok()),
        record: notification_attr(attrs, "record"),
        result: notification_attr(attrs, "result"),
    })
}

/// 从开标签属性段抽 `name="value"`（简单串搜；值不含引号——core 侧已消毒）
pub(crate) fn notification_attr(attrs: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let start = attrs.find(&needle)? + needle.len();
    let value = &attrs[start..];
    let end = value.find('"')?;
    Some(value[..end].to_string())
}

/// 通知卡耗时格式化：<60s → "X.X 秒"；≥60s → "m 分 ss 秒"
pub(crate) fn format_notification_duration(ms: u64) -> String {
    if ms < 60_000 {
        format!("{:.1} 秒", ms as f64 / 1000.0)
    } else {
        format!("{} 分 {:02} 秒", ms / 60_000, (ms % 60_000) / 1000)
    }
}

/// 记录文件路径中段省略（保留首字符与末尾两段）：…/sessions/{sid}.agents/{id}.jsonl 式
pub(crate) fn elide_record_path(path: &str) -> String {
    const MAX_CHARS: usize = 48;
    if path.chars().count() <= MAX_CHARS {
        return path.to_string();
    }
    let mut tail = path.rsplit('/');
    let (Some(file), Some(parent)) = (tail.next(), tail.next()) else {
        return path.to_string();
    };
    let head = path.chars().next().unwrap_or('…');
    format!("{head}…/{parent}/{file}")
}

/// 文件大小格式化（<1KB 显示 B，否则一位小数 KB/MB）
pub(crate) fn format_file_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / 1048576.0)
    }
}
