use super::*;

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
        /// 展开正文的滚动句柄（track_scroll 持久滚动位置）
        body_scroll: ScrollHandle,
    },
    /// 一轮结束时的本轮文件改动面板（ZCode turn 头部文件更改同款）
    TurnChanges { rows: Vec<TurnFileRow>, open: bool },
    Approval {
        request_id: String,
        decision: Option<ApprovalDecision>,
    },
}

/// 每轮改动面板里的单文件行
pub struct TurnFileRow {
    pub(crate) edit: EditDiff,
    pub(crate) expanded: bool,
    /// 内联 diff 卡的滚动句柄
    pub(crate) scroll: ScrollHandle,
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

pub struct ChatMessage {
    pub role: Role,
    pub text: String,
    /// 用户消息的选择 handle + 刷新订阅（拖动选择时驱动实时高亮），仅 User 角色有
    pub selection: Option<(TextSelectionHandle, Subscription)>,
    pub files: Vec<String>,
    /// 用户消息的图片附件（链接已从 text 剥出；仅 User 角色非空）
    pub images: Vec<UserImage>,
    pub segments: Vec<Segment>,
    pub footer: Option<String>,
    /// 后台子代理通知卡的 UI 态（render 前惰性创建；仅 <task-notification> 消息为 Some）
    pub notification_ui: Option<NotificationUi>,
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
            selection: None,
            files,
            images: vec![],
            segments: vec![],
            footer: None,
            notification_ui: None,
        }
    }

    pub(crate) fn system(text: String) -> Self {
        Self {
            role: Role::System,
            text,
            selection: None,
            files: vec![],
            images: vec![],
            segments: vec![],
            footer: None,
            notification_ui: None,
        }
    }

    pub(crate) fn assistant() -> Self {
        Self {
            role: Role::Assistant,
            text: String::new(),
            selection: None,
            files: vec![],
            images: vec![],
            segments: vec![],
            footer: None,
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
