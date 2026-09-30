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
pub const FORBIDDEN_CHILD_TOOLS: [&str; 5] = [
    "Agent",
    "AgentSwarm",
    "EnterPlanMode",
    "ExitPlanMode",
    "AskUserQuestion",
];

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

mod parse;
mod profiles;
mod records;
mod store;
mod swarm;

pub(crate) use parse::*;
pub(crate) use profiles::*;
pub(crate) use records::*;
pub use store::set_model_override;
pub(crate) use store::*;
pub(crate) use swarm::*;

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

/// 子代理 MCP 工具继承判定：收窄后的内置工具集含写工具（Write/Edit）→ 继承全部
/// 已连接 MCP 工具；否则（只读档案，如 explore）只继承 readOnlyHint 的 MCP 工具。
/// 注意 Bash 不作判别：explore 收窄后含 Bash 但属只读档案——写面判别只看
/// 纯文件变更工具（Bash 调用本身仍过审批门/规则门）。
/// 入参是 child_tool_set 收窄后的内置工具名（MCP 工具不在其中，mcp__ 前缀天然不撞名）。
pub fn child_inherits_all_mcp(child_tool_names: &[String]) -> bool {
    child_tool_names
        .iter()
        .any(|name| name == "Write" || name == "Edit")
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
            "AgentSwarm",
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
        assert!(
            !tools.contains(&"AgentSwarm".to_string()),
            "AgentSwarm 永远剔除"
        );
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
            "AgentSwarm".into(),
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
        assert!(
            !tools.contains(&"AgentSwarm".to_string()),
            "显式列出 AgentSwarm 也要剔除"
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

    // ---------- child_inherits_all_mcp ----------

    #[test]
    fn mcp_inherit_all_for_full_profile() {
        // general-purpose（tools=None → 全部内置收窄）：含 Write/Edit → 继承全部 MCP
        let general = &builtin_profiles()[0];
        let keep = child_tool_set(general, &all_tool_names(), true);
        assert!(child_inherits_all_mcp(&keep), "全工具档案继承全部 MCP 工具");
    }

    #[test]
    fn mcp_inherit_readonly_for_readonly_profile() {
        // explore 收窄后不含 Write/Edit（含 Bash 不影响判定）→ 只继承 readOnlyHint
        let explore = &builtin_profiles()[1];
        let keep = child_tool_set(explore, &all_tool_names(), true);
        assert!(keep.contains(&"Bash".to_string()), "explore 含 Bash");
        assert!(
            !child_inherits_all_mcp(&keep),
            "只读档案只继承 readOnlyHint 的 MCP 工具"
        );
        // 显式列表给了 Write 的自定义档案 → 全继承
        let mut writer = subagent(None, None);
        writer.tools = Some(vec!["Read".into(), "Write".into()]);
        let keep = child_tool_set(&writer, &all_tool_names(), true);
        assert!(child_inherits_all_mcp(&keep));
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
        let prompt = crate::prompt::subagent_system_prompt(&profile, &cwd, &tmp.0, None);
        assert!(prompt.contains("<env>"), "应含 env 块");
        assert!(
            prompt.contains("工作区 AGENTS.md"),
            "inject_agents_md=true 应注入"
        );
        assert!(prompt.contains("项目规则"));
        assert!(prompt.ends_with("</env>"), "env 块收尾（易变内容放最后）");
        assert!(prompt.contains("档案正文。"), "档案正文保留在 env 之前");
        // 关闭注入后不再有 AGENTS.md 段
        profile.inject_agents_md = false;
        let prompt = crate::prompt::subagent_system_prompt(&profile, &cwd, &tmp.0, None);
        assert!(!prompt.contains("AGENTS.md"));
        assert!(prompt.ends_with("</env>"));
    }
}
