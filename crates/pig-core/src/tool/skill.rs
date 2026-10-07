use super::{Tool, ToolContext, ToolEffect};
use crate::skills;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

/// Skill tool: loads a skill's full SKILL.md body by name (frontmatter stripped, directory
/// placeholders substituted); after loading, the model continues following the skill's instructions. Read-only, approval-free in all modes (same semantics
/// as ZCode: the listing lives in the system prompt and bodies load on demand, avoiding full injection blowing up the context).
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
                "description": "Load a skill's full instructions and follow them. A skill is a packaged domain capability or workflow (a SKILL.md); the names and summaries of available skills are listed in the system prompt's skill listing.\nWhen to call: when the user's task matches a skill's purpose, call this tool as the first step (blocking requirement: do not start the task itself until the skill is loaded); a \"/<name>\" in the user's message also refers to a skill.\nConstraints: only invoke skills that appear in the listing or that the user explicitly typed as /<name> — never guess skill names from training memory; do not merely mention a skill without actually calling it; if a skill was already loaded this session, follow its instructions directly instead of loading it again.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "skill": { "type": "string", "description": "Skill name (no slash, from the available-skills listing)" },
                        "args": { "type": "string", "description": "Optional task arguments/context, passed through to the skill body" }
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
            let _ = ctx; // discovery paths are injected at construction time (cwd/data_dir), no session state needed
            let name = args["skill"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or("Missing required parameter: skill (the skill name, without a slash)")?;
            let output = skills::load_skill_output(&self.cwd, &self.data_dir, name)?;
            let args_note = args["args"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|args| format!("\n\nUser-supplied arguments: {args}"))
                .unwrap_or_default();
            Ok(ToolEffect::plain(format!("{output}{args_note}")))
        })
    }
}
