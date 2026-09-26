//! 子代理档案体系（A1：档案/配置层，不接 Session）。
//! 内置两个档案 + 用户级（{data_dir}/agents/*.md）/项目级（{cwd}/.pigcode/agents/*.md）
//! Markdown 档案按名覆盖（ZCode 同款体系）：目录优先级 项目级 > 用户级 > 内置，
//! 同名后者整体替换前者。另有 {data_dir}/agents-state.json 的运行时模型覆盖。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use pig_protocol::{AppConfig, ProviderConfig};
use serde::{Deserialize, Serialize};

use crate::provider::ResolvedModel;

/// 子代理默认最大回合数（max_turns 未配置时）
pub const DEFAULT_MAX_TURNS: usize = 20;

/// 子代理强制剔除的工具（防嵌套委派/计划模式死锁/阻塞父 turn 提问）
pub const FORBIDDEN_CHILD_TOOLS: [&str; 4] =
    ["Agent", "EnterPlanMode", "ExitPlanMode", "AskUserQuestion"];

/// 子代理档案
#[derive(Clone, Debug)]
pub struct AgentProfile {
    /// ^[a-zA-Z0-9-]{3,50}$
    pub name: String,
    /// 给主模型看的调用依据
    pub description: String,
    /// None 或含 "*" = 全部工具
    pub tools: Option<Vec<String>>,
    /// None = 继承父会话；"providerId/modelId" 或裸 "modelId"（默认供应商内找）
    pub model: Option<String>,
    /// 推理档位名，仅配合显式 model 生效
    pub thought_level: Option<String>,
    /// 缺省 DEFAULT_MAX_TURNS
    pub max_turns: Option<usize>,
    /// 系统提示是否注入 AGENTS.md，缺省 true
    pub inject_agents_md: bool,
    /// 系统提示正文（markdown）
    pub system_prompt: String,
    pub source: AgentSource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentSource {
    BuiltIn,
    User,
    Project,
}

/// 固定交付尾段：所有子代理系统提示都必须带——主代理只看得到最后一条消息。
fn delivery_suffix() -> &'static str {
    "你是子代理：主代理看不到你的过程，只能看到你的最后一条消息。\
     最后一条消息就是完整交付物——自包含、结论先行、关键证据带 路径:行号；\
     不能向用户提问，信息不足时在结果里写明你的假设。"
}

/// 两个内置子代理档案（中文系统提示）
pub fn builtin_profiles() -> Vec<AgentProfile> {
    vec![
        AgentProfile {
            name: "general-purpose".into(),
            description: "通用子代理：研究复杂问题、执行多步任务；中间过程不进主上下文，只把最终结论带回。"
                .into(),
            tools: None,
            model: None,
            thought_level: None,
            max_turns: None,
            inject_agents_md: true,
            system_prompt: format!(
                "你是 general-purpose，一个通用研究与执行子代理：主代理把复杂问题研究、多步任务委派给你，\
                 你在用户的工作区里独立完成，只把最终结果带回。\n\n\
                 职责:\n\
                 - 研究复杂问题：读代码、查文档、跑命令验证假设，把结论带回来。\n\
                 - 执行多步任务：从任务描述自行规划步骤，复杂任务先用 TodoList 拆分并随时更新进度。\n\
                 - 修改代码前先读文件确认现状，改动贴合项目现有风格。\n\n\
                 工作方式:\n\
                 - 你拿到的只有任务描述，没有主会话的上下文：先自行补齐背景（读相关文件、搜关键符号）再动手。\n\
                 - 每完成一步验证一步（编译、测试、搜索核对），不要假设改动正确。\n\
                 - 不执行有破坏性的命令（删除、格式化、强制推送等）。\n\n\
                 {}",
                delivery_suffix()
            ),
            source: AgentSource::BuiltIn,
        },
        AgentProfile {
            name: "explore".into(),
            description: "只读搜索代理：宽幅 fan-out 搜代码/查问题，返回带 路径:行号 证据的结论；不会修改任何文件。"
                .into(),
            tools: Some(
                ["Read", "Glob", "Grep", "Bash", "FetchURL", "ReadMediaFile", "TodoList"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            ),
            model: None,
            thought_level: None,
            max_turns: None,
            inject_agents_md: true,
            system_prompt: format!(
                "你是 explore，一个只读搜索子代理：宽幅 fan-out 搜代码、查问题，\
                 把带证据的结论带回给主代理。\n\n\
                 铁律（只读）:\n\
                 - 不得调用 Write/Edit 等任何修改文件的工具。\n\
                 - Bash 只允许只读命令：ls、cat、head/tail、grep、find、\
                 git log/git show/git status/git diff 等。\n\
                 - 禁止任何修改文件或状态的命令：写入/移动/删除文件、\
                 git add/commit/checkout/clean、包管理安装、mkdir/touch 等。\n\
                 - 拿不准一条命令是否只读时，不要执行它。\n\n\
                 搜索策略:\n\
                 - 先宽幅撒网：用 Glob 摸目录结构，用 Grep 按多个关键词/正则并行搜索，\
                 不要一次只验证一个假设。\n\
                 - 多假设并行：命名变体、不同目录、上下游调用方同时查证。\n\
                 - 从宽到窄收敛：先框定相关文件集合，再精读关键片段，拿到 路径:行号 级证据。\n\n\
                 交付要求:\n\
                 - 结论先行，随后列出支撑证据（路径:行号 + 关键代码/配置摘要）。\n\
                 - 一句话带过搜过但排除的方向及原因，让主代理不必重搜。\n\n\
                 {}",
                delivery_suffix()
            ),
            source: AgentSource::BuiltIn,
        },
    ]
}

/// ^[a-zA-Z0-9-]{3,50}$（全 ASCII，字节数即字符数）
fn valid_name(name: &str) -> bool {
    (3..=50).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// 剥离值两侧的成对引号（"..." 或 '...'）
fn strip_quotes(value: &str) -> &str {
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
fn parse_model_value(raw: &str) -> Option<String> {
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

/// agents-state.json：运行时可写的子代理覆盖（设置页改子代理模型用）
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct AgentsState {
    #[serde(default)]
    model_overrides: HashMap<String, ModelOverride>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct ModelOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    thought_level: Option<String>,
}

fn state_path(data_dir: &Path) -> PathBuf {
    data_dir.join("agents-state.json")
}

/// 文件不存在/解析失败 = 空 state（不致命）
fn load_agents_state(data_dir: &Path) -> AgentsState {
    let Ok(raw) = std::fs::read_to_string(state_path(data_dir)) else {
        return AgentsState::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

/// 读-改-写 agents-state.json：两个键都给 None 时删除该 name 条目。
/// 写前 create_dir_all，序列化 pretty。设置页 UI 用。
pub fn set_model_override(
    data_dir: &Path,
    name: &str,
    model: Option<&str>,
    thought_level: Option<&str>,
) -> Result<(), String> {
    let mut state = load_agents_state(data_dir);
    // 空白串按 None 处理
    let model = model.map(str::trim).filter(|m| !m.is_empty());
    let thought_level = thought_level.map(str::trim).filter(|l| !l.is_empty());
    match (model, thought_level) {
        (None, None) => {
            state.model_overrides.remove(name);
        }
        (model, thought_level) => {
            state.model_overrides.insert(
                name.to_string(),
                ModelOverride {
                    model: model.map(str::to_string),
                    thought_level: thought_level.map(str::to_string),
                },
            );
        }
    }
    std::fs::create_dir_all(data_dir)
        .map_err(|e| format!("创建数据目录失败 {}: {e}", data_dir.display()))?;
    let raw = serde_json::to_string_pretty(&state)
        .map_err(|e| format!("序列化 agents-state 失败: {e}"))?;
    std::fs::write(state_path(data_dir), raw)
        .map_err(|e| format!("写入 agents-state.json 失败: {e}"))
}

/// 加载全部子代理档案：内置 → 用户级 → 项目级，按 name 后者覆盖前者；
/// 再应用 agents-state.json 的模型覆盖；最终按 name 排序输出。
pub fn load_profiles(cwd: &Path, data_dir: &Path) -> Vec<AgentProfile> {
    let mut by_name: HashMap<String, AgentProfile> = HashMap::new();
    for profile in builtin_profiles() {
        by_name.insert(profile.name.clone(), profile);
    }
    // 目录优先级：项目级 > 用户级 > 内置；同名整体替换
    for (dir, source) in [
        (data_dir.join("agents"), AgentSource::User),
        (cwd.join(".pigcode").join("agents"), AgentSource::Project),
    ] {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue; // 目录不存在 = 空
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
            .collect();
        files.sort(); // 同目录内按文件名稳定顺序，行为可预测
        for path in files {
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue; // 读取失败跳过，不致命
            };
            match parse_agent_markdown(&content) {
                Ok(mut profile) => {
                    profile.source = source;
                    by_name.insert(profile.name.clone(), profile);
                }
                Err(e) => {
                    eprintln!("[agent] 跳过无法解析的子代理档案 {}: {e}", path.display());
                }
            }
        }
    }
    // state 覆盖：仅对「frontmatter 未显式指定 model」的档案生效（内置必然生效），
    // 因此在覆盖合并完成后统一应用——用户/项目档案同名替换内置也不会丢覆盖。
    let state = load_agents_state(data_dir);
    for (name, profile) in by_name.iter_mut() {
        if profile.model.is_some() {
            continue;
        }
        if let Some(override_) = state.model_overrides.get(name) {
            if let Some(model) = override_.model.as_ref().filter(|m| !m.trim().is_empty()) {
                profile.model = Some(model.clone());
            }
            if let Some(level) = override_
                .thought_level
                .as_ref()
                .filter(|l| !l.trim().is_empty())
            {
                profile.thought_level = Some(level.clone());
            }
        }
    }
    let mut profiles: Vec<AgentProfile> = by_name.into_values().collect();
    profiles.sort_by(|a, b| a.name.cmp(&b.name));
    profiles
}

/// 归一化：小写 + 去空白/破折号/下划线（"My Agent" ≈ "my-agent" ≈ "my_agent"）
fn normalize_name(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace() && *c != '-' && *c != '_')
        .flat_map(char::to_lowercase)
        .collect()
}

/// 按名字找档案：精确匹配 → 归一匹配；多个命中报错列候选，零命中报错列全部可用名。
pub fn find_profile<'a>(
    profiles: &'a [AgentProfile],
    query: &str,
) -> Result<&'a AgentProfile, String> {
    if let Some(profile) = profiles.iter().find(|p| p.name == query) {
        return Ok(profile);
    }
    let normalized = normalize_name(query);
    let hits: Vec<&AgentProfile> = profiles
        .iter()
        .filter(|p| normalize_name(&p.name) == normalized)
        .collect();
    match hits.len() {
        0 => Err(format!(
            "未找到子代理 \"{query}\"。可用: {}",
            profiles
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
        1 => Ok(hits[0]),
        _ => Err(format!(
            "\"{query}\" 匹配到多个子代理: {}",
            hits.iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// 子代理实际可用工具集：profile.tools=None 或含 "*" → 全部 − FORBIDDEN；
/// 显式列表 → 列表 ∩ 全部 − FORBIDDEN（列表里不存在的名字忽略）；
/// input_image=false 再剔 ReadMediaFile。
pub fn child_tool_set(
    profile: &AgentProfile,
    all_tool_names: &[String],
    input_image: bool,
) -> Vec<String> {
    let base: Vec<&String> = match &profile.tools {
        Some(list) if !list.iter().any(|t| t == "*") => {
            all_tool_names.iter().filter(|n| list.contains(n)).collect()
        }
        _ => all_tool_names.iter().collect(),
    };
    base.into_iter()
        .filter(|name| !FORBIDDEN_CHILD_TOOLS.contains(&name.as_str()))
        .filter(|name| input_image || name.as_str() != "ReadMediaFile")
        .cloned()
        .collect()
}

/// 「providerId/modelId」全集（仅启用供应商），报错提示用
fn available_model_list(config: &AppConfig) -> String {
    let ids: Vec<String> = config
        .providers
        .iter()
        .filter(|p| p.enabled)
        .flat_map(|p| p.models.iter().map(|m| format!("{}/{}", p.id, m.id)))
        .collect();
    if ids.is_empty() {
        "（无已启用供应商）".to_string()
    } else {
        ids.join(", ")
    }
}

/// 严格解析子代理模型（ZCode 同款：解析失败即报错，不回落——与
/// session::resolve_model 的宽松静默回落相对，子代理配置错误必须当场暴露）。
pub fn resolve_subagent_model(
    config: &AppConfig,
    parent: &ResolvedModel,
    profile: &AgentProfile,
) -> Result<ResolvedModel, String> {
    let Some(spec) = profile.model.as_ref() else {
        // 继承父会话模型；thought_level 在继承时忽略——档位是各模型自己的表，
        // 父模型的推理参数在父会话解析时已定型，这里不替它改。
        return Ok(parent.clone());
    };
    let (provider, model_id): (&ProviderConfig, &str) = match spec.split_once('/') {
        Some((provider_id, model_id)) => {
            let provider = config
                .providers
                .iter()
                .find(|p| p.enabled && p.id == provider_id)
                .ok_or_else(|| {
                    format!(
                        "子代理 {} 指定的供应商 \"{provider_id}\" 不存在或未启用。可用模型: {}",
                        profile.name,
                        available_model_list(config)
                    )
                })?;
            (provider, model_id)
        }
        None => {
            let provider = config
                .providers
                .iter()
                .find(|p| p.enabled && p.id == config.default_provider)
                .ok_or_else(|| {
                    format!(
                        "默认供应商 \"{}\" 不存在或未启用。可用模型: {}",
                        config.default_provider,
                        available_model_list(config)
                    )
                })?;
            (provider, spec.as_str())
        }
    };
    let model = provider
        .models
        .iter()
        .find(|m| m.id == model_id)
        .ok_or_else(|| {
            format!(
                "供应商 {} 下没有模型 \"{model_id}\"（子代理 {}）。可用模型: {}",
                provider.id,
                profile.name,
                available_model_list(config)
            )
        })?;
    let reasoning_params = match profile.thought_level.as_ref() {
        Some(level) => {
            let params = model.reasoning_params.get(level).cloned().ok_or_else(|| {
                let mut levels: Vec<&str> =
                    model.reasoning_params.keys().map(String::as_str).collect();
                levels.sort_unstable();
                let available = if levels.is_empty() {
                    "（无）".to_string()
                } else {
                    levels.join(", ")
                };
                format!(
                    "模型 \"{}\" 没有推理档位 \"{level}\"，可用档位: {available}",
                    model.id
                )
            })?;
            Some(params)
        }
        None => None,
    };
    // 字段填法对齐 session::resolve_model
    Ok(ResolvedModel {
        base_url: provider.base_url.clone(),
        api_key: crate::config::expand_env(&provider.api_key),
        model: model.id.clone(),
        context_window: model.context_window,
        max_output_tokens: model.max_output_tokens,
        api_format: provider.api_format,
        reasoning_params,
        cap_web_search: model.cap_web_search,
        web_search_tool: model.web_search_tool.clone(),
        input_image: model.input_image,
        provider_name: provider.name.clone(),
    })
}

/// 给 Agent 工具描述拼接用的档案清单，每行一个。
pub fn agent_description_list(profiles: &[AgentProfile]) -> String {
    profiles
        .iter()
        .map(|p| {
            let tools = match &p.tools {
                None => "全部".to_string(),
                Some(list) if list.iter().any(|t| t == "*") => "全部".to_string(),
                Some(list) if list.is_empty() => "无".to_string(),
                Some(list) => list.join(", "),
            };
            format!("- {}: {}（工具: {}）", p.name, p.description, tools)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pig_protocol::{ApiFormat, ModelConfig};

    // ---------- 测试辅助 ----------

    /// 临时目录：进程号+纳秒保证唯一，Drop 自动清理
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let path = std::env::temp_dir().join(format!(
                "pig-agent-test-{tag}-{}-{nanos}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("创建临时目录");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 在 dir 下写一个档案文件（自动建目录）
    fn write_agent(dir: &Path, file: &str, content: &str) {
        std::fs::create_dir_all(dir).expect("建 agents 目录");
        std::fs::write(dir.join(file), content).expect("写档案文件");
    }

    /// 造一个只改 model/thought_level 的档案（其余字段本组测试不关心）
    fn subagent(model: Option<&str>, level: Option<&str>) -> AgentProfile {
        AgentProfile {
            name: "tester".into(),
            description: "d".into(),
            tools: None,
            model: model.map(str::to_string),
            thought_level: level.map(str::to_string),
            max_turns: None,
            inject_agents_md: true,
            system_prompt: "正文。".into(),
            source: AgentSource::BuiltIn,
        }
    }

    // ---------- frontmatter 解析 ----------

    #[test]
    fn parse_full_frontmatter() {
        let md = r#"---
name: code-review
description: "审查代码改动"
tools:
  - Read
  - Grep
  - "Bash"
model: deepseek/deepseek-chat
thoughtLevel: high
maxTurns: 30
injectAgentsMd: false
unknown: 忽略我
---

你是代码审查员，只输出问题清单。
"#;
        let p = parse_agent_markdown(md).expect("完整 frontmatter 应解析成功");
        assert_eq!(p.name, "code-review");
        assert_eq!(p.description, "审查代码改动", "值两侧引号应剥离");
        assert_eq!(
            p.tools,
            Some(vec![
                "Read".to_string(),
                "Grep".to_string(),
                "Bash".to_string()
            ])
        );
        assert_eq!(p.model.as_deref(), Some("deepseek/deepseek-chat"));
        assert_eq!(p.thought_level.as_deref(), Some("high"));
        assert_eq!(p.max_turns, Some(30));
        assert!(!p.inject_agents_md);
        assert_eq!(p.system_prompt, "你是代码审查员，只输出问题清单。");
    }

    #[test]
    fn parse_minimal_frontmatter() {
        // 只有必填字段：其余取缺省值
        let md = "---\nname: helper\ndescription: 简单助手\n---\n做该做的事。";
        let p = parse_agent_markdown(md).expect("最小 frontmatter");
        assert_eq!(p.tools, None, "tools 缺省 = 全部");
        assert_eq!(p.model, None, "model 缺省 = 继承");
        assert_eq!(p.thought_level, None);
        assert_eq!(p.max_turns, None);
        assert!(p.inject_agents_md, "inject_agents_md 缺省 true");
        assert_eq!(p.system_prompt, "做该做的事。");
    }

    #[test]
    fn parse_inline_list() {
        let md = "---\nname: helper\ndescription: d\ntools: [Read, Grep, \"Bash\"]\n---\n正文。";
        let p = parse_agent_markdown(md).expect("行内列表");
        assert_eq!(
            p.tools,
            Some(vec![
                "Read".to_string(),
                "Grep".to_string(),
                "Bash".to_string()
            ])
        );
    }

    #[test]
    fn parse_dash_list_with_comments() {
        // 缩进 `- item` 列表 + # 注释行 + 单引号剥离
        let md = "---\n# 这是注释\nname: helper\ndescription: d\ntools:\n  - Read  \n  - 'Glob'\n---\n正文。";
        let p = parse_agent_markdown(md).expect("破折号列表");
        assert_eq!(p.tools, Some(vec!["Read".to_string(), "Glob".to_string()]));
    }

    #[test]
    fn parse_unknown_fields_ignored() {
        let md = "---\nname: helper\ndescription: d\nfoo: bar\nzzz:\n  - a\n---\n正文。";
        let p = parse_agent_markdown(md).expect("未知字段应被忽略");
        assert_eq!(p.name, "helper");
        assert_eq!(p.tools, None, "未知字段的列表不应串到 tools");
    }

    #[test]
    fn parse_missing_name_err() {
        let md = "---\ndescription: d\n---\n正文。";
        assert!(parse_agent_markdown(md).unwrap_err().contains("name"));
    }

    #[test]
    fn parse_missing_description_err() {
        let md = "---\nname: helper\n---\n正文。";
        assert!(
            parse_agent_markdown(md)
                .unwrap_err()
                .contains("description")
        );
    }

    #[test]
    fn parse_invalid_name_err() {
        let too_long = "x".repeat(51);
        for bad in ["ab", "has space", "under_score", too_long.as_str()] {
            let md = format!("---\nname: {bad}\ndescription: d\n---\n正文。");
            assert!(parse_agent_markdown(&md).is_err(), "name {bad:?} 应非法");
        }
    }

    #[test]
    fn parse_bad_max_turns_err() {
        for bad in ["0", "-1", "abc"] {
            let md = format!("---\nname: helper\ndescription: d\nmaxTurns: {bad}\n---\n正文。");
            assert!(parse_agent_markdown(&md).is_err(), "maxTurns={bad} 应报错");
        }
    }

    #[test]
    fn parse_model_inherit_is_none() {
        for value in ["inherit", "main", ""] {
            let md = format!("---\nname: helper\ndescription: d\nmodel: {value}\n---\n正文。");
            let p = parse_agent_markdown(&md).expect("inherit/main/空 应解析为继承");
            assert_eq!(p.model, None, "model: {value:?} 应为 None");
        }
    }

    #[test]
    fn parse_no_frontmatter_err() {
        assert!(parse_agent_markdown("没有 frontmatter 的正文").is_err());
        assert!(parse_agent_markdown("").is_err());
    }

    // ---------- load_profiles ----------

    #[test]
    fn load_profiles_override_and_sort() {
        let tmp = TempDir::new("load");
        let data_dir = tmp.0.join("data");
        let cwd = tmp.0.join("proj");
        // 用户级覆盖内置 explore
        write_agent(
            &data_dir.join("agents"),
            "explore.md",
            "---\nname: explore\ndescription: 用户版 explore\n---\n用户正文。",
        );
        // 项目级再覆盖用户级（优先级：项目 > 用户 > 内置）
        write_agent(
            &cwd.join(".pigcode/agents"),
            "explore.md",
            "---\nname: explore\ndescription: 项目版 explore\n---\n项目正文。",
        );
        // 用户级新档案（名字排在最后，顺带验证排序）
        write_agent(
            &data_dir.join("agents"),
            "helper.md",
            "---\nname: z-helper\ndescription: 用户助手\n---\n正文。",
        );
        // 坏文件：解析失败应跳过，不影响其他档案
        write_agent(&data_dir.join("agents"), "bad.md", "这不是档案");
        // 非 .md 文件应被忽略
        write_agent(
            &data_dir.join("agents"),
            "note.txt",
            "---\nname: ghost\ndescription: x\n---\n正文。",
        );

        let profiles = load_profiles(&cwd, &data_dir);
        let names: Vec<&str> = profiles.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            ["explore", "general-purpose", "z-helper"],
            "按 name 排序；坏文件/非 md 不进列表"
        );
        let explore = find_profile(&profiles, "explore").expect("explore");
        assert_eq!(
            explore.description, "项目版 explore",
            "项目级应覆盖用户级与内置"
        );
        assert_eq!(explore.source, AgentSource::Project);
        assert_eq!(
            explore.system_prompt, "项目正文。",
            "同名整体替换（含正文）"
        );
        let helper = find_profile(&profiles, "z-helper").expect("z-helper");
        assert_eq!(helper.source, AgentSource::User);
        let general = find_profile(&profiles, "general-purpose").expect("内置 general-purpose");
        assert_eq!(general.source, AgentSource::BuiltIn);
    }

    #[test]
    fn load_profiles_user_overrides_builtin() {
        let tmp = TempDir::new("load-user");
        let data_dir = tmp.0.join("data");
        let cwd = tmp.0.join("proj"); // 无项目级目录
        write_agent(
            &data_dir.join("agents"),
            "explore.md",
            "---\nname: explore\ndescription: 用户版 explore\n---\n正文。",
        );
        let profiles = load_profiles(&cwd, &data_dir);
        let explore = find_profile(&profiles, "explore").expect("explore");
        assert_eq!(explore.description, "用户版 explore", "用户级应覆盖内置");
        assert_eq!(explore.source, AgentSource::User);
    }

    #[test]
    fn load_profiles_missing_dirs_ok() {
        let tmp = TempDir::new("load-empty");
        let profiles = load_profiles(&tmp.0.join("nope"), &tmp.0.join("nope-data"));
        assert_eq!(profiles.len(), 2, "目录不存在 = 只剩两个内置");
    }

    // ---------- agents-state.json 覆盖 ----------

    #[test]
    fn state_override_applies_to_builtin() {
        let tmp = TempDir::new("state");
        let data_dir = tmp.0.join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::write(
            data_dir.join("agents-state.json"),
            r#"{"model_overrides": {"general-purpose": {"model": "p1/m1", "thought_level": "low"}}}"#,
        )
        .unwrap();
        let profiles = load_profiles(&tmp.0.join("proj"), &data_dir);
        let general = find_profile(&profiles, "general-purpose").expect("general-purpose");
        assert_eq!(
            general.model.as_deref(),
            Some("p1/m1"),
            "内置无 frontmatter，state 必生效"
        );
        assert_eq!(general.thought_level.as_deref(), Some("low"));
        let explore = find_profile(&profiles, "explore").expect("explore");
        assert_eq!(explore.model, None, "未列在 state 里的内置不受影响");
    }

    #[test]
    fn state_override_skipped_when_frontmatter_has_model() {
        let tmp = TempDir::new("state-explicit");
        let data_dir = tmp.0.join("data");
        write_agent(
            &data_dir.join("agents"),
            "custom.md",
            "---\nname: custom\ndescription: d\nmodel: p2/m2\n---\n正文。",
        );
        std::fs::write(
            data_dir.join("agents-state.json"),
            r#"{"model_overrides": {"custom": {"model": "p9/m9", "thought_level": "max"}}}"#,
        )
        .unwrap();
        let profiles = load_profiles(&tmp.0.join("proj"), &data_dir);
        let custom = find_profile(&profiles, "custom").expect("custom");
        assert_eq!(
            custom.model.as_deref(),
            Some("p2/m2"),
            "frontmatter 显式 model 优先于 state"
        );
        assert_eq!(
            custom.thought_level, None,
            "整个 state 条目对显式 model 的档案不生效"
        );
    }

    #[test]
    fn set_model_override_roundtrip() {
        let tmp = TempDir::new("state-rw");
        let data_dir = tmp.0.join("data"); // 不存在：写入前应 create_dir_all
        set_model_override(&data_dir, "explore", Some("p1/m1"), Some("high")).expect("设置覆盖");
        let profiles = load_profiles(&tmp.0.join("proj"), &data_dir);
        let explore = find_profile(&profiles, "explore").expect("explore");
        assert_eq!(explore.model.as_deref(), Some("p1/m1"));
        assert_eq!(explore.thought_level.as_deref(), Some("high"));
        // 两个键都给 None = 删除该 name 条目
        set_model_override(&data_dir, "explore", None, None).expect("删除覆盖");
        let profiles = load_profiles(&tmp.0.join("proj"), &data_dir);
        let explore = find_profile(&profiles, "explore").expect("explore");
        assert_eq!(explore.model, None, "删除后恢复继承");
        assert_eq!(explore.thought_level, None);
        // 文件仍是合法 JSON（坏 state 会让 load 静默丢覆盖，必须守住）
        let raw = std::fs::read_to_string(data_dir.join("agents-state.json")).unwrap();
        serde_json::from_str::<serde_json::Value>(&raw).expect("state 文件应为合法 JSON");
    }

    // ---------- resolve_subagent_model ----------

    /// 两个供应商各带模型：p1/m1 有两档推理参数，p2/m2 无
    fn test_config() -> AppConfig {
        let mut m1 = ModelConfig::new("m1", 100_000, 8_000);
        m1.cap_web_search = true;
        m1.input_image = true;
        m1.reasoning_params = HashMap::from([
            ("low".to_string(), serde_json::json!({"effort": "low"})),
            ("high".to_string(), serde_json::json!({"effort": "high"})),
        ]);
        let m2 = ModelConfig::new("m2", 200_000, 16_000);
        AppConfig {
            providers: vec![
                ProviderConfig {
                    id: "p1".into(),
                    name: "供应商一".into(),
                    base_url: "http://p1.local".into(),
                    api_key: "key-1".into(),
                    api_format: ApiFormat::OpenAiChat,
                    enabled: true,
                    models: vec![m1],
                },
                ProviderConfig {
                    id: "p2".into(),
                    name: "供应商二".into(),
                    base_url: "http://p2.local".into(),
                    api_key: "key-2".into(),
                    api_format: ApiFormat::AnthropicMessages,
                    enabled: true,
                    models: vec![m2],
                },
            ],
            default_provider: "p1".into(),
            default_model: "m1".into(),
        }
    }

    fn parent_model() -> ResolvedModel {
        ResolvedModel {
            base_url: "http://parent.local".into(),
            api_key: "parent-key".into(),
            model: "parent-model".into(),
            context_window: 1,
            max_output_tokens: 1,
            api_format: ApiFormat::OpenAiChat,
            reasoning_params: Some(serde_json::json!({"effort": "max"})),
            cap_web_search: false,
            web_search_tool: None,
            input_image: false,
            provider_name: "父供应商".into(),
        }
    }

    #[test]
    fn resolve_inherit_clones_parent() {
        let parent = parent_model();
        // 继承时 thought_level 应被忽略
        let resolved =
            resolve_subagent_model(&test_config(), &parent, &subagent(None, Some("low")))
                .expect("继承");
        assert_eq!(resolved.model, "parent-model");
        assert_eq!(resolved.api_key, "parent-key");
        assert_eq!(
            resolved.reasoning_params, parent.reasoning_params,
            "继承时沿用父模型推理参数"
        );
    }

    #[test]
    fn resolve_explicit_provider_model_ok() {
        let resolved = resolve_subagent_model(
            &test_config(),
            &parent_model(),
            &subagent(Some("p2/m2"), None),
        )
        .expect("p2/m2 应解析成功");
        assert_eq!(resolved.model, "m2");
        assert_eq!(resolved.base_url, "http://p2.local");
        assert_eq!(resolved.api_key, "key-2");
        assert_eq!(resolved.api_format, ApiFormat::AnthropicMessages);
        assert_eq!(resolved.provider_name, "供应商二");
        assert_eq!(resolved.context_window, 200_000);
        assert_eq!(resolved.reasoning_params, None, "未给档位 = 无推理参数");
    }

    #[test]
    fn resolve_bare_model_uses_default_provider() {
        let resolved =
            resolve_subagent_model(&test_config(), &parent_model(), &subagent(Some("m1"), None))
                .expect("裸 modelId 应命中默认供应商");
        assert_eq!(resolved.base_url, "http://p1.local");
        assert!(resolved.cap_web_search, "能力标记取自 ModelConfig");
        assert!(resolved.input_image);
    }

    #[test]
    fn resolve_bad_provider_err_lists_available() {
        let err = resolve_subagent_model(
            &test_config(),
            &parent_model(),
            &subagent(Some("p9/m1"), None),
        )
        .unwrap_err();
        assert!(err.contains("p9"), "应点名坏供应商: {err}");
        assert!(
            err.contains("p1/m1") && err.contains("p2/m2"),
            "应列可用 providerId/modelId 全集: {err}"
        );
    }

    #[test]
    fn resolve_bad_model_err_lists_available() {
        let err = resolve_subagent_model(
            &test_config(),
            &parent_model(),
            &subagent(Some("p1/m9"), None),
        )
        .unwrap_err();
        assert!(err.contains("m9"), "应点名坏模型: {err}");
        assert!(err.contains("p1/m1"), "应列可用全集: {err}");
    }

    #[test]
    fn resolve_bad_thought_level_err_lists_levels() {
        let err = resolve_subagent_model(
            &test_config(),
            &parent_model(),
            &subagent(Some("p1/m1"), Some("max")),
        )
        .unwrap_err();
        assert!(err.contains("max"), "应点名坏档位: {err}");
        assert!(
            err.contains("low") && err.contains("high"),
            "应列该模型可用档位: {err}"
        );
    }

    #[test]
    fn resolve_good_thought_level_injects_params() {
        let resolved = resolve_subagent_model(
            &test_config(),
            &parent_model(),
            &subagent(Some("p1/m1"), Some("high")),
        )
        .expect("好档位");
        assert_eq!(
            resolved.reasoning_params,
            Some(serde_json::json!({"effort": "high"})),
            "档位参数应注入 reasoning_params"
        );
    }

    // ---------- find_profile ----------

    fn three_profiles() -> Vec<AgentProfile> {
        let mut profiles = builtin_profiles();
        profiles.push(subagent(None, None)); // name = "tester"
        profiles
    }

    #[test]
    fn find_exact_match() {
        let profiles = three_profiles();
        assert_eq!(find_profile(&profiles, "explore").unwrap().name, "explore");
    }

    #[test]
    fn find_normalized_match() {
        let profiles = three_profiles();
        for query in [
            "General-Purpose",
            "general purpose",
            "GENERAL_PURPOSE",
            "generalpurpose",
        ] {
            let found =
                find_profile(&profiles, query).unwrap_or_else(|e| panic!("{query} 应命中: {e}"));
            assert_eq!(found.name, "general-purpose", "归一匹配 {query}");
        }
    }

    #[test]
    fn find_ambiguous_err_lists_candidates() {
        // "my-agent" 与 "my_agent" 归一后相同 → 归一查询命中两个
        let mut a = subagent(None, None);
        a.name = "my-agent".into();
        let mut b = subagent(None, None);
        b.name = "my_agent".into();
        let err = find_profile(&[a, b], "myagent").unwrap_err();
        assert!(
            err.contains("my-agent") && err.contains("my_agent"),
            "应列候选名: {err}"
        );
    }

    #[test]
    fn find_missing_err_lists_all() {
        let profiles = three_profiles();
        let err = find_profile(&profiles, "nope").unwrap_err();
        for name in ["explore", "general-purpose", "tester"] {
            assert!(err.contains(name), "应列可用 {name}: {err}");
        }
    }

    // ---------- child_tool_set ----------

    fn all_tool_names() -> Vec<String> {
        crate::tool::all()
            .iter()
            .map(|t| t.name().to_string())
            .collect()
    }

    #[test]
    fn child_set_explore_read_only() {
        let explore = &builtin_profiles()[1];
        assert_eq!(explore.name, "explore");
        let tools = child_tool_set(explore, &all_tool_names(), true);
        for expected in [
            "Read",
            "Glob",
            "Grep",
            "Bash",
            "FetchURL",
            "ReadMediaFile",
            "TodoList",
        ] {
            assert!(
                tools.contains(&expected.to_string()),
                "explore 应有 {expected}"
            );
        }
        for banned in [
            "Write",
            "Edit",
            "Agent",
            "AskUserQuestion",
            "EnterPlanMode",
            "ExitPlanMode",
        ] {
            assert!(
                !tools.contains(&banned.to_string()),
                "explore 不应有 {banned}"
            );
        }
    }

    #[test]
    fn child_set_none_tools_all_minus_forbidden() {
        let general = &builtin_profiles()[0];
        assert_eq!(general.name, "general-purpose");
        let tools = child_tool_set(general, &all_tool_names(), true);
        assert!(tools.contains(&"Bash".to_string()), "全部工具应含 Bash");
        assert!(tools.contains(&"Write".to_string()), "全部工具应含 Write");
        assert!(!tools.contains(&"Agent".to_string()), "Agent 永远剔除");
        assert!(!tools.contains(&"AskUserQuestion".to_string()));
        assert!(!tools.contains(&"EnterPlanMode".to_string()));
        assert!(!tools.contains(&"ExitPlanMode".to_string()));
    }

    #[test]
    fn child_set_explicit_list_intersection() {
        // 列表 ∩ 全部；不存在的名字忽略；FORBIDDEN 即使显式列出也剔除
        let mut profile = subagent(None, None);
        profile.tools = Some(vec![
            "Read".into(),
            "NotExist".into(),
            "Agent".into(),
            "Bash".into(),
        ]);
        let tools = child_tool_set(&profile, &all_tool_names(), true);
        assert!(tools.contains(&"Read".to_string()));
        assert!(tools.contains(&"Bash".to_string()));
        assert!(
            !tools.contains(&"NotExist".to_string()),
            "不存在的工具名忽略"
        );
        assert!(
            !tools.contains(&"Agent".to_string()),
            "显式列出 Agent 也要剔除"
        );
        assert!(!tools.contains(&"Write".to_string()), "不在列表里的不给");
    }

    #[test]
    fn child_set_star_means_all() {
        let mut profile = subagent(None, None);
        profile.tools = Some(vec!["*".into()]);
        let tools = child_tool_set(&profile, &all_tool_names(), true);
        assert!(tools.contains(&"Write".to_string()), "含 * = 全部");
        assert!(!tools.contains(&"Agent".to_string()));
    }

    #[test]
    fn child_set_input_image_gates_media_tool() {
        let general = &builtin_profiles()[0];
        let with = child_tool_set(general, &all_tool_names(), true);
        assert!(
            with.contains(&"ReadMediaFile".to_string()),
            "支持图片时保留"
        );
        let without = child_tool_set(general, &all_tool_names(), false);
        assert!(
            !without.contains(&"ReadMediaFile".to_string()),
            "不支持图片时剔除"
        );
    }

    // ---------- 其他 ----------

    #[test]
    fn builtin_prompts_have_delivery_suffix() {
        for p in builtin_profiles() {
            assert!(
                p.system_prompt.contains("你是子代理"),
                "{} 缺固定交付尾段",
                p.name
            );
        }
    }

    #[test]
    fn description_list_format() {
        let list = agent_description_list(&builtin_profiles());
        assert!(list.contains("- general-purpose: "), "每行 - name: 开头");
        assert!(list.contains("（工具: 全部）"), "tools=None 显示全部");
        assert!(list.contains("- explore: "));
        assert!(
            list.contains("（工具: Read, Glob, Grep, Bash, FetchURL, ReadMediaFile, TodoList）"),
            "显式列表应逐名列出"
        );
    }

    #[test]
    fn subagent_prompt_composition() {
        let tmp = TempDir::new("prompt");
        let cwd = tmp.0.join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(cwd.join("AGENTS.md"), "项目规则").unwrap();
        let mut profile = subagent(None, None);
        profile.system_prompt = "档案正文。".into();
        let prompt = crate::prompt::subagent_system_prompt(&profile, &cwd, &tmp.0);
        assert!(prompt.contains("<env>"), "应含 env 块");
        assert!(
            prompt.contains("工作区 AGENTS.md"),
            "inject_agents_md=true 应注入"
        );
        assert!(prompt.contains("项目规则"));
        assert!(prompt.ends_with("档案正文。"), "档案正文收尾");
        // 关闭注入后不再有 AGENTS.md 段
        profile.inject_agents_md = false;
        let prompt = crate::prompt::subagent_system_prompt(&profile, &cwd, &tmp.0);
        assert!(!prompt.contains("AGENTS.md"));
        assert!(prompt.ends_with("档案正文。"));
    }
}
