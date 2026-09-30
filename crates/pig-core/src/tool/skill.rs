use super::{Tool, ToolContext, ToolEffect};
use crate::skills;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

/// Skill 工具：按名加载技能的完整 SKILL.md 正文（剥 frontmatter、替换目录
/// 占位符），加载后模型按技能指示继续工作。只读、全模式免审批（与 ZCode 同款
/// 语义：清单在系统提示词里，正文按需加载，避免全量注入撑爆上下文）。
pub(crate) struct SkillTool {
    pub cwd: PathBuf,
    pub data_dir: PathBuf,
}

impl SkillTool {
    pub fn new(cwd: &std::path::Path, data_dir: &std::path::Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            data_dir: data_dir.to_path_buf(),
        }
    }
}

impl Tool for SkillTool {
    fn name(&self) -> &'static str {
        "Skill"
    }

    fn read_only(&self) -> bool {
        true
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "Skill",
                "description": "加载技能的完整说明并按其指示执行。技能 = 领域能力/工作流的说明书（SKILL.md），可用技能的名称与简介列在系统提示词的「可用技能」清单里。\n何时调用：用户任务与某技能的职责匹配时，先把本工具作为第一步调用（阻塞性要求：加载完成前不要开始做任务本体）；用户消息里的 \"/<名字>\" 指的也是技能。\n重要约束：只能调用清单里出现的技能或用户显式输入的 /<名字>，不要凭训练记忆猜技能名；不要只提到技能却不实际调用；同一技能本次会话已加载过就直接遵循其指示，不要重复加载。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "skill": { "type": "string", "description": "技能名（不带斜杠，取自可用技能清单）" },
                        "args": { "type": "string", "description": "可选的任务参数/上下文，透传给技能正文使用" }
                    },
                    "required": ["skill"]
                }
            }
        })
    }

    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ToolEffect, String>> + Send + 'a>> {
        Box::pin(async move {
            let _ = ctx; // 发现路径经构造期注入（cwd/data_dir），不依赖会话态
            let name = args["skill"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or("缺少参数 skill（技能名，不带斜杠）")?;
            let output = skills::load_skill_output(&self.cwd, &self.data_dir, name)?;
            let args_note = args["args"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|args| format!("\n\n用户传入参数：{args}"))
                .unwrap_or_default();
            Ok(ToolEffect::plain(format!("{output}{args_note}")))
        })
    }
}
