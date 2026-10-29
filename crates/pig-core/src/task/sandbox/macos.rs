use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::{NetworkPolicy, ResolvedPolicy, SandboxCommand, SandboxKind};

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
const BASE_POLICY: &str = include_str!("macos_base_policy.sbpl");

const RUNTIME_READ_ROOTS: &[&str] = &[
    "/bin",
    "/sbin",
    "/usr/bin",
    "/usr/sbin",
    "/usr/libexec",
    "/usr/lib",
    "/usr/share",
    "/System/Library",
    "/Library/Apple",
    "/Library/Frameworks",
    "/Library/Java",
    "/Library/Preferences",
    "/etc",
    "/private/etc",
    "/var/db",
    "/private/var/db",
    "/opt/homebrew/bin",
    "/opt/homebrew/lib",
    "/opt/homebrew/Cellar",
    "/opt/homebrew/opt",
    "/opt/homebrew/share",
    "/usr/local/bin",
    "/usr/local/lib",
    "/usr/local/Cellar",
    "/usr/local/opt",
    "/usr/local/share",
    "/Applications",
];

const HOME_RUNTIME_READ_ROOTS: &[&str] = &[
    ".rustup",
    ".cargo/bin",
    ".cargo/registry",
    ".cargo/git",
    ".nvm",
    ".pyenv",
    ".asdf",
    ".local/bin",
    ".local/share/mise",
    ".volta",
    ".sdkman",
    "go/pkg/mod",
];

pub(crate) fn prepare_command(
    command: SandboxCommand,
    policy: &ResolvedPolicy,
) -> std::io::Result<SandboxCommand> {
    // `Unrestricted` never reaches a backend: `prepare_command` returns before
    // building a policy for Full mode.
    if policy.kind == SandboxKind::Unrestricted {
        return Ok(command);
    }
    let mut definitions = Vec::new();
    let mut sections = vec![BASE_POLICY.to_string()];

    let mut readable_roots = policy.read.clone();
    // A write grant implies read access in the policy model, but Seatbelt's
    // `file-write*` allow does not include `file-read*` — and `resolve` drops a
    // read entry a write entry covers, so an escalated retry (write at `/`)
    // would otherwise leave the command unable to read at all.
    for root in &policy.write {
        if !readable_roots.contains(root) {
            readable_roots.push(root.clone());
        }
    }
    for root in runtime_read_roots() {
        if !readable_roots.contains(&root) {
            readable_roots.push(root);
        }
    }
    let (read_policy, _) = access_policy(
        "READABLE_ROOT",
        "file-read* file-test-existence file-map-executable",
        &readable_roots,
        &mut definitions,
    );
    sections.push(read_policy);
    // An empty root list would produce `(allow file-write*\n\n)` — an allow
    // with no filter, i.e. "write anywhere under the other rules". A policy
    // with no write entry must instead keep the base policy's implicit deny, so
    // no write allow is emitted at all: "no write entry" is not "allow all".
    let (write_policy, writable_params) = if policy.write.is_empty() {
        (String::new(), Vec::new())
    } else {
        access_policy(
            "WRITABLE_ROOT",
            "file-write*",
            &policy.write,
            &mut definitions,
        )
    };
    if !write_policy.is_empty() {
        sections.push(write_policy);
    }

    if policy.network == NetworkPolicy::Allow {
        sections.push("(allow network*)".to_string());
    }
    // Denials come after the grants: a later rule wins in Seatbelt. The
    // carve-outs are the workspace metadata that must stay readable but never
    // writable (`.git/hooks`, `.pigcode`, `.agents`, `.aws`) — the same set the
    // Windows backend expresses as read-only entries deeper than the writable
    // root — and literal deny entries follow the same shape.
    let denials = deny_policy(policy, &mut definitions);
    if !denials.is_empty() {
        sections.push(denials);
    }
    // The compiled read-deny rules (`[[sandbox.deny]]` groups and `deny_read`
    // strings) are enforced as access-time regexes, exactly like Codex's
    // Seatbelt backend: no launch-time snapshot, so a file created later is
    // still covered. Each group's exceptions follow its own denies; a later rule
    // wins. There is no hardcoded sensitive-file section any more: the rule
    // groups are the whole table, so deleting a group in the config file
    // switches it off on this backend too.
    let rule_denials = deny_rules_policy(policy)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    if !rule_denials.is_empty() {
        sections.push(rule_denials);
    }
    let anchors = writable_root_anchor_policy(&writable_params);
    if !anchors.is_empty() {
        sections.push(anchors);
    }
    sections.push(cloud_directory_anchor_policy());
    sections.push("(deny system-fcntl (fcntl-command 80 110))".to_string());

    let mut args = vec![OsString::from("-p"), sections.join("\n").into()];
    args.extend(definitions);
    args.push("--".into());
    args.push(command.program);
    args.extend(command.args);

    Ok(SandboxCommand::new(SANDBOX_EXEC.into(), args))
}

fn access_policy(
    param_prefix: &str,
    operations: &str,
    roots: &[PathBuf],
    definitions: &mut Vec<OsString>,
) -> (String, Vec<String>) {
    let mut filters = Vec::new();
    let mut params = Vec::new();
    for (index, root) in roots.iter().enumerate() {
        let param = format!("{param_prefix}_{index}");
        let mut definition = OsString::from(format!("-D{param}="));
        definition.push(root.as_os_str());
        definitions.push(definition);
        params.push(param.clone());
        let matcher = if root.is_dir() || !root.exists() {
            "subpath"
        } else {
            "literal"
        };
        filters.push(format!("({matcher} (param \"{param}\"))"));
    }
    (
        format!("(allow {operations}\n{}\n)", filters.join("\n")),
        params,
    )
}

/// Compile the scoped deny rules into Seatbelt denies plus per-group exception
/// allows.
///
/// A later rule wins, so each group's exceptions follow that group's denies;
/// for a scoped pattern the ancestors of its literal prefix also deny
/// `file-write-unlink`, so a matching directory cannot be renamed out of the
/// pattern's reach (Codex's Seatbelt backend does the same).
fn deny_rules_policy(policy: &ResolvedPolicy) -> Result<String, String> {
    let mut sections: Vec<String> = Vec::new();
    for group in &policy.deny_groups {
        for pattern in &group.globs {
            let regex = seatbelt_regex_for_glob(pattern)?;
            sections.push(format!(
                "(deny file-read* file-write* (regex #\"{regex}\"))"
            ));
            if !pattern.starts_with("**/")
                && let Some(prefix) = super::deny_rules::literal_prefix(pattern)
            {
                for ancestor in prefix.ancestors().skip(1) {
                    let regex = seatbelt_regex_for_glob(&ancestor.to_string_lossy())?;
                    sections.push(format!(
                        "(deny file-write-unlink (require-all (vnode-type DIRECTORY) (regex #\"{regex}\")))"
                    ));
                }
            }
        }
        for exception in &group.exceptions {
            let regex = seatbelt_regex_for_glob(exception)?;
            // The exception restores exactly what the denial removed: a
            // read exception stays read-only, so a path excepted from a
            // deny is not thereby made writable where nothing grants it.
            sections.push(format!("(allow file-read* (regex #\"{regex}\"))"));
        }
        for exception in &group.write_exceptions {
            let regex = seatbelt_regex_for_glob(exception)?;
            sections.push(format!(
                "(allow file-read* file-write* (regex #\"{regex}\"))"
            ));
        }
    }
    Ok(sections.join(
        "
",
    ))
}

mod glob;

use glob::seatbelt_regex_for_glob;

/// Denials that must win over the grants, emitted after the allows (a later
/// rule wins).
///
/// Two kinds, and they are not the same rule:
///
/// * a **carve-out** is read-only: it stays readable and loses the write grant
///   that covers it (`.git/hooks`, `.pigcode`, `.agents`, `.aws`, and any read
///   entry configured inside a writable root). This is Codex's
///   `read_only_subpaths`, which Seatbelt expresses as a write exclusion;
/// * a **literal deny entry** is refused in both directions, because that is
///   what the user asked for.
///
/// Denying reads for a carve-out would break the tools that must read them —
/// git reads `.git/config` on every command — and would turn a configured read
/// grant into an unreadable path.
fn deny_policy(policy: &ResolvedPolicy, definitions: &mut Vec<OsString>) -> String {
    let mut sections: Vec<String> = Vec::new();
    let mut carve_outs: Vec<PathBuf> = Vec::new();
    for path in &policy.carve_outs {
        if !carve_outs.contains(path) && !policy.deny.contains(path) {
            carve_outs.push(path.clone());
        }
    }
    if !carve_outs.is_empty() {
        let (allowed, params) =
            access_policy("READ_ONLY_PATH", "file-write*", &carve_outs, definitions);
        sections.push(allowed.replace("(allow ", "(deny "));
        // Renaming a directory must not move a carve-out out of reach, the same
        // anchor the scoped rules carry.
        for param in params {
            sections.push(format!(
                "(deny file-write-unlink (require-all (vnode-type DIRECTORY) (literal (param \"{param}\"))))"
            ));
        }
    }
    if !policy.deny.is_empty() {
        let (allowed, _) = access_policy(
            "DENIED_PATH",
            "file-read* file-write*",
            &policy.deny,
            definitions,
        );
        sections.push(allowed.replace("(allow ", "(deny "));
    }
    sections.join("\n")
}

fn writable_root_anchor_policy(params: &[String]) -> String {
    params
        .iter()
        .map(|param| {
            format!(
                "(deny file-write-unlink (require-all (literal (param \"{param}\")) (vnode-type DIRECTORY)))"
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn runtime_read_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for root in RUNTIME_READ_ROOTS {
        push_existing_root(&mut roots, Path::new(root));
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        for root in HOME_RUNTIME_READ_ROOTS {
            push_existing_root(&mut roots, &home.join(root));
        }
    }
    roots
}

fn push_existing_root(roots: &mut Vec<PathBuf>, root: &Path) {
    let Ok(root) = root.canonicalize() else {
        return;
    };
    if !roots.contains(&root) {
        roots.push(root);
    }
}

fn cloud_directory_anchor_policy() -> String {
    let cloud_dirs = [".aws", ".gcp"].map(regex_literal).join("|");
    format!(
        "(deny file-write-unlink (require-all (vnode-type DIRECTORY) (regex #\"(^|/)({cloud_dirs})$\")))"
    )
}

fn regex_literal(value: &str) -> String {
    let mut output = String::new();
    for character in value.chars() {
        if character.is_ascii_alphabetic() {
            output.push('[');
            output.push(character.to_ascii_lowercase());
            output.push(character.to_ascii_uppercase());
            output.push(']');
        } else {
            if matches!(
                character,
                '\\' | '.' | '[' | ']' | '{' | '}' | '(' | ')' | '*' | '+' | '?' | '^' | '$' | '|'
            ) {
                output.push('\\');
            }
            output.push(character);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pig_core_deny_group(
        globs: Vec<&str>,
        exceptions: Vec<&str>,
    ) -> super::super::deny_rules::DenyRuleGroup {
        super::super::deny_rules::DenyRuleGroup {
            globs: globs.into_iter().map(str::to_string).collect(),
            exceptions: exceptions.into_iter().map(str::to_string).collect(),
            write_exceptions: Vec::new(),
        }
    }

    /// The groups a fresh install is seeded with (`[[sandbox.deny]]`).
    fn seeded() -> Vec<super::super::deny_rules::DenyRuleGroup> {
        crate::config::default_deny_groups()
            .iter()
            .map(super::super::deny_rules::DenyRuleGroup::from)
            .collect()
    }

    fn resolved(read: Vec<PathBuf>, write: Vec<PathBuf>) -> ResolvedPolicy {
        ResolvedPolicy {
            kind: SandboxKind::Restricted,
            read,
            write,
            carve_outs: Vec::new(),
            deny: Vec::new(),
            deny_scan_roots: Vec::new(),
            deny_groups: Vec::new(),
            network: NetworkPolicy::Allow,
            deny_sensitive_files: true,
        }
    }

    fn policy() -> ResolvedPolicy {
        resolved(
            vec![PathBuf::from("/workspace")],
            vec![PathBuf::from("/workspace")],
        )
    }

    #[test]
    fn profile_places_sensitive_denies_and_root_anchors_after_allows() {
        let mut policy = policy();
        policy.deny_groups = seeded();
        let command = prepare_command(
            SandboxCommand::new("/bin/bash".into(), vec!["-c".into(), "pwd".into()]),
            &policy,
        )
        .expect("build Seatbelt command");
        let profile = command.args[1].to_string_lossy();
        let network = profile.find("(allow network*)").expect("network allow");
        let sensitive = profile
            .find("(deny file-read* file-write*")
            .expect("sensitive deny");
        // The seeded exceptions are read-only allows, so the first exception
        // allow after the deny block is the `.env` family's.
        let exceptions = sensitive
            + profile[sensitive..]
                .find("(allow file-read* (regex")
                .expect("sensitive exceptions");
        let anchor = profile
            .rfind("WRITABLE_ROOT_0")
            .expect("writable root anchor");

        assert!(network < sensitive);
        assert!(sensitive < exceptions);
        assert!(exceptions < anchor);
        assert!(profile.contains("(deny system-fcntl (fcntl-command 80 110))"));
    }

    /// The seeded groups arrive as configuration and land in the profile as
    /// access-time regexes: the `.env` family is denied, its examples are
    /// re-allowed read-only, and the SSH keys keep their `*.pub` exception.
    /// Deleting a group from the config deletes exactly its regexes.
    #[test]
    fn the_seeded_groups_become_regexes_and_can_be_deleted() {
        let mut policy = policy();
        policy.deny_groups = seeded();
        let profile = prepare_command(
            SandboxCommand::new("/bin/bash".into(), vec!["-c".into(), "true".into()]),
            &policy,
        )
        .expect("build Seatbelt command")
        .args[1]
            .to_string_lossy()
            .to_string();
        assert!(
            profile.contains(r#"(deny file-read* file-write* (regex #"^(.*/)?\.[eE][nN][vV]$"))"#),
            "{profile}"
        );
        assert!(
            profile.contains(r#"(allow file-read* (regex #"^(.*/)?\.[eE][nN][vV]\.[eE][xX]"#),
            "{profile}"
        );
        assert!(profile.contains("[iI][dD]_[rR][sS][aA]"), "{profile}");
        assert!(
            profile.contains(r#"(allow file-read* (regex #"^(.*/)?[^/]*\.[pP][uU][bB]$"))"#),
            "the ssh group's public-key exception: {profile}"
        );

        // Without the `.env` group its regexes are gone and the SSH rule stays.
        policy.deny_groups = seeded().into_iter().skip(1).collect();
        let profile = prepare_command(
            SandboxCommand::new("/bin/bash".into(), vec!["-c".into(), "true".into()]),
            &policy,
        )
        .expect("build Seatbelt command")
        .args[1]
            .to_string_lossy()
            .to_string();
        assert!(!profile.contains(r"\[eE][nN][vV]"), "{profile}");
        assert!(profile.contains("[iI][dD]_[rR][sS][aA]"), "{profile}");
    }

    /// The metadata carve-outs lose the write grant, so the model cannot write
    /// executable state (`.pigcode/hooks.toml`, `.git/hooks`, …) even though the
    /// workspace itself is writable. This is Codex's `protected_metadata_names`
    /// (its `read_only_subpaths`) on Seatbelt.
    ///
    /// A carve-out stays **readable**: git reads `.git/config` on every command,
    /// and a configured read entry inside a writable root would otherwise turn
    /// into an unreadable path. Only the literal deny entries refuse reads.
    #[test]
    fn carve_outs_lose_the_write_grant_but_stay_readable() {
        let mut policy = policy();
        policy.carve_outs = vec![
            PathBuf::from("/workspace/.pigcode"),
            PathBuf::from("/workspace/.git/hooks"),
        ];
        let command = prepare_command(
            SandboxCommand::new("/bin/bash".into(), vec!["-c".into(), "true".into()]),
            &policy,
        )
        .expect("build Seatbelt command");
        let profile = command.args[1].to_string_lossy();

        let write_grant = profile.find("(allow file-write*").expect("write grant");
        let carve_out = profile.rfind("(deny file-write*\n").expect("metadata deny");
        assert!(
            write_grant < carve_out,
            "the deny must come after the grant: {profile}"
        );
        assert!(profile.contains("READ_ONLY_PATH_0"));
        assert!(profile.contains("READ_ONLY_PATH_1"));
        assert!(
            !profile[carve_out..].contains("file-read*"),
            "a carve-out must stay readable: {profile}"
        );
        // Renaming a protected directory cannot move it out of reach.
        assert!(profile[carve_out..].contains("file-write-unlink"));
    }

    /// A literal deny entry is refused in both directions: the user asked for
    /// the path to be off limits, not merely unwritable.
    #[test]
    fn a_literal_deny_entry_refuses_reads_and_writes() {
        let mut policy = policy();
        policy.deny = vec![PathBuf::from("/workspace/vault")];
        let command = prepare_command(
            SandboxCommand::new("/bin/bash".into(), vec!["-c".into(), "true".into()]),
            &policy,
        )
        .expect("build Seatbelt command");
        let profile = command.args[1].to_string_lossy();
        let deny = profile.rfind("DENIED_PATH_0").expect("literal deny");
        assert!(
            profile[..deny]
                .rfind("(deny file-read* file-write*")
                .is_some()
        );
    }

    /// A policy with no write entry emits no `(allow file-write*` at all: an
    /// unfiltered allow would mean "write anywhere", the opposite of read-only.
    #[test]
    fn a_policy_without_write_entries_has_no_write_allow() {
        let policy = resolved(vec![PathBuf::from("/workspace")], Vec::new());
        let command = prepare_command(
            SandboxCommand::new("/bin/bash".into(), vec!["-c".into(), "true".into()]),
            &policy,
        )
        .expect("build Seatbelt command");
        let profile = command.args[1].to_string_lossy();

        // The sensitive-file policy has its own `allow` line for exceptions, and
        // the base policy may too; what must not appear is a write grant.
        assert!(
            !profile.contains("(allow file-write*"),
            "read-only must not emit a write allow: {profile}"
        );
    }

    /// The compiled deny rules are enforced as access-time regexes, with each
    /// group's exceptions allowed *after* its denies.
    #[test]
    fn deny_rules_are_regexes_with_scoped_exceptions() {
        let mut policy = policy();
        policy.deny_groups = vec![
            pig_core_deny_group(vec!["**/.env", "**/.env.*"], vec!["**/.env.example"]),
            pig_core_deny_group(vec!["/work/repo/secrets/**"], Vec::new()),
        ];
        let command = prepare_command(
            SandboxCommand::new("/bin/bash".into(), vec!["-c".into(), "true".into()]),
            &policy,
        )
        .expect("build Seatbelt command");
        let profile = command.args[1].to_string_lossy();

        let env_deny = profile
            .find("(deny file-read* file-write* (regex #\"^(.*/)?\\.[eE][nN][vV]$\"))")
            .expect("env deny");
        let env_exception = profile
            .find("(allow file-read* (regex #\"^(.*/)?\\.[eE][nN][vV]\\.[eE][xX]")
            .expect("env exception");
        assert!(
            env_deny < env_exception,
            "an exception must come after its group's deny: {profile}"
        );
        assert!(
            !profile.contains("(allow file-read* file-write* (regex #\"^(.*/)?\\.[eE][nN][vV]"),
            "a read exception must not restore write access: {profile}"
        );
        assert!(
            profile.contains("[sS][eE][cC][rR][eE][tT][sS]"),
            "the user rule is compiled too (letters are case-expanded): {profile}"
        );
    }

    /// `resolve` drops a read entry a write entry covers, and Seatbelt's
    /// `file-write*` allow does not include `file-read*` — so a write-only
    /// root (the escalated retry's `/`) must join the readable set explicitly.
    #[test]
    fn writable_roots_are_readable() {
        let mut policy = policy();
        policy.read = Vec::new();
        policy.write = vec![PathBuf::from("/writable-only")];
        let command = prepare_command(
            SandboxCommand::new("/bin/bash".into(), vec!["-c".into(), "true".into()]),
            &policy,
        )
        .expect("build Seatbelt command");

        assert!(
            command
                .args
                .iter()
                .any(|arg| arg.to_string_lossy() == "-DREADABLE_ROOT_0=/writable-only"),
            "a write-only root still lands in the readable set: {:?}",
            command.args
        );
    }

    /// `*` never crosses a separator and `**/` matches any depth, matching the
    /// Windows matcher's semantics. The `**/` prefix is a *capturing* group:
    /// Seatbelt's POSIX ERE dialect has no `(?:…)`, and a rule whose regex fails
    /// to compile is silently dropped.
    #[test]
    fn glob_translation_matches_the_matcher_semantics() {
        assert_eq!(
            seatbelt_regex_for_glob("**/.env").unwrap(),
            "^(.*/)?\\.[eE][nN][vV]$"
        );
        assert_eq!(
            seatbelt_regex_for_glob("/w/*.pem").unwrap(),
            "^/[wW]/[^/]*\\.[pP][eE][mM]$"
        );
        assert_eq!(
            seatbelt_regex_for_glob("**/*.pem").unwrap(),
            "^(.*/)?[^/]*\\.[pP][eE][mM]$"
        );
        // A trailing `/**` is a recursive suffix, while `**` glued to a name
        // is two ordinary stars (no separator crossing).
        assert_eq!(seatbelt_regex_for_glob("/w/**").unwrap(), "^/[wW]/.*$");
        assert_eq!(
            seatbelt_regex_for_glob("/w/**.pem").unwrap(),
            "^/[wW]/[^/]*\\.[pP][eE][mM]$"
        );
        // `**` glued to a literal is not a recursive component, even when a
        // slash follows: globset still treats it as two ordinary stars.
        assert_eq!(
            seatbelt_regex_for_glob("/w/a**/x").unwrap(),
            "^/[wW]/[aA][^/]*/[xX]$"
        );
    }

    /// Character classes used to be escaped as literal brackets, so a rule
    /// `secret[0-9].txt` denied a file whose name contains `[0-9]` and let the
    /// real matches through. They now become POSIX bracket expressions, with
    /// ASCII case folding matching the globset matcher.
    #[test]
    fn glob_character_classes_become_posix_classes() {
        assert_eq!(
            seatbelt_regex_for_glob("/w/secret[0-9].txt").unwrap(),
            "^/[wW]/[sS][eE][cC][rR][eE][tT][0-9]\\.[tT][xX][tT]$"
        );
        assert_eq!(
            seatbelt_regex_for_glob("/w/[a-z].pem").unwrap(),
            "^/[wW]/[A-Za-z]\\.[pP][eE][mM]$"
        );
        assert_eq!(
            seatbelt_regex_for_glob("/w/[!0-9].txt").unwrap(),
            "^/[wW]/[^0-9]\\.[tT][xX][tT]$"
        );
        // A trailing `-` is a literal member, not a range operator. The
        // alphabetic member is still case-folded.
        assert_eq!(
            seatbelt_regex_for_glob("[a-].txt").unwrap(),
            "^[-Aa]\\.[tT][xX][tT]$"
        );
    }

    /// Brace alternation was silently treated as literal braces. Each branch is
    /// rendered as a POSIX group (no `(?:…)`), including nesting.
    #[test]
    fn glob_brace_alternation_becomes_a_posix_group() {
        assert_eq!(
            seatbelt_regex_for_glob("/w/{alpha,beta}.txt").unwrap(),
            "^/[wW]/([aA][lL][pP][hH][aA]|[bB][eE][tT][aA])\\.[tT][xX][tT]$"
        );
        assert_eq!(
            seatbelt_regex_for_glob("/w/{a,{b,c}}.txt").unwrap(),
            "^/[wW]/([aA]|([bB]|[cC]))\\.[tT][xX][tT]$"
        );
        assert_eq!(
            seatbelt_regex_for_glob("/w/**.{rs,toml}").unwrap(),
            "^/[wW]/[^/]*\\.([rR][sS]|[tT][oO][mM][lL])$"
        );
    }

    /// Anything the parser cannot represent exactly fails the launch. Dropping
    /// the deny rule would silently widen the policy.
    #[test]
    fn unrepresentable_glob_rules_fail_closed() {
        assert!(seatbelt_regex_for_glob("[abc").is_err());
        assert!(seatbelt_regex_for_glob("{a,b").is_err());
        assert!(seatbelt_regex_for_glob("a}").is_err());
        assert!(seatbelt_regex_for_glob("{,a}").is_err());
        assert!(seatbelt_regex_for_glob("{a,}").is_err());
        assert!(seatbelt_regex_for_glob("[z-a]").is_err());
        assert!(seatbelt_regex_for_glob("[[a]").is_err());
        assert!(seatbelt_regex_for_glob("line\nbreak").is_err());

        // The launch itself must fail instead of silently dropping the rule.
        let mut policy = policy();
        policy.deny_groups = vec![pig_core_deny_group(vec!["line\nbreak"], Vec::new())];
        assert!(
            prepare_command(SandboxCommand::new("/bin/true".into(), Vec::new()), &policy).is_err(),
            "an unrepresentable deny rule must abort the launch"
        );
    }

    #[test]
    fn command_preserves_original_argv_after_separator() {
        let command = prepare_command(
            SandboxCommand::new(
                "/bin/bash".into(),
                vec!["-c".into(), "printf '%s' \"$1\"".into()],
            ),
            &policy(),
        )
        .expect("build Seatbelt command");
        let separator = command
            .args
            .iter()
            .position(|arg| arg == "--")
            .expect("separator");

        assert_eq!(command.program, OsString::from(SANDBOX_EXEC));
        assert_eq!(
            &command.args[separator + 1..],
            &["/bin/bash", "-c", "printf '%s' \"$1\""]
        );
    }

    #[cfg(unix)]
    #[test]
    fn definitions_preserve_non_utf8_paths_outside_profile() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let root = OsString::from_vec(b"/tmp/pig-\xff".to_vec());
        let command = prepare_command(
            SandboxCommand::new("/bin/true".into(), Vec::new()),
            &resolved(vec![PathBuf::from(&root)], vec![PathBuf::from(&root)]),
        )
        .expect("build Seatbelt command");
        let definition = command
            .args
            .iter()
            .find(|arg| arg.as_bytes().starts_with(b"-DREADABLE_ROOT_0="))
            .expect("root definition");

        assert!(definition.as_bytes().ends_with(b"/tmp/pig-\xff"));
        assert!(!command.args[1].as_bytes().contains(&0xff));
    }

    /// Real `sandbox-exec` regression: character classes and brace alternation
    /// were previously emitted as literal characters, so the translated rule
    /// compiled and matched the wrong names. Run the generated profile against
    /// a real kernel to prove both construct shapes deny their matches, allow
    /// non-matches, and keep `*` from crossing a separator.
    #[cfg(target_os = "macos")]
    #[test]
    fn sandbox_exec_enforces_character_classes_and_braces() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tmp")
            .join(format!("pig-seatbelt-glob-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create fixture");
        let root = root.canonicalize().expect("canonical fixture");
        for (name, content) in [
            ("secret1.txt", "CLASS-DENIED"),
            ("secretx.txt", "CLASS-ALLOWED"),
            ("alpha.txt", "BRACE-DENIED-A"),
            ("beta.txt", "BRACE-DENIED-B"),
            ("gamma.txt", "BRACE-ALLOWED"),
            ("nested/secret1.txt", "NESTED-CLASS"),
        ] {
            let path = root.join(name);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("create dir");
            std::fs::write(path, content).expect("write fixture");
        }
        let mut policy = resolved(vec![root.clone()], vec![root.clone()]);
        policy.deny_groups = vec![
            pig_core_deny_group(vec![&format!("{}/secret[0-9].txt", root.display())], vec![]),
            pig_core_deny_group(
                vec![&format!("{}/{{alpha,beta}}.txt", root.display())],
                vec![],
            ),
        ];

        let cat = |name: &str| {
            let command = prepare_command(
                SandboxCommand::new("/bin/cat".into(), vec![root.join(name).into_os_string()]),
                &policy,
            )
            .expect("build Seatbelt command");
            std::process::Command::new(command.program)
                .args(&command.args)
                .output()
                .expect("run sandbox-exec")
        };

        for denied in ["secret1.txt", "alpha.txt", "beta.txt"] {
            let output = cat(denied);
            assert!(
                !output.status.success(),
                "{denied} must be denied by the translated regex: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                !String::from_utf8_lossy(&output.stdout).contains("DENIED"),
                "{denied} leaked content"
            );
        }
        for allowed in ["secretx.txt", "gamma.txt"] {
            let output = cat(allowed);
            assert!(
                output.status.success(),
                "{allowed} must stay readable: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        // `*` does not cross `/`: the class rule is anchored at the named
        // directory, so a nested copy is outside the rule.
        let nested = cat("nested/secret1.txt");
        assert!(
            nested.status.success(),
            "a separator must not be crossed by the translated class rule: {}",
            String::from_utf8_lossy(&nested.stderr)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The translation itself is tested above; this pins that the seeded
    /// patterns come out case-insensitive with their exceptions intact, which is
    /// what makes the profile refuse `.ENV` on a case-preserving filesystem.
    #[test]
    fn sensitive_regexes_are_case_insensitive_and_keep_exceptions() {
        let rules = super::super::deny_rules::from_groups(&seeded()).expect("compile seeded");
        for pattern in [
            "**/.env",
            "**/.env.example",
            "**/id_rsa",
            "**/*.pub",
            "**/.aws/credentials",
        ] {
            let regex = seatbelt_regex_for_glob(pattern).expect("regex");
            assert!(regex.contains('['), "{pattern}: {regex}");
        }
        assert!(rules.denies_path(Path::new("/home/u/.ENV")));
        assert!(!rules.denies_path(Path::new("/home/u/.ENV.EXAMPLE")));
        assert!(rules.denies_path(Path::new("/home/u/.SSH/ID_RSA")));
        assert!(!rules.denies_path(Path::new("/home/u/.SSH/ID_RSA.PUB")));
    }
}
