//! 显式导入（不用 `use super::*`）：test-support 下 glob 会把 gpui 的 test 宏
//! 引进来遮蔽内置 #[test]（gpui-kit lib.rs 注明的坑，mcp/tests.rs 同款做法）
use super::{
    SkillInfo, SkillsSnapshot, create_skill_dir, delete_skill_dir, load_skills_from,
    sanitize_dir_name, write_skill_md,
};
use pig_core::skills as skill_core;
use std::path::{Path, PathBuf};

/// 最小临时目录辅助（mcp/tests.rs 同款做法，不引 tempfile 依赖）
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("pig-skills-ui-test-{tag}-{}", std::process::id()));
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
    std::fs::write(dir.join(skill_core::SKILL_FILE), markdown).unwrap();
}

/// 快照合并：项目级覆盖用户级并打标；启停状态来自 skills-state.json
#[test]
fn snapshot_merges_scopes_and_state() {
    let tmp = TempDir::new("snapshot");
    let data = tmp.0.join("data");
    let ws = tmp.0.join("ws");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&ws).unwrap();
    write_skill(
        &skill_core::user_root(&data),
        "pdf",
        "---\nname: pdf\ndescription: 用户级 PDF\n---\n正文",
    );
    write_skill(
        &skill_core::project_root(&ws),
        "pdf",
        "---\nname: pdf\ndescription: 项目级 PDF\n---\n正文",
    );
    write_skill(&skill_core::user_root(&data), "plain", "无 frontmatter 正文");
    let off_path = skill_core::user_root(&data).join("plain").join(skill_core::SKILL_FILE);
    skill_core::set_skill_enabled(&data, &off_path, false).unwrap();

    let snapshot: SkillsSnapshot = load_skills_from(&data, Some(&ws));
    assert_eq!(snapshot.skills.len(), 2);
    let pdf: &SkillInfo = snapshot.skills.iter().find(|s| s.name == "pdf").unwrap();
    assert_eq!(pdf.description, "项目级 PDF");
    assert_eq!(pdf.source, skill_core::SkillSource::Project);
    assert!(pdf.overrides_user);
    assert_eq!(pdf.dir_name, "pdf");
    // 停用状态挂到用户级 plain 上，frontmatter 缺失也有标记
    let plain: &SkillInfo = snapshot.skills.iter().find(|s| s.name == "plain").unwrap();
    assert!(!plain.enabled);
    assert!(!plain.has_frontmatter);
    assert!(snapshot.user_exists);
    assert!(snapshot.project_exists);
}

/// 用户级作用域（workspace=None）：只列用户级，项目级不参与
#[test]
fn snapshot_user_scope_only() {
    let tmp = TempDir::new("user-only");
    let data = tmp.0.join("data");
    let ws = tmp.0.join("ws");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&ws).unwrap();
    write_skill(
        &skill_core::user_root(&data),
        "a",
        "---\nname: a\ndescription: 用户\n---\n正文",
    );
    write_skill(
        &skill_core::project_root(&ws),
        "b",
        "---\nname: b\ndescription: 项目\n---\n正文",
    );
    let snapshot = load_skills_from(&data, None);
    let names: Vec<&str> = snapshot.skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["a"]);
    assert!(snapshot.project_path.is_none());
}

/// 目录名归一：小写、空白/下划线折成 -、首尾 - 裁剪、非法输入 None
#[test]
fn dir_name_sanitizing() {
    assert_eq!(
        sanitize_dir_name("Commit Helper").as_deref(),
        Some("commit-helper")
    );
    assert_eq!(sanitize_dir_name("My_Skill 2").as_deref(), Some("my-skill-2"));
    assert_eq!(sanitize_dir_name("pdf").as_deref(), Some("pdf"));
    assert_eq!(sanitize_dir_name("--pdf--").as_deref(), Some("pdf"));
    assert_eq!(sanitize_dir_name("中文技能"), None, "无 ASCII 字符应失败");
    assert_eq!(sanitize_dir_name("   "), None);
    let long = "a".repeat(65);
    assert_eq!(sanitize_dir_name(&long), None);
    let ok = "a".repeat(64);
    assert_eq!(sanitize_dir_name(&ok).as_deref(), Some(ok.as_str()));
}

/// 新建目录：正常落盘 + 已存在拒绝覆盖；编辑保存整文件重写
#[test]
fn create_and_rewrite_skill_files() {
    let tmp = TempDir::new("create");
    let root = tmp.0.join("skills");
    let dir = create_skill_dir(&root, "demo", "---\nname: demo\n---\n正文").unwrap();
    assert!(dir.join(skill_core::SKILL_FILE).is_file());
    let err = create_skill_dir(&root, "demo", "---\nname: demo\n---\n正文").unwrap_err();
    assert!(err.contains("已存在"), "应拒绝覆盖: {err}");
    write_skill_md(&dir, "---\nname: demo\ndescription: 补写\n---\n新正文").unwrap();
    let content = std::fs::read_to_string(dir.join(skill_core::SKILL_FILE)).unwrap();
    assert!(content.contains("新正文"));
}

/// 删除护栏：仅受控根的直接子目录可删；根自身/越界路径拒绝
#[test]
fn delete_guarded_to_roots() {
    let tmp = TempDir::new("delete");
    let root = tmp.0.join("skills");
    let dir = create_skill_dir(&root, "demo", "---\nname: demo\n---\n正文").unwrap();
    delete_skill_dir(&dir, &[&root]).unwrap();
    assert!(!dir.exists());
    let outside = tmp.0.join("elsewhere");
    std::fs::create_dir_all(&outside).unwrap();
    let err = delete_skill_dir(&outside, &[&root]).unwrap_err();
    assert!(err.contains("拒绝删除"), "越界应拒绝: {err}");
}
