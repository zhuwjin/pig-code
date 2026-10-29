//! Read-deny materialization for the Windows sandbox.
//!
//! The sandbox's kernel policy expresses denials as **concrete paths**, so a
//! pattern rule (`.env`, `*.key`, `secrets/**`) has to be resolved against the
//! filesystem before launch. Codex solves this the same way: literal denials go
//! straight into the native list, and glob denials are expanded once per launch
//! (its `deny_read_resolver` runs bundled ripgrep over the glob's literal
//! prefix). This module does the same with the `ignore` walker that the rest of
//! the codebase already uses, so traversal, hidden-file handling and pruning
//! behave like the search tools.
//!
//! The rule table, rule normalization and glob compilation live in the shared
//! `deny_rules` module, because the macOS backend enforces the same rules as
//! access-time regexes. This module keeps the Windows half: compiling a rule
//! set into matchers and materializing it by walking.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use ignore::WalkState;

pub use super::super::deny_rules::DenyRuleKind;
pub(crate) use super::super::deny_rules::DenyRules;
pub use super::super::deny_rules::classify_deny_rule;
pub(crate) use super::super::deny_rules::from_groups;
pub(crate) use super::super::deny_rules::normalize_candidate_into;
pub use super::super::deny_rules::validate_deny_rule;
use super::policy::MAX_DENIED_PATHS;
use super::policy::MAX_SCAN_ENTRIES;

/// Why a launch could not build its deny list (test-only: the product path
/// compiles rules in the parent, where failures are `String`s).
#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct DenyError(pub(crate) String);

#[cfg(test)]
impl std::fmt::Display for DenyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// One scan outcome.
pub(crate) struct ScanOutcome {
    /// Denied files and directories. A matched directory already covers its
    /// subtree, so the walker stops descending into it.
    pub(crate) paths: Vec<PathBuf>,
    /// A bound was hit. The caller decides whether that fails the launch.
    pub(crate) truncated: bool,
}

/// Compile the configured rules (structured groups + `deny_read` strings).
///
/// The compilation itself is shared with the macOS backend; this wrapper keeps
/// the `(rules, literal denials)` shape the Windows plan builder consumes.
#[cfg(test)]
use super::super::deny_rules::CompiledRules;
#[cfg(test)]
use super::super::deny_rules::DenyRuleGroup;
#[cfg(test)]
use super::super::deny_rules::compile;

#[cfg(test)]
pub(crate) fn build(
    configured: &[DenyRuleGroup],
    user_patterns: &[String],
    workspace: &Path,
) -> Result<(DenyRules, Vec<PathBuf>), DenyError> {
    compile(configured, user_patterns, workspace)
        .map(|compiled: CompiledRules| (compiled.rules, compiled.literal_denials))
        .map_err(DenyError)
}

/// Walk `root` and collect everything the rules deny.
///
/// The walker is the `ignore` engine with standard filters **off**: sensitive
/// files are frequently hidden and frequently gitignored (`.env`, `.aws/`), so
/// honouring `.gitignore` here would silently create holes. It runs in
/// parallel (Codex's rg fast path is a parallel `ignore` walk with a glob
/// filter; this keeps the same traversal engine in-process) and each worker
/// reuses one normalized-path buffer, so the hot loop does not allocate per
/// entry.
///
/// There is **no depth cap and no name-based pruning**: either one would leave
/// a matching file un-denied without failing the launch (Codex walks the same
/// trees). The entry and denied-path bounds below are the safety valve; hitting
/// either one fails the launch rather than running with an incomplete list.
///
/// A denied **directory** is recorded and its subtree skipped: one deny covers
/// it, and descending would multiply the deny list for no benefit.
pub(crate) fn scan(root: &Path, rules: &DenyRules) -> Result<ScanOutcome, String> {
    // A scan root that does not exist is a policy error, not an empty branch:
    // the policy named it as a writable root, and silently running without
    // scanning it is exactly the incomplete deny list the caller refuses.
    if !root.exists() {
        return Err(format!(
            "cannot scan {} for sensitive files: the directory does not exist",
            root.display()
        ));
    }
    let rules = Arc::new(rules.clone());
    let found: Arc<Mutex<Vec<PathBuf>>> = Arc::new(Mutex::new(Vec::new()));
    let visited = Arc::new(AtomicUsize::new(0));
    let denied = Arc::new(AtomicUsize::new(0));
    let truncated = Arc::new(AtomicBool::new(false));
    let fatal: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let root_display = Arc::new(root.display().to_string());
    // Per-command thread pool: enough to use the machine, small enough that
    // spawning it for a tiny workspace stays cheap.
    let threads = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1)
        .min(8);

    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .standard_filters(false)
        .follow_links(false)
        .threads(threads);
    builder.build_parallel().run({
        let rules = Arc::clone(&rules);
        let found = Arc::clone(&found);
        let visited = Arc::clone(&visited);
        let denied = Arc::clone(&denied);
        let truncated = Arc::clone(&truncated);
        let fatal = Arc::clone(&fatal);
        let root_display = Arc::clone(&root_display);
        move || {
            let rules = Arc::clone(&rules);
            let found = Arc::clone(&found);
            let visited = Arc::clone(&visited);
            let denied = Arc::clone(&denied);
            let truncated = Arc::clone(&truncated);
            let fatal = Arc::clone(&fatal);
            let root_display = Arc::clone(&root_display);
            let mut candidate = String::new();
            Box::new(move |result| {
                let entry = match result {
                    Ok(entry) => entry,
                    Err(error) => {
                        // A branch that disappeared or that this process cannot
                        // read cannot hide anything from the workload: a deleted
                        // file is gone and an unreadable directory is equally
                        // unreadable to the sandboxed command. Codex's resolver
                        // skips exactly these two kinds; every other I/O error
                        // aborts the scan, because an incomplete deny list must
                        // not silently run.
                        if error.io_error().is_some_and(|io| {
                            matches!(
                                io.kind(),
                                std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
                            )
                        }) {
                            return WalkState::Continue;
                        }
                        if let Ok(mut slot) = fatal.lock()
                            && slot.is_none()
                        {
                            *slot = Some(format!(
                                "cannot scan {root_display} for sensitive files: {error}"
                            ));
                        }
                        return WalkState::Quit;
                    }
                };
                if visited.fetch_add(1, Ordering::Relaxed) + 1 > MAX_SCAN_ENTRIES {
                    truncated.store(true, Ordering::Relaxed);
                    return WalkState::Quit;
                }
                let is_root = entry.depth() == 0;
                let file_type = entry.file_type();
                let is_dir = file_type.is_some_and(|kind| kind.is_dir());
                normalize_candidate_into(entry.path(), &mut candidate);
                if is_dir {
                    // Never prune the root itself: a user glob can legitimately
                    // name the directory the scan is rooted in (`target/**`
                    // with `target` as the extra scan root).
                    if !is_root && rules.denies_normalized(&candidate) {
                        if denied.fetch_add(1, Ordering::Relaxed) >= MAX_DENIED_PATHS {
                            truncated.store(true, Ordering::Relaxed);
                            return WalkState::Quit;
                        }
                        match found.lock() {
                            Ok(mut paths) => paths.push(entry.path().to_path_buf()),
                            Err(_) => return WalkState::Quit,
                        }
                        return WalkState::Skip;
                    }
                    return WalkState::Continue;
                }
                // Symlinks are matched on their **lexical** path but never
                // followed: the walker stays inside the granted tree, and a
                // link named `.env` is denied like a real file. (Following links
                // would let one junction fan the walk out over an arbitrary tree
                // on every launch.)
                let is_candidate =
                    file_type.is_some_and(|kind| kind.is_file() || kind.is_symlink());
                if is_candidate && rules.denies_normalized(&candidate) {
                    if denied.fetch_add(1, Ordering::Relaxed) >= MAX_DENIED_PATHS {
                        truncated.store(true, Ordering::Relaxed);
                        return WalkState::Quit;
                    }
                    match found.lock() {
                        Ok(mut paths) => paths.push(entry.path().to_path_buf()),
                        Err(_) => return WalkState::Quit,
                    }
                }
                WalkState::Continue
            })
        }
    });

    let fatal = fatal
        .lock()
        .map_err(|_| "deny scan error lock poisoned".to_string())?
        .take();
    if let Some(error) = fatal {
        return Err(error);
    }
    let paths = found
        .lock()
        .map_err(|_| "deny scan result lock poisoned".to_string())?
        .clone();
    Ok(ScanOutcome {
        paths,
        truncated: truncated.load(Ordering::Relaxed),
    })
}

#[cfg(test)]
mod tests {
    use super::super::super::deny_rules::literal_prefix;
    use super::super::super::deny_rules::normalize_pattern;
    use super::*;

    /// The groups a fresh install is seeded with (`[[sandbox.deny]]`).
    fn seeded() -> Vec<DenyRuleGroup> {
        crate::config::default_deny_groups()
            .iter()
            .map(DenyRuleGroup::from)
            .collect()
    }

    fn deny_rules(workspace: &Path, patterns: &[&str]) -> DenyRules {
        let patterns: Vec<String> = patterns.iter().map(|p| p.to_string()).collect();
        build(&seeded(), &patterns, workspace)
            .expect("rules build")
            .0
    }

    fn temp_workspace(name: &str) -> PathBuf {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .join("test-tmp");
        std::fs::create_dir_all(&root).expect("create deny test root");
        let dir = root.join(format!("pig-deny-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create deny test directory");
        dunce::canonicalize(&dir).expect("canonicalize deny test directory")
    }

    #[test]
    fn the_seeded_groups_match_the_names_the_file_tools_refuse() {
        let workspace = temp_workspace("seeded");
        let rules = deny_rules(&workspace, &[]);
        for name in [".env", ".env.local", "id_rsa", "id_ed25519.pub.bak"] {
            let path = workspace.join(name);
            assert!(rules.denies_path(&path), "{name} should be denied");
        }
        for name in [".env.example", ".env.sample", ".env.template", "id_rsa.pub"] {
            let path = workspace.join(name);
            assert!(!rules.denies_path(&path), "{name} should be allowed");
        }
    }

    #[test]
    fn cloud_credentials_are_denied_through_their_directory() {
        let workspace = temp_workspace("cloud");
        let rules = deny_rules(&workspace, &[]);
        assert!(rules.denies_path(&workspace.join(".aws").join("credentials")));
        assert!(rules.denies_path(&workspace.join(".gcp").join("credentials")));
        assert!(!rules.denies_path(&workspace.join("credentials")));
    }

    #[test]
    fn user_globs_and_literals_both_apply() {
        let workspace = temp_workspace("user");
        let secret = workspace.join("secrets");
        std::fs::create_dir_all(&secret).unwrap();
        let rules = deny_rules(&workspace, &["secrets/**", "**/*.pem"]);
        assert!(rules.denies_path(&secret.join("key.pem")));
        assert!(rules.denies_path(&workspace.join("other").join("a.pem")));

        let (_, literals) = build(&seeded(), &["secrets".to_string()], &workspace).unwrap();
        assert_eq!(literals, vec![workspace.join("secrets")]);
    }

    #[test]
    fn a_simple_rule_cannot_exempt_a_seeded_group() {
        let workspace = temp_workspace("no-exempt");
        // Only deny additions are expressible for `deny_read`; there is no
        // "allow" spelling, and a group's exceptions lift only that group.
        let rules = deny_rules(&workspace, &["docs/**"]);
        assert!(rules.denies_path(&workspace.join(".env")));
    }

    #[test]
    fn relative_patterns_anchor_at_the_workspace() {
        let workspace = temp_workspace("relative");
        assert_eq!(
            normalize_pattern("secrets/**", &workspace).replace('\\', "/"),
            format!(
                "{}/secrets/**",
                workspace.to_string_lossy().replace('\\', "/")
            )
        );
        // A bare file-name glob matches at any depth, so it must stay
        // unanchored (the `**/` prefix is added before the workspace check).
        assert_eq!(normalize_pattern("*.pem", &workspace), "**/*.pem");
        // A slash-less literal is a workspace-relative path, not a pattern.
        assert_eq!(
            normalize_pattern("secrets", &workspace).replace('\\', "/"),
            format!("{}/secrets", workspace.to_string_lossy().replace('\\', "/"))
        );
        // An absolute rule is never anchored.
        assert_eq!(
            normalize_pattern("C:/work/vault/**", &workspace),
            "C:/work/vault/**"
        );
        let home = normalize_pattern("~/private/**", &workspace);
        assert!(!home.contains("**/~"), "{home}");
        // A `**`-leading rule is root-independent: anchoring it would turn
        // "any depth anywhere scanned" into "under the workspace only".
        assert_eq!(normalize_pattern("**/secrets", &workspace), "**/secrets");
    }

    /// Regression: a slash-less rule used to be anchored at the workspace
    /// first, so `*.pem` only matched the workspace's top level — the opposite
    /// of the documented any-depth rule.
    #[test]
    fn bare_file_name_rules_match_at_any_depth() {
        let workspace = temp_workspace("bare-name");
        let rules = deny_rules(&workspace, &["*.pem"]);
        assert!(rules.denies_path(&workspace.join("top.pem")));
        assert!(rules.denies_path(&workspace.join("a").join("b").join("deep.pem")));
        assert!(!rules.denies_path(&workspace.join("a").join("b").join("deep.txt")));
    }

    /// Regression: `literal_prefix` used to yield `/C:/work` (unconditional
    /// leading slash), so an absolute glob outside the writable roots never got
    /// an extra scan root and silently expanded to nothing.
    #[test]
    fn literal_prefix_keeps_windows_drive_roots_intact() {
        #[cfg(windows)]
        {
            assert_eq!(
                literal_prefix("C:/work/repo/**/*.pem"),
                Some(PathBuf::from("C:/work/repo"))
            );
            assert_eq!(
                literal_prefix("C:/work/*.pem"),
                Some(PathBuf::from("C:/work"))
            );
            // A drive-root glob has no literal directory to walk safely.
            assert_eq!(literal_prefix("C:/**/*.pem"), Some(PathBuf::from("C:/")));
        }
        assert_eq!(literal_prefix("**/*.pem"), None);
        assert_eq!(literal_prefix("/*.pem"), None);
    }

    /// Regression: exceptions used to be one global set, so the built-in
    /// `.env.example`/`*.pub` exceptions also neutralized a user's own rule.
    /// Exceptions must only lift the rule they belong to.
    #[test]
    fn exceptions_are_scoped_to_their_own_rule() {
        let workspace = temp_workspace("scoped-exceptions");
        let secrets = workspace.join("secrets");
        std::fs::create_dir_all(&secrets).unwrap();
        std::fs::write(secrets.join("config.env.example"), "x").unwrap();
        std::fs::write(secrets.join("id_rsa.pub"), "x").unwrap();

        let rules = deny_rules(&workspace, &["secrets/**"]);
        assert!(
            rules.denies_path(&secrets.join("config.env.example")),
            "a user rule must not inherit the built-in template exception"
        );
        assert!(
            rules.denies_path(&secrets.join("id_rsa.pub")),
            "a user rule must not inherit the built-in public-key exception"
        );
        // The built-in exceptions still protect their own groups.
        assert!(!rules.denies_path(&workspace.join(".env.example")));
        assert!(!rules.denies_path(&workspace.join("id_rsa.pub")));
    }

    /// The scan root itself is never recorded as a denied directory and never
    /// skipped, even when a rule matches that directory: a glob whose literal
    /// prefix is a build-output directory still expands.
    #[test]
    fn a_glob_rooted_in_a_build_directory_still_expands() {
        let workspace = temp_workspace("pruned-root");
        let target = workspace.join("target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("secret.pem"), "x").unwrap();

        let rules = deny_rules(&workspace, &["target/**/*.pem"]);
        let outcome = scan(&target, &rules).expect("scan");
        assert!(
            outcome
                .paths
                .iter()
                .any(|path| path.ends_with("secret.pem")),
            "the explicitly named root must still be walked: {:?}",
            outcome.paths
        );
    }

    /// A drive-root rule is rejected at build time instead of walking an entire
    /// volume on every command.
    #[test]
    fn drive_root_rules_are_rejected() {
        let workspace = temp_workspace("drive-root");
        for rule in ["C:/**/*.pem", "C:/"] {
            let error = match build(&[], &[rule.to_string()], &workspace) {
                Ok(_) => panic!("{rule} must be rejected"),
                Err(error) => error,
            };
            assert!(
                error.to_string().contains("entire drive")
                    || error.to_string().contains("drive root"),
                "{rule}: {error}"
            );
        }
        // A wildcard with no literal directory is allowed: it matches under the
        // policy's writable roots, which are scanned anyway. It just needs the
        // policy's scan to run (every workspace root is always scanned).
        let rules = deny_rules(&workspace, &["/*.pem"]);
        assert!(rules.extra_scan_roots().is_empty());

        // A pattern that fixes no literal text would deny the whole workspace.
        for rule in ["**", "*", "**/*", "?"] {
            let error = match build(&[], &[rule.to_string()], &workspace) {
                Ok(_) => panic!("{rule} must be rejected"),
                Err(error) => error,
            };
            assert!(error.to_string().contains("matches everything"), "{error}");
        }
    }

    #[test]
    fn scan_finds_nested_and_hidden_matches() {
        let workspace = temp_workspace("scan");
        std::fs::create_dir_all(workspace.join("app")).unwrap();
        std::fs::write(workspace.join(".env"), "x").unwrap();
        std::fs::write(workspace.join("app").join(".env.local"), "x").unwrap();
        std::fs::write(workspace.join("app").join(".env.example"), "x").unwrap();
        std::fs::write(workspace.join("app").join("main.rs"), "x").unwrap();

        let rules = deny_rules(&workspace, &[]);
        let outcome = scan(&workspace, &rules).expect("scan");
        let names: Vec<String> = outcome
            .paths
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&".env".to_string()));
        assert!(names.contains(&".env.local".to_string()));
        assert!(!names.contains(&".env.example".to_string()));
        assert!(!names.contains(&"main.rs".to_string()));
    }

    /// Codex parity: the deny walk does not prune build or dependency outputs.
    /// A copied `.env` under `target/` is still a secret the sandbox must not
    /// read, so it is denied like one in the source tree.
    #[test]
    fn scan_does_not_prune_output_directories() {
        let workspace = temp_workspace("prune");
        let target = workspace.join("target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join(".env"), "x").unwrap();
        let nested = workspace.join("src");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join(".env"), "x").unwrap();

        let rules = deny_rules(&workspace, &[]);
        let outcome = scan(&workspace, &rules).expect("scan");
        assert!(outcome.paths.iter().any(|path| path.starts_with(&nested)));
        assert!(
            outcome.paths.iter().any(|path| path.starts_with(&target)),
            "a match under target/ must not be silently pruned: {:?}",
            outcome.paths
        );
    }

    /// No hidden depth cutoff: the old walk stopped at 32 levels without
    /// reporting truncation, leaving deeper matches readable. Codex walks
    /// `**` patterns unbounded unless `glob_scan_max_depth` is configured.
    #[test]
    fn scan_finds_matches_below_the_old_depth_cutoff() {
        let workspace = temp_workspace("deep");
        let mut deep = workspace.clone();
        for level in 0..40 {
            deep = deep.join(format!("d{level}"));
        }
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join(".env"), "x").unwrap();

        let rules = deny_rules(&workspace, &[]);
        let outcome = scan(&workspace, &rules).expect("scan");
        assert!(
            outcome.paths.iter().any(|path| path.ends_with(".env")),
            "a match at depth 40 must be denied: {:?}",
            outcome.paths
        );
    }

    #[test]
    fn denied_directory_is_recorded_and_its_subtree_skipped() {
        let workspace = temp_workspace("dir-deny");
        let secrets = workspace.join("secrets");
        std::fs::create_dir_all(secrets.join("nested")).unwrap();
        std::fs::write(secrets.join("nested").join("a.key"), "x").unwrap();

        // `**/secrets` matches the directory itself: it is denied as one unit
        // and the walker stops descending into it.
        let rules = deny_rules(&workspace, &["**/secrets"]);
        let outcome = scan(&workspace, &rules).expect("scan");
        assert!(
            outcome.paths.iter().any(|path| path == &secrets),
            "the matched directory should be denied as a unit: {:?}",
            outcome.paths
        );
        assert!(
            !outcome.paths.iter().any(|path| path.ends_with("a.key")),
            "the subtree should not be enumerated: {:?}",
            outcome.paths
        );

        // `secrets/**` matches the contained directories instead; the deepest
        // matched directory still covers the file beneath it.
        let rules = deny_rules(&workspace, &["secrets/**"]);
        let outcome = scan(&workspace, &rules).expect("scan");
        assert!(
            outcome
                .paths
                .iter()
                .any(|path| path == &secrets.join("nested")),
            "the nested directory should be recorded: {:?}",
            outcome.paths
        );
        assert!(!outcome.paths.iter().any(|path| path.ends_with("a.key")));
    }

    /// A group removed from the configuration stops contributing its globs
    /// (and its exceptions go with it), while every other group keeps applying.
    /// Removing a group is deleting its `[[sandbox.deny]]` entry.
    #[test]
    fn an_unconfigured_group_denies_nothing() {
        let workspace = temp_workspace("removed-group");
        let enabled = deny_rules(&workspace, &[]);
        assert!(enabled.denies_path(&workspace.join(".env")));
        assert!(enabled.denies_path(&workspace.join("id_rsa")));

        // The seeded table without its first group (the `.env` family).
        let without_env: Vec<DenyRuleGroup> = seeded().into_iter().skip(1).collect();
        let (rules, _) = build(&without_env, &[], &workspace).unwrap();
        assert!(
            !rules.denies_path(&workspace.join(".env")),
            "env now allowed"
        );
        assert!(!rules.denies_path(&workspace.join(".env.local")));
        assert!(
            rules.denies_path(&workspace.join("id_rsa")),
            "ssh still denied"
        );
        assert!(rules.denies_path(&workspace.join(".aws").join("credentials")));

        // An empty table is exactly "no groups at all".
        let (rules, _) = build(&[], &[], &workspace).unwrap();
        assert!(!rules.denies_path(&workspace.join(".env")));
        assert!(!rules.denies_path(&workspace.join("id_rsa")));
        assert!(!rules.denies_path(&workspace.join(".aws").join("credentials")));
    }

    #[test]
    fn invalid_globs_are_rejected() {
        let workspace = temp_workspace("invalid");
        let error = match build(&[], &["[unclosed".to_string()], &workspace) {
            Ok(_) => panic!("an unclosed character class must be rejected"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("invalid sandbox deny rule"),
            "{error}"
        );

        // A configured group fails the same way, and fails closed: an invalid
        // pattern must stop the launch rather than being skipped.
        let groups = vec![DenyRuleGroup {
            globs: vec!["[unclosed".to_string()],
            exceptions: Vec::new(),
            write_exceptions: Vec::new(),
        }];
        let error = match build(&groups, &[], &workspace) {
            Ok(_) => panic!("an unclosed character class must be rejected"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("invalid sandbox deny"),
            "{error}"
        );
    }
}
