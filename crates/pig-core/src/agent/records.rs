use super::*;

/// 子代理上下文目录：{sessions_dir}/{session_id}.agents/（每个子代理一个 {agent_id}.jsonl）
pub fn agents_dir(sessions_dir: &Path, session_id: &str) -> PathBuf {
    sessions_dir.join(format!("{session_id}.agents"))
}

/// 子代理上下文 JSONL 追加一行（meta / msg 记录）；目录不存在时创建。
/// 失败返回 Err，调用方决定冷热（当前会话层按非致命处理，与 rollout.append 同口径）。
pub fn append_agent_record(path: &Path, line: &serde_json::Value) -> Result<(), String> {
    use std::io::Write as _;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("创建子代理目录失败 {}: {e}", dir.display()))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("打开子代理上下文失败 {}: {e}", path.display()))?;
    let mut line = serde_json::to_string(line).map_err(|e| e.to_string())?;
    line.push('\n');
    file.write_all(line.as_bytes())
        .map_err(|e| format!("写入子代理上下文失败 {}: {e}", path.display()))
}

/// 子代理上下文 JSONL 的首行 meta 记录
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentMeta {
    pub agent_id: String,
    pub profile: String,
    pub description: String,
    pub model: String,
    pub provider: String,
    pub created_at: u64,
}

/// 读入子代理上下文：首行 meta + 余下 msg 行重建历史（resume 用）。
/// 坏行报错带行号；meta 行的 "type" 字段 serde 默认忽略。
pub fn read_agent(path: &Path) -> Result<(AgentMeta, Vec<crate::provider::ChatMsg>), String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("读取子代理上下文失败 {}: {e}", path.display()))?;
    let mut lines = raw.lines().enumerate();
    let Some((_, first)) = lines.next() else {
        return Err(format!("子代理上下文为空 {}: 缺少 meta 行", path.display()));
    };
    let first: serde_json::Value = serde_json::from_str(first)
        .map_err(|e| format!("子代理上下文第 1 行（meta）解析失败: {e}"))?;
    if first["type"].as_str() != Some("meta") {
        return Err("子代理上下文第 1 行不是 meta 记录".to_string());
    }
    let meta: AgentMeta = serde_json::from_value(first)
        .map_err(|e| format!("子代理上下文 meta 记录解析失败: {e}"))?;
    let mut history = Vec::new();
    for (ix, line) in lines {
        let line_no = ix + 1;
        let value: serde_json::Value = serde_json::from_str(line)
            .map_err(|e| format!("子代理上下文第 {line_no} 行解析失败: {e}"))?;
        if value["type"].as_str() != Some("msg") {
            return Err(format!("子代理上下文第 {line_no} 行不是 msg 记录"));
        }
        let msg: crate::provider::ChatMsg = serde_json::from_value(value["msg"].clone())
            .map_err(|e| format!("子代理上下文第 {line_no} 行消息解析失败: {e}"))?;
        history.push(msg);
    }
    Ok((meta, history))
}

/// 子代理上下文 → 右侧面板只读展示的投影（A3b）：
/// 返回 (title=meta.description, subtitle="{provider} · {model}", items)。
/// user → 任务行；assistant → 正文行 + 每个工具调用各一条 tool 行（摘要来自
/// tool::summarize）；tool 结果按 tool_call_id 回填对应 tool 行的 output
///（截断 2000 字符，字符边界）；system 跳过。
/// ChatMsg 没有工具错误标记，is_error 恒 false。
pub fn display_items(
    meta: &AgentMeta,
    msgs: &[crate::provider::ChatMsg],
) -> (String, String, Vec<pig_protocol::SubagentItem>) {
    let title = meta.description.clone();
    let subtitle = format!("{} · {}", meta.provider, meta.model);
    (title, subtitle, project_display_items(msgs))
}

/// 一批消息 → 展示项（A3d：全量加载与 SubagentActivity 增量投影共用）。
/// tool 结果按 tool_call_id 在**本批内**回填对应 tool 行的 output——
/// 增量场景的批 = 同一 step 的 assistant（带 tool_calls）+ 紧随的 tool 结果，
/// 匹配天然落在批内。
pub fn project_display_items(msgs: &[crate::provider::ChatMsg]) -> Vec<pig_protocol::SubagentItem> {
    let mut items: Vec<pig_protocol::SubagentItem> = Vec::new();
    // tool 行下标按 call id 索引：tool 结果消息回填 output 用
    let mut tool_row_by_call: HashMap<String, usize> = HashMap::new();
    for msg in msgs {
        match msg.role.as_str() {
            "user" => items.push(pig_protocol::SubagentItem {
                role: "user".to_string(),
                text: msg.content.clone().unwrap_or_default(),
                tool: None,
                output: None,
                is_error: false,
            }),
            "assistant" => {
                if let Some(text) = msg.content.as_ref().filter(|t| !t.is_empty()) {
                    items.push(pig_protocol::SubagentItem {
                        role: "assistant".to_string(),
                        text: text.clone(),
                        tool: None,
                        output: None,
                        is_error: false,
                    });
                }
                for wire in msg.tool_calls.iter().flatten() {
                    let call = crate::provider::ToolCall {
                        id: wire.id.clone(),
                        name: wire.function.name.clone(),
                        arguments: wire.function.arguments.clone(),
                    };
                    tool_row_by_call.insert(call.id.clone(), items.len());
                    items.push(pig_protocol::SubagentItem {
                        role: "tool".to_string(),
                        text: crate::tool::summarize(&call),
                        tool: Some(call.name.clone()),
                        output: None,
                        is_error: false,
                    });
                }
            }
            "tool" => {
                let output: String = msg
                    .content
                    .clone()
                    .unwrap_or_default()
                    .chars()
                    .take(2000)
                    .collect();
                if let Some(ix) = msg
                    .tool_call_id
                    .as_ref()
                    .and_then(|id| tool_row_by_call.get(id))
                {
                    items[*ix].output = Some(output);
                }
                // 找不到对应调用（截断/乱序的历史）则丢弃该结果
            }
            // system 提示不进展示
            _ => {}
        }
    }
    items
}
