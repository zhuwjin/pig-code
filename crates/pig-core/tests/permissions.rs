//! 项目级权限规则（.pigcode/permissions.toml）：解析、匹配、语法容错。

use pig_core::permissions::PermissionRules;

#[test]
fn parse_and_match_allow_deny() {
    let rules = PermissionRules::parse(
        r#"
        allow = ["Bash(cargo *)", "Bash(ls)", "Edit(src/**)", "Write(docs/**)"]
        deny  = ["Bash(git push *)"]
        "#,
    )
    .expect("合法 toml");
    assert!(!rules.is_empty());
    assert_eq!(rules.skipped, 0);

    // Bash 匹配完整命令串
    assert!(rules.allow_hit("Bash", "cargo test"));
    assert!(rules.allow_hit("Bash", "cargo build --release"));
    assert!(!rules.allow_hit("Bash", "npm test"));
    assert!(rules.allow_hit("Bash", "ls"));
    assert!(
        !rules.allow_hit("Bash", "ls -la"),
        "Bash(ls) 不匹配带参形式"
    );
    // 工具名大小写不敏感
    assert!(rules.allow_hit("bash", "cargo test"));
    assert!(rules.allow_hit("BASH", "ls"));

    // Write/Edit 匹配 path（glob）
    assert!(rules.allow_hit("Edit", "src/a.rs"));
    assert!(rules.allow_hit("Edit", "src/deep/nested.rs"));
    assert!(!rules.allow_hit("Edit", "docs/a.md"));
    assert!(rules.allow_hit("Write", "docs/a.md"));

    // deny 命中返回规则原文
    assert_eq!(
        rules.deny_hit("Bash", "git push --force"),
        Some("Bash(git push *)")
    );
    assert!(rules.deny_hit("Bash", "git status").is_none());
    assert!(rules.deny_hit("Write", "src/a.rs").is_none());
}

#[test]
fn backslash_subject_normalized() {
    let rules = PermissionRules::parse(r#"deny = ["Edit(src/**)"]"#).unwrap();
    assert!(
        rules.deny_hit("Edit", "src\\win\\a.rs").is_some(),
        "反斜杠归一为 / 后匹配"
    );
}

#[test]
fn missing_file_is_empty_rules() {
    let dir = std::env::temp_dir().join(format!("pig-core-perms-none-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let rules = PermissionRules::load(&dir).expect("缺文件按空规则");
    assert!(rules.is_empty());
    assert!(!rules.allow_hit("Bash", "anything"));
    assert!(rules.deny_hit("Bash", "anything").is_none());
}

#[test]
fn invalid_toml_is_error() {
    let err = PermissionRules::parse("allow = [not toml").unwrap_err();
    assert!(err.contains("解析失败"), "{err}");
}

#[test]
fn malformed_rules_skipped_and_counted() {
    let rules = PermissionRules::parse(
        r#"
        allow = ["Bash(cargo *)", "not-a-rule", "(空工具名)", "Edit(src/**"]
        "#,
    )
    .expect("语法错误的规则不影响整体解析");
    assert_eq!(rules.skipped, 3, "3 条非法规则跳过计数");
    assert!(rules.allow_hit("Bash", "cargo check"), "合法规则仍生效");
}

#[test]
fn load_from_workspace_root() {
    let dir = std::env::temp_dir().join(format!("pig-core-perms-load-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".pigcode")).unwrap();
    std::fs::write(
        dir.join(".pigcode/permissions.toml"),
        "allow = [\"Bash(cargo *)\"]\n",
    )
    .unwrap();
    let rules = PermissionRules::load(&dir).unwrap();
    assert!(rules.allow_hit("Bash", "cargo test"));
    let _ = std::fs::remove_dir_all(&dir);
}
