//! 项目级 allow/deny 规则：工作区根目录 `.pigcode/permissions.toml`。
//!
//! ```toml
//! allow = ["Bash(cargo *)", "Edit(src/**)"]
//! deny  = ["Bash(git push *)"]
//! ```
//!
//! 规则语法 `Tool(pattern)`：工具名大小写不敏感；pattern 用 glob 匹配该工具的
//! subject（Bash=完整命令字符串，Write/Edit=path；`\` 归一为 `/`）。
//! 判定语义在 session 工具循环：deny 最优先（所有模式含 Yolo 都硬拒），
//! allow 免审批但不豁免危险命令弹窗。

use std::path::Path;

/// 规则文件在工作区根的固定位置
pub const PERMISSIONS_FILE: &str = ".pigcode/permissions.toml";

#[derive(Debug, Clone)]
struct Rule {
    /// 工具名（小写归一，匹配时大小写不敏感）
    tool: String,
    pattern: glob::Pattern,
    /// 原文（"Bash(cargo *)"），报错/提示用
    raw: String,
}

impl Rule {
    /// 解析 "Tool(pattern)"；括号不配对/工具名为空/pattern 非法 → None
    fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        let open = raw.find('(')?;
        if !raw.ends_with(')') {
            return None;
        }
        let tool = raw[..open].trim().to_ascii_lowercase();
        if tool.is_empty() {
            return None;
        }
        let pattern = glob::Pattern::new(raw[open + 1..raw.len() - 1].trim()).ok()?;
        Some(Self {
            tool,
            pattern,
            raw: raw.to_string(),
        })
    }

    fn matches(&self, tool: &str, subject: &str) -> bool {
        self.tool == tool.to_ascii_lowercase() && self.pattern.matches(subject)
    }
}

/// 一个工作区加载出的规则集（缺文件 = 空规则）
#[derive(Debug, Default, Clone)]
pub struct PermissionRules {
    allow: Vec<Rule>,
    deny: Vec<Rule>,
    /// 语法错误被跳过的规则条数（加载时提示用）
    pub skipped: usize,
}

impl PermissionRules {
    /// 从工作区根加载：文件缺失 → 空规则；TOML 非法 → Err（调用方提示，不致命）
    pub fn load(cwd: &Path) -> Result<Self, String> {
        let path = cwd.join(PERMISSIONS_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(format!("读取失败 {}: {e}", path.display())),
        };
        Self::parse(&text)
    }

    /// 解析 TOML 文本（非法规则跳过并计数）
    pub fn parse(text: &str) -> Result<Self, String> {
        #[derive(serde::Deserialize)]
        struct File {
            #[serde(default)]
            allow: Vec<String>,
            #[serde(default)]
            deny: Vec<String>,
        }
        let parsed: File =
            toml::from_str(text).map_err(|e| format!("permissions.toml 解析失败: {e}"))?;
        let mut rules = Self::default();
        let mut load_list = |list: Vec<String>, into_deny: bool| {
            for raw in list {
                match Rule::parse(&raw) {
                    Some(rule) => {
                        if into_deny {
                            rules.deny.push(rule);
                        } else {
                            rules.allow.push(rule);
                        }
                    }
                    None => rules.skipped += 1,
                }
            }
        };
        load_list(parsed.allow, false);
        load_list(parsed.deny, true);
        Ok(rules)
    }

    /// subject 归一：`\` → `/`（Windows 路径/命令统一成 glob 友好的正斜杠）
    fn normalize_subject(subject: &str) -> String {
        subject.replace('\\', "/")
    }

    /// deny 命中 → 返回规则原文（硬拒文案用）
    pub fn deny_hit(&self, tool: &str, subject: &str) -> Option<&str> {
        let subject = Self::normalize_subject(subject);
        self.deny
            .iter()
            .find(|rule| rule.matches(tool, &subject))
            .map(|rule| rule.raw.as_str())
    }

    pub fn allow_hit(&self, tool: &str, subject: &str) -> bool {
        let subject = Self::normalize_subject(subject);
        self.allow.iter().any(|rule| rule.matches(tool, &subject))
    }

    pub fn is_empty(&self) -> bool {
        self.allow.is_empty() && self.deny.is_empty()
    }
}
