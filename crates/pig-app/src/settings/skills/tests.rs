//! Explicit imports (no `use super::*`): under test-support a glob would pull
//! in gpui's test macro and shadow the built-in #[test] (a pitfall noted in
//! gpui-kit's lib.rs; same approach as mcp/tests.rs)
use super::{
    SkillInfo, SkillsSnapshot, create_skill_dir, delete_skill_dir, load_skills_from,
    sanitize_dir_name, write_skill_md,
};
use pig_utils::skills as skill_core;
use std::path::{Path, PathBuf};

/// Minimal temp directory helper (same approach as mcp/tests.rs, no tempfile
/// dependency)
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

/// Snapshot merging: project level overrides user level and is flagged; the
/// enable state comes from skills-state.json
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
        "---\nname: pdf\ndescription: user-level PDF\n---\nbody",
    );
    write_skill(
        &skill_core::project_root(&ws),
        "pdf",
        "---\nname: pdf\ndescription: project-level PDF\n---\nbody",
    );
    write_skill(
        &skill_core::user_root(&data),
        "plain",
        "no frontmatter body",
    );
    let off_path = skill_core::user_root(&data)
        .join("plain")
        .join(skill_core::SKILL_FILE);
    skill_core::set_skill_enabled(&data, &off_path, false).unwrap();

    let snapshot: SkillsSnapshot = load_skills_from(&data, Some(&ws));
    assert_eq!(snapshot.skills.len(), 2);
    let pdf: &SkillInfo = snapshot.skills.iter().find(|s| s.name == "pdf").unwrap();
    assert_eq!(pdf.description, "project-level PDF");
    assert_eq!(pdf.source, skill_core::SkillSource::Project);
    assert!(pdf.overrides_user);
    assert_eq!(pdf.dir_name, "pdf");
    // The disabled state attaches to the user-level plain; a missing
    // frontmatter is flagged too
    let plain: &SkillInfo = snapshot.skills.iter().find(|s| s.name == "plain").unwrap();
    assert!(!plain.enabled);
    assert!(!plain.has_frontmatter);
    assert!(snapshot.user_exists);
    assert!(snapshot.project_exists);
}

/// User-level scope (workspace=None): only user-level skills listed, project
/// level does not participate
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
        "---\nname: a\ndescription: user\n---\nbody",
    );
    write_skill(
        &skill_core::project_root(&ws),
        "b",
        "---\nname: b\ndescription: project\n---\nbody",
    );
    let snapshot = load_skills_from(&data, None);
    let names: Vec<&str> = snapshot.skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["a"]);
    assert!(snapshot.project_path.is_none());
}

/// Directory name normalization: lowercase, whitespace/underscores folded into
/// dashes, leading/trailing dashes trimmed, invalid input None
#[test]
fn dir_name_sanitizing() {
    assert_eq!(
        sanitize_dir_name("Commit Helper").as_deref(),
        Some("commit-helper")
    );
    assert_eq!(
        sanitize_dir_name("My_Skill 2").as_deref(),
        Some("my-skill-2")
    );
    assert_eq!(sanitize_dir_name("pdf").as_deref(), Some("pdf"));
    assert_eq!(sanitize_dir_name("--pdf--").as_deref(), Some("pdf"));
    assert_eq!(
        sanitize_dir_name("中文技能"),
        None,
        "input without ASCII characters should fail"
    );
    assert_eq!(sanitize_dir_name("   "), None);
    let long = "a".repeat(65);
    assert_eq!(sanitize_dir_name(&long), None);
    let ok = "a".repeat(64);
    assert_eq!(sanitize_dir_name(&ok).as_deref(), Some(ok.as_str()));
}

/// Create directory: normal persist plus refuses to overwrite when it exists;
/// edit save rewrites the whole file
#[test]
fn create_and_rewrite_skill_files() {
    let tmp = TempDir::new("create");
    let root = tmp.0.join("skills");
    let dir = create_skill_dir(&root, "demo", "---\nname: demo\n---\nbody").unwrap();
    assert!(dir.join(skill_core::SKILL_FILE).is_file());
    let err = create_skill_dir(&root, "demo", "---\nname: demo\n---\nbody").unwrap_err();
    assert!(err.contains("已存在"), "should refuse to overwrite: {err}");
    write_skill_md(&dir, "---\nname: demo\ndescription: amended\n---\nnew body").unwrap();
    let content = std::fs::read_to_string(dir.join(skill_core::SKILL_FILE)).unwrap();
    assert!(content.contains("new body"));
}

/// Delete guard: only direct children of the controlled roots may be deleted;
/// the root itself/out-of-bounds paths are refused
#[test]
fn delete_guarded_to_roots() {
    let tmp = TempDir::new("delete");
    let root = tmp.0.join("skills");
    let dir = create_skill_dir(&root, "demo", "---\nname: demo\n---\nbody").unwrap();
    delete_skill_dir(&dir, &[&root]).unwrap();
    assert!(!dir.exists());
    let outside = tmp.0.join("elsewhere");
    std::fs::create_dir_all(&outside).unwrap();
    let err = delete_skill_dir(&outside, &[&root]).unwrap_err();
    assert!(
        err.contains("拒绝删除"),
        "out-of-bounds path should be refused: {err}"
    );
}
