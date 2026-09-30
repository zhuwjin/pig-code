//! 技能（Skills）：`<技能目录>/SKILL.md` 的发现、frontmatter 解析、启停状态、
//! 提示词清单与正文加载。语义对齐 ZCode：系统提示词只注入「名称 + 描述」清单
//! （带预算，超出降级为仅名称），正文由 Skill 工具按需加载；项目级同名技能
//! 覆盖用户级。目录布局与 agent 档案同款双层：`{data_dir}/skills/`（用户级）
//! + `{cwd}/.pigcode/skills/`（项目级）。

use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 技能文件名（目录内必须含它才算一个技能）
pub const SKILL_FILE: &str = "SKILL.md";
/// 提示词技能清单的字符预算：超出后降级为仅名称+路径（ZCode 同款量级）
const SECTION_BUDGET: usize = 20 * 1024;
/// 清单里单条描述（description + when_to_use 拼接后）的截断长度
const LISTING_DESC_CHARS: usize = 250;
/// Skill 工具读取正文的字节上限
pub const MAX_SKILL_BYTES: u64 = 100 * 1024;
/// 正文替换占位符：技能正文里的相对路径基准目录（兼容 CLAUDE 技能生态）
const DIR_PLACEHOLDERS: [&str; 2] = ["${SKILL_DIR}", "${CLAUDE_SKILL_DIR}"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkillSource {
    /// 用户级 `{data_dir}/skills/`
    User,
    /// 项目级 `{cwd}/.pigcode/skills/`
    Project,
}

impl SkillSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::User => "用户级",
            Self::Project => "项目级",
        }
    }
}

/// 一个技能的展示与调用信息（正文不驻留，Skill 工具按需读取）
#[derive(Clone, Debug)]
pub struct Skill {
    /// 调用名：frontmatter name（缺省回退目录名）
    pub name: String,
    pub description: String,
    /// 可选的适用场景说明（frontmatter when_to_use，拼进清单）
    pub when_to_use: Option<String>,
    /// SKILL.md 绝对路径（启停状态的 key）
    pub path: PathBuf,
    /// 技能目录（正文相对路径的基准）
    pub directory: PathBuf,
    pub source: SkillSource,
    /// SKILL.md 是否带 frontmatter（无则 name=目录名、description 为空）
    pub has_frontmatter: bool,
}

pub fn user_root(data_dir: &Path) -> PathBuf {
    data_dir.join("skills")
}

pub fn project_root(cwd: &Path) -> PathBuf {
    cwd.join(".pigcode").join("skills")
}

/// 扫描单个技能根：根自身 SKILL.md + 一层子目录的 SKILL.md（ZCode agent 端
/// 同款深度；不支持更深的分组目录）。目录不存在 = 空。条目内读/解析失败跳过。
pub fn discover_root(root: &Path, source: SkillSource) -> Vec<Skill> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if root.join(SKILL_FILE).is_file() {
        dirs.push(root.to_path_buf());
    }
    if let Ok(entries) = std::fs::read_dir(root) {
        let mut children: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        children.sort();
        dirs.extend(children);
    }
    let mut skills = Vec::new();
    for dir in dirs {
        let path = dir.join(SKILL_FILE);
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue; // 读取失败（编码/权限）跳过
        };
        let parsed = parse_skill_markdown(&content);
        let name = parsed
            .frontmatter
            .as_ref()
            .and_then(|fm| fm.name.clone())
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| {
                dir.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "skill".to_string())
            });
        if name.trim().is_empty() {
            continue;
        }
        skills.push(Skill {
            name,
            description: parsed
                .frontmatter
                .as_ref()
                .and_then(|fm| fm.description.clone())
                .unwrap_or_default(),
            when_to_use: parsed
                .frontmatter
                .as_ref()
                .and_then(|fm| fm.when_to_use.clone()),
            path,
            directory: dir,
            source,
            has_frontmatter: parsed.frontmatter.is_some(),
        });
    }
    skills
}

/// 双层发现：用户级为底、项目级同名整体覆盖（agent 档案同口径），按 name 排序。
/// 不应用启停状态（UI 要展示停用条目；调用方自行过滤）
pub fn discover(cwd: &Path, data_dir: &Path) -> Vec<Skill> {
    let user = discover_root(&user_root(data_dir), SkillSource::User);
    let project = discover_root(&project_root(cwd), SkillSource::Project);
    let mut by_name: HashMap<String, Skill> = HashMap::new();
    for skill in user.into_iter().chain(project) {
        by_name.insert(skill.name.clone(), skill);
    }
    let mut skills: Vec<Skill> = by_name.into_values().collect();
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

/// 应用启停状态后的可用清单（注入提示词 / Skill 工具查找用）
pub fn discover_enabled(cwd: &Path, data_dir: &Path) -> Vec<Skill> {
    let disabled = load_disabled_map(data_dir);
    discover(cwd, data_dir)
        .into_iter()
        .filter(|skill| !disabled.contains(&normalize_key(&skill.path)))
        .collect()
}

// ---------- SKILL.md 解析 / 序列化 ----------

/// frontmatter 的已识别字段 + 其余键值对（回写保真）
#[derive(Clone, Debug, Default)]
pub struct SkillFrontmatter {
    pub name: Option<String>,
    pub description: Option<String>,
    pub when_to_use: Option<String>,
    /// 未识别的键值对（license/metadata 等），按原顺序保留
    pub extra: Vec<(String, String)>,
}

/// 解析结果：frontmatter（无则为 None）+ 正文
#[derive(Clone, Debug)]
pub struct ParsedSkill {
    pub frontmatter: Option<SkillFrontmatter>,
    pub body: String,
}

/// 解析 SKILL.md：`---` 包围的 frontmatter + 正文。手写逐行解析（无 serde_yaml
/// 依赖，agent 档案同款做法）：`key: value` 标量、引号剥离、`>`/`|` 块标量、
/// `#` 注释与空行跳过；frontmatter 缺失或未闭合按「无 frontmatter」宽容处理。
pub fn parse_skill_markdown(content: &str) -> ParsedSkill {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let lines: Vec<&str> = content.lines().collect();
    if lines.first().map(|line| line.trim()) != Some("---") {
        return ParsedSkill {
            frontmatter: None,
            body: content.trim().to_string(),
        };
    }
    let mut fm = SkillFrontmatter::default();
    let mut i = 1;
    let mut closed = false;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        if trimmed == "---" {
            closed = true;
            i += 1;
            break;
        }
        if trimmed.is_empty() || trimmed.starts_with('#') {
            i += 1;
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            i += 1; // 无法识别的行宽容跳过
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        // 块标量：> 折叠 / | 字面（含 >- |- 变体），后续缩进行都属该值
        if let Some(style) = block_style(value) {
            let (text, consumed) = read_block_scalar(&lines, i + 1, style);
            assign(&mut fm, key, &text, value.to_string());
            i += 1 + consumed;
            continue;
        }
        assign(&mut fm, key, strip_quotes(value), String::new());
        i += 1;
    }
    if !closed {
        return ParsedSkill {
            frontmatter: None,
            body: content.trim().to_string(),
        };
    }
    ParsedSkill {
        frontmatter: Some(fm),
        body: lines[i.min(lines.len())..].join("\n").trim().to_string(),
    }
}

/// 值是块标量起始则返回其折叠风格
fn block_style(value: &str) -> Option<bool> {
    let fold = value.starts_with('>');
    if fold || value.starts_with('|') {
        // > | >- |- >2 等变体都按基本风格处理（缩进指示 digit 忽略）
        Some(fold)
    } else {
        None
    }
}

/// 从 start 起读块标量体：缩进行（含空行）直到首个非缩进非空行。
/// 返回（文本, 消耗行数）；折叠风格用空格连接，字面风格保留换行。
fn read_block_scalar(lines: &[&str], start: usize, fold: bool) -> (String, usize) {
    let mut body: Vec<String> = Vec::new();
    let mut i = start;
    while i < lines.len() {
        let raw = lines[i];
        if raw.trim().is_empty() {
            body.push(String::new());
            i += 1;
            continue;
        }
        if !raw.starts_with(' ') && !raw.starts_with('\t') {
            break;
        }
        body.push(raw.trim_start().to_string());
        i += 1;
    }
    // 尾部空行属块尾换行控制，文本里裁掉
    while body.last().is_some_and(|line| line.is_empty()) {
        body.pop();
        i -= 1;
    }
    let text = if fold {
        body.join(" ")
    } else {
        body.join("\n")
    };
    (text, i - start)
}

fn assign(fm: &mut SkillFrontmatter, key: &str, value: &str, _raw: String) {
    let value = value.trim();
    let owned = (!value.is_empty()).then(|| value.to_string());
    match key {
        "name" => fm.name = owned,
        "description" => fm.description = owned,
        "when_to_use" | "whenToUse" => fm.when_to_use = owned,
        _ => fm.extra.push((key.to_string(), value.trim().to_string())),
    }
}

/// 剥离值两侧的成对引号（"..." 或 '...'；agent 档案同款）
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

/// 渲染 SKILL.md：三个已知字段 + 保真 extra 键 + 正文。含换行/特殊起始符的
/// 值用 `|-` 块标量输出（解析端同款支持），保证 round-trip。
pub fn render_skill_markdown(
    name: &str,
    description: &str,
    when_to_use: Option<&str>,
    extra: &[(String, String)],
    body: &str,
) -> String {
    let scalar = |value: &str| {
        if value.contains('\n') || value.starts_with(['"', '\'', '>', '|', '#', '&', '*']) {
            // 块标量（字面）：解析端按缩进收集
            let indented: String = value
                .lines()
                .map(|line| format!("  {}\n", line))
                .collect::<String>();
            format!("|-\n{indented}")
        } else {
            value.to_string()
        }
    };
    let mut out = String::from("---\n");
    out.push_str(&format!("name: {}\n", scalar(name)));
    out.push_str(&format!("description: {}\n", scalar(description)));
    if let Some(when) = when_to_use.filter(|w| !w.trim().is_empty()) {
        out.push_str(&format!("when_to_use: {}\n", scalar(when)));
    }
    for (key, value) in extra {
        if matches!(
            key.as_str(),
            "name" | "description" | "when_to_use" | "whenToUse"
        ) {
            continue;
        }
        out.push_str(&format!("{key}: {}\n", scalar(value)));
    }
    out.push_str("---\n\n");
    out.push_str(body.trim());
    out.push('\n');
    out
}

// ---------- 启停状态（skills-state.json） ----------

/// `{data_dir}/skills-state.json`：只存 false 条目（默认启用），
/// key 为 SKILL.md 绝对路径（反斜杠归一为 `/`，ZCode 同款）
#[derive(Default, Deserialize)]
struct SkillsState {
    #[serde(default)]
    skills: HashMap<String, SkillEnable>,
}

#[derive(Default, Deserialize)]
struct SkillEnable {
    #[serde(default)]
    enable: bool,
}

pub fn state_path(data_dir: &Path) -> PathBuf {
    data_dir.join("skills-state.json")
}

fn normalize_key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// 停用清单：key = 归一化 SKILL.md 路径。文件缺失/非法 = 空（不致命）
pub fn load_disabled_map(data_dir: &Path) -> std::collections::HashSet<String> {
    let Ok(raw) = std::fs::read_to_string(state_path(data_dir)) else {
        return Default::default();
    };
    serde_json::from_str::<SkillsState>(&raw)
        .unwrap_or_default()
        .skills
        .into_iter()
        .filter(|(_, entry)| !entry.enable)
        .map(|(key, _)| key)
        .collect()
}

/// 启停一个技能：enabled=true 删除条目（回默认），false 写入。
/// 读-改-写整文件（agents-state 同款）
pub fn set_skill_enabled(
    data_dir: &Path,
    skill_md_path: &Path,
    enabled: bool,
) -> Result<(), String> {
    let path = state_path(data_dir);
    let mut raw: serde_json::Map<String, serde_json::Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|v| v.get("skills").and_then(|s| s.as_object()).cloned())
        .unwrap_or_default();
    let key = normalize_key(skill_md_path);
    if enabled {
        raw.remove(&key);
    } else {
        raw.insert(key, serde_json::json!({ "enable": false }));
    }
    let file = serde_json::json!({ "skills": raw });
    std::fs::create_dir_all(data_dir)
        .map_err(|e| format!("创建数据目录失败 {}: {e}", data_dir.display()))?;
    let text = serde_json::to_string_pretty(&file)
        .map_err(|e| format!("序列化 skills-state 失败: {e}"))?;
    std::fs::write(&path, text).map_err(|e| format!("写入 skills-state.json 失败: {e}"))
}

// ---------- 提示词清单与正文加载 ----------

/// 系统提示词里的技能清单：名称 + 描述（拼 when_to_use，250 字符截断）+ 文件
/// 路径；总量超预算降级为仅名称+路径。无可用技能时返回空串
pub fn skills_section(cwd: &Path, data_dir: &Path) -> String {
    let skills = discover_enabled(cwd, data_dir);
    if skills.is_empty() {
        return String::new();
    }
    let entry = |skill: &Skill| {
        let mut desc = skill.description.trim().to_string();
        if let Some(when) = skill
            .when_to_use
            .as_deref()
            .filter(|w| !w.trim().is_empty())
        {
            if !desc.is_empty() {
                desc.push_str(" - ");
            }
            desc.push_str(when.trim());
        }
        if desc.chars().count() > LISTING_DESC_CHARS {
            desc = format!(
                "{}...",
                desc.chars().take(LISTING_DESC_CHARS).collect::<String>()
            );
        }
        let path = skill.path.display().to_string();
        if desc.is_empty() {
            format!("- {} (file: {path})", skill.name)
        } else {
            format!("- {}: {desc} (file: {path})", skill.name)
        }
    };
    let full: Vec<String> = skills.iter().map(entry).collect();
    let header = "## 可用技能\n\
                  以下技能可经 Skill 工具按需加载完整说明；任务与某技能匹配时，\
                  先调用 Skill 加载它再继续，不要凭记忆编造技能名或内容：\n";
    if header.len() + full.iter().map(|l| l.len() + 1).sum::<usize>() <= SECTION_BUDGET {
        return format!("{header}{}\n", full.join("\n"));
    }
    // 降级：只保留名称 + 路径
    let names: Vec<String> = skills
        .iter()
        .map(|skill| format!("- {} (file: {})", skill.name, skill.path.display()))
        .collect();
    format!("{header}{}\n", names.join("\n"))
}

/// 按名查找（discover 已去重，直接找 name；找不到 None）
pub fn find_skill<'a>(skills: &'a [Skill], name: &str) -> Option<&'a Skill> {
    skills.iter().find(|skill| skill.name == name)
}

/// Skill 工具的正文加载：按名查找 → 读文件（体积护栏）→ 剥 frontmatter →
/// 替换目录占位符 → 包 `<skill_content>`。找不到/读失败返回 Err（错误文案
/// 直接进工具结果）
pub fn load_skill_output(cwd: &Path, data_dir: &Path, name: &str) -> Result<String, String> {
    let skills = discover_enabled(cwd, data_dir);
    let skill = find_skill(&skills, name).ok_or_else(|| {
        let available: Vec<String> = skills.iter().map(|s| s.name.clone()).collect();
        if available.is_empty() {
            format!("技能 \"{name}\" 不存在（当前没有可用技能），不要编造技能名")
        } else {
            format!(
                "技能 \"{name}\" 不存在。可用技能：{}；只能调用清单里的技能",
                available.join(", ")
            )
        }
    })?;
    let len = std::fs::metadata(&skill.path)
        .map_err(|e| format!("读取 {} 失败: {e}", skill.path.display()))?
        .len();
    if len > MAX_SKILL_BYTES {
        return Err(format!(
            "技能文件过大（{} KB，超过 {} KB 上限）",
            len / 1024,
            MAX_SKILL_BYTES / 1024
        ));
    }
    let content = std::fs::read_to_string(&skill.path)
        .map_err(|e| format!("读取 {} 失败: {e}", skill.path.display()))?;
    let parsed = parse_skill_markdown(&content);
    let dir = skill.directory.display().to_string();
    let mut body = parsed.body;
    for placeholder in DIR_PLACEHOLDERS {
        body = body.replace(placeholder, &dir);
    }
    Ok(format!(
        "<skill_content name=\"{}\">\n\n# Skill: {}\n\n{}\n\n技能目录: {}\n正文中的相对路径都相对该目录。\n</skill_content>",
        skill.name,
        skill.name,
        body.trim(),
        dir
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("pig-skills-test-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write_skill(root: &Path, dir_name: &str, markdown: &str) {
        let dir = root.join(dir_name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(SKILL_FILE), markdown).unwrap();
    }

    #[test]
    fn parse_frontmatter_scalars() {
        let parsed = parse_skill_markdown(
            "---\nname: pdf\ndescription: PDF 工具箱\nwhen_to_use: 生成 PDF 时\n---\n\n正文说明\n",
        );
        let fm = parsed.frontmatter.expect("应解析出 frontmatter");
        assert_eq!(fm.name.as_deref(), Some("pdf"));
        assert_eq!(fm.description.as_deref(), Some("PDF 工具箱"));
        assert_eq!(fm.when_to_use.as_deref(), Some("生成 PDF 时"));
        assert_eq!(parsed.body, "正文说明");
    }

    #[test]
    fn parse_quoted_and_multiline_description() {
        let parsed =
            parse_skill_markdown("---\nname: x\ndescription: \"带: 冒号的描述\"\n---\n正文");
        let fm = parsed.frontmatter.unwrap();
        assert_eq!(fm.description.as_deref(), Some("带: 冒号的描述"));

        let folded =
            parse_skill_markdown("---\nname: x\ndescription: >\n  第一行\n  第二行\n---\n正文");
        assert_eq!(
            folded.frontmatter.unwrap().description.as_deref(),
            Some("第一行 第二行")
        );

        let literal =
            parse_skill_markdown("---\nname: x\ndescription: |-\n  第一行\n  第二行\n---\n正文");
        assert_eq!(
            literal.frontmatter.unwrap().description.as_deref(),
            Some("第一行\n第二行")
        );
    }

    #[test]
    fn parse_without_frontmatter_falls_back() {
        let parsed = parse_skill_markdown("直接是正文，没有 frontmatter");
        assert!(parsed.frontmatter.is_none());
        assert_eq!(parsed.body, "直接是正文，没有 frontmatter");
    }

    #[test]
    fn render_round_trip_with_extras() {
        let extra = vec![("license".to_string(), "MIT".to_string())];
        let markdown = render_skill_markdown(
            "demo",
            "演示技能\n第二行说明",
            Some("做 demo 时"),
            &extra,
            "正文内容",
        );
        assert!(markdown.starts_with("---\n"));
        let parsed = parse_skill_markdown(&markdown);
        let fm = parsed.frontmatter.expect("round-trip 应有 frontmatter");
        assert_eq!(fm.name.as_deref(), Some("demo"));
        assert_eq!(fm.description.as_deref(), Some("演示技能\n第二行说明"));
        assert_eq!(fm.when_to_use.as_deref(), Some("做 demo 时"));
        assert_eq!(fm.extra, vec![("license".to_string(), "MIT".to_string())]);
        assert_eq!(parsed.body, "正文内容");
    }

    #[test]
    fn discover_scopes_and_name_fallback() {
        let tmp = TempDir::new("discover");
        let data = tmp.0.join("data");
        let ws = tmp.0.join("ws");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&ws).unwrap();
        write_skill(
            &user_root(&data),
            "pdf",
            "---\nname: pdf\ndescription: 用户级 PDF\n---\n用户级正文",
        );
        write_skill(&user_root(&data), "plain", "没有 frontmatter 的技能正文");
        write_skill(
            &project_root(&ws),
            "pdf",
            "---\nname: pdf\ndescription: 项目级 PDF\n---\n项目级正文",
        );
        let skills = discover(&ws, &data);
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["pdf", "plain"]);
        let pdf = find_skill(&skills, "pdf").unwrap();
        assert_eq!(pdf.description, "项目级 PDF");
        assert_eq!(pdf.source, SkillSource::Project);
        // 无 frontmatter：name 回退目录名，description 为空
        let plain = find_skill(&skills, "plain").unwrap();
        assert_eq!(plain.name, "plain");
        assert!(plain.description.is_empty());
        assert!(!plain.has_frontmatter);
    }

    #[test]
    fn disabled_state_filters_and_roundtrips() {
        let tmp = TempDir::new("state");
        let data = tmp.0.join("data");
        let ws = tmp.0.join("ws");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&ws).unwrap();
        write_skill(
            &user_root(&data),
            "a",
            "---\nname: a\ndescription: 技能 A\n---\n正文",
        );
        write_skill(
            &user_root(&data),
            "b",
            "---\nname: b\ndescription: 技能 B\n---\n正文",
        );
        let a_md = user_root(&data).join("a").join(SKILL_FILE);
        set_skill_enabled(&data, &a_md, false).unwrap();
        let enabled_list = discover_enabled(&ws, &data);
        let enabled: Vec<&str> = enabled_list.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(enabled, vec!["b"]);
        // 重新启用 = 条目删除（文件只剩 b 的停用为空 → skills 空对象）
        set_skill_enabled(&data, &a_md, true).unwrap();
        assert_eq!(discover_enabled(&ws, &data).len(), 2);
    }

    #[test]
    fn skills_section_lists_and_hides_disabled() {
        let tmp = TempDir::new("section");
        let data = tmp.0.join("data");
        let ws = tmp.0.join("ws");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&ws).unwrap();
        assert!(skills_section(&ws, &data).is_empty(), "无技能时空段");
        write_skill(
            &user_root(&data),
            "pdf",
            "---\nname: pdf\ndescription: PDF 工具箱\nwhen_to_use: 生成 PDF\n---\n正文",
        );
        let section = skills_section(&ws, &data);
        assert!(section.contains("## 可用技能"));
        assert!(section.contains("- pdf: PDF 工具箱 - 生成 PDF (file: "));
        assert!(section.contains("先调用 Skill 加载"));
    }

    #[test]
    fn load_skill_output_wraps_body_and_dir() {
        let tmp = TempDir::new("load");
        let data = tmp.0.join("data");
        let ws = tmp.0.join("ws");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&ws).unwrap();
        write_skill(
            &user_root(&data),
            "demo",
            "---\nname: demo\ndescription: 演示\n---\n说明见 ${SKILL_DIR}/README.md",
        );
        let out = load_skill_output(&ws, &data, "demo").unwrap();
        assert!(out.starts_with("<skill_content name=\"demo\">"));
        assert!(out.contains("# Skill: demo"));
        assert!(!out.contains("${SKILL_DIR}"), "占位符应替换为真实目录");
        assert!(out.contains("技能目录: "));
        assert!(out.ends_with("</skill_content>"));
        let err = load_skill_output(&ws, &data, "不存在").unwrap_err();
        assert!(err.contains("不存在"), "未知技能应报错: {err}");
        assert!(err.contains("demo"), "错误里应列出可用技能: {err}");
    }

    /// 接线检查：系统提示词用调用方传入的冻结清单段（会话开始快照）——
    /// 快照之后技能目录再变化也不影响已冻结的提示词（缓存稳定）；
    /// 传空段时不输出技能段
    #[test]
    fn system_prompt_wires_frozen_skills_section() {
        let tmp = TempDir::new("wire");
        let data = tmp.0.join("data");
        let ws = tmp.0.join("ws");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&ws).unwrap();
        let empty = crate::prompt::system_prompt(&ws, true, None, "2026-09-30", "", "");
        assert!(!empty.contains("可用技能"), "空冻结段不输出技能段");
        write_skill(
            &user_root(&data),
            "pdf",
            "---\nname: pdf\ndescription: PDF 工具箱\n---\n正文",
        );
        // 会话创建时冻结一份
        let frozen = skills_section(&ws, &data);
        // 冻结后删除技能目录：提示词仍用冻结快照（不重扫）
        std::fs::remove_dir_all(user_root(&data)).unwrap();
        let prompt = crate::prompt::system_prompt(&ws, true, None, "2026-09-30", "", &frozen);
        assert!(prompt.contains("## 可用技能"), "系统提示词应含冻结清单");
        assert!(prompt.contains("- pdf: PDF 工具箱"));
        // 重扫已无技能：验证冻结段与现扫描确实解耦
        assert!(skills_section(&ws, &data).is_empty());
    }
}
