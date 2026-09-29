use super::*;

/// ^[a-zA-Z0-9-]{3,50}$（全 ASCII，字节数即字符数）
pub(crate) fn valid_name(name: &str) -> bool {
    (3..=50).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// 剥离值两侧的成对引号（"..." 或 '...'）
pub(crate) fn strip_quotes(value: &str) -> &str {
    let bytes = value.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

/// model 字段：空 / inherit / main = 继承父会话（None）
pub(crate) fn parse_model_value(raw: &str) -> Option<String> {
    let value = raw.trim();
    if value.is_empty() || value == "inherit" || value == "main" {
        None
    } else {
        Some(value.to_string())
    }
}

/// 解析子代理 Markdown 档案：`---` 包围的 frontmatter + 正文（系统提示）。
/// frontmatter 手写逐行解析（无 serde_yaml 依赖，ZCode 同款做法）：支持
/// `key: value` 标量、`key:` + 后续缩进 `- item` 列表、行内 `[a, b]` 列表、
/// `#` 注释行、值两侧引号剥离；未知字段忽略。
pub fn parse_agent_markdown(content: &str) -> Result<AgentProfile, String> {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let lines: Vec<&str> = content.lines().collect();
    if lines.first().map(|line| line.trim()) != Some("---") {
        return Err("子代理档案格式错误：首行必须是 ---（frontmatter 起始）".to_string());
    }

    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    let mut tools: Option<Vec<String>> = None;
    let mut model: Option<String> = None;
    let mut thought_level: Option<String> = None;
    let mut max_turns: Option<usize> = None;
    let mut inject_agents_md = true;

    let mut i = 1;
    let mut closed = false;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        if trimmed == "---" {
            closed = true;
            i += 1;
            break;
        }
        // 空行与 # 注释行跳过
        if trimmed.is_empty() || trimmed.starts_with('#') {
            i += 1;
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            i += 1; // 无法识别的行：宽容跳过
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        // 值形态：行内 [a, b] 列表 / 后续缩进 `- item` 列表 / 标量
        let mut list: Option<Vec<String>> = None;
        let scalar = strip_quotes(value).to_string();
        let mut consumed = 1;
        if value.starts_with('[') && value.ends_with(']') {
            list = Some(
                value[1..value.len() - 1]
                    .split(',')
                    .map(|item| strip_quotes(item.trim()).to_string())
                    .filter(|item| !item.is_empty())
                    .collect(),
            );
        } else if value.is_empty() {
            let mut items = Vec::new();
            while i + consumed < lines.len() {
                let raw = lines[i + consumed];
                let t = raw.trim();
                if (raw.starts_with(' ') || raw.starts_with('\t')) && t.starts_with('-') {
                    let item = strip_quotes(t[1..].trim());
                    if !item.is_empty() {
                        items.push(item.to_string());
                    }
                    consumed += 1;
                } else {
                    break;
                }
            }
            if !items.is_empty() {
                list = Some(items);
            }
        }
        i += consumed;
        match key {
            "name" => name = Some(scalar),
            "description" => description = Some(scalar),
            "tools" => {
                tools = list.or_else(|| (!scalar.is_empty()).then(|| vec![scalar.clone()]));
            }
            "model" => model = parse_model_value(&scalar),
            "thoughtLevel" | "thought_level" => {
                thought_level = (!scalar.is_empty()).then_some(scalar);
            }
            "maxTurns" | "max_turns" => {
                // 0/负数/非数字都报错（负数以 usize 解析失败覆盖）
                let turns: usize = scalar
                    .parse()
                    .map_err(|_| format!("maxTurns 必须是正整数，得到 \"{scalar}\""))?;
                if turns == 0 {
                    return Err("maxTurns 必须是正整数（0 不合法）".to_string());
                }
                max_turns = Some(turns);
            }
            "injectAgentsMd" | "inject_agents_md" => {
                inject_agents_md = match scalar.as_str() {
                    "true" => true,
                    "false" => false,
                    _ => {
                        return Err(format!(
                            "injectAgentsMd 只接受 true/false，得到 \"{scalar}\""
                        ));
                    }
                };
            }
            _ => {} // 未知字段忽略
        }
    }
    if !closed {
        return Err("子代理档案格式错误：frontmatter 缺少收尾的 --- 行".to_string());
    }

    let name = name
        .filter(|n| !n.is_empty())
        .ok_or_else(|| "frontmatter 缺少必填字段 name".to_string())?;
    if !valid_name(&name) {
        return Err(format!(
            "子代理 name \"{name}\" 非法：需匹配 ^[a-zA-Z0-9-]{{3,50}}$（3-50 位字母/数字/破折号）"
        ));
    }
    let description = description
        .filter(|d| !d.is_empty())
        .ok_or_else(|| "frontmatter 缺少必填字段 description".to_string())?;
    let body = lines[i..].join("\n").trim().to_string();
    if body.is_empty() {
        return Err("子代理档案正文（系统提示）为空".to_string());
    }
    Ok(AgentProfile {
        name,
        description,
        tools,
        model,
        thought_level,
        max_turns,
        inject_agents_md,
        system_prompt: body,
        source: AgentSource::BuiltIn, // 占位：load_profiles 按来源目录改写
    })
}
