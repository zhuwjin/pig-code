use super::*;

/// Conservative read-only command check (for AutoEdit pass-through; better to miss than to let through): a single simple command —
/// no pipes/redirections/chaining/command substitution/multiple lines. After the shape gate:
/// - file-dumping commands (cat/head/tail/sort/uniq) go through the argument-level check (readonly_dump_command):
///   sensitive files, escapes from the workspace, globs, stdin, follow mode, and write-output options are never allowed;
/// - other commands need their first word in the allowlist; git additionally checks the subcommand allowlist (branch/remote/tag only in the argument-less list form).
pub fn is_readonly_command(command: &str, cwd: &Path) -> bool {
    if command
        .chars()
        .any(|c| matches!(c, '>' | '<' | '|' | '&' | ';' | '`' | '\n' | '\r'))
        || command.contains("$(")
    {
        return false;
    }
    let tokens: Vec<&str> = command.split_whitespace().collect();
    let Some(&first) = tokens.first() else {
        return false;
    };
    if matches!(first, "cat" | "head" | "tail" | "sort" | "uniq") {
        return readonly_dump_command(first, &tokens[1..], cwd);
    }
    const READONLY: &[&str] = &[
        "ls", "pwd", "echo", "find", "grep", "rg", "wc", "file", "stat", "which", "whoami", "date",
        "uname", "hostname", "tree", "du", "df", "diff",
    ];
    if READONLY.contains(&first) {
        return true;
    }
    if first == "git" {
        let second = tokens.get(1).copied().unwrap_or("");
        const GIT_READONLY: &[&str] = &[
            "status",
            "log",
            "diff",
            "show",
            "rev-parse",
            "ls-files",
            "blame",
            "describe",
            "shortlog",
        ];
        if GIT_READONLY.contains(&second) {
            return true;
        }
        // branch/remote/tag only in the pure list form (no third argument)
        if matches!(second, "branch" | "remote" | "tag") && tokens.len() == 2 {
            return true;
        }
    }
    false
}

/// Argument-level read-only check for file-dumping commands. The shape gate already guarantees no pipes/substitution, so tokens are arguments
/// with no quoting ambiguity; the option tables list only common shapes, and unrecognized options are treated as boolean flags — the misjudgment direction is
/// "one extra approval dialog", never letting something slip through (fail-closed). Rejected shapes:
/// tail -f/-F (long-running follow output), sort -o/--output and uniq's second file argument (write output),
/// glob characters (expansion results unknowable), `-`/`--`/no file argument (stdin semantics),
/// paths escaping the workspace (including MSYS absolute paths /x on Windows), sensitive file names.
fn readonly_dump_command(cmd: &str, args: &[&str], cwd: &Path) -> bool {
    let mut files: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let token = args[i];
        i += 1;
        if token == "-" || token == "--" {
            return false;
        }
        let Some(flag) = token.strip_prefix('-') else {
            files.push(token);
            continue;
        };
        if let Some(long) = flag.strip_prefix('-') {
            // Long option: name or name=value
            let name = long.split('=').next().unwrap_or("");
            if name.is_empty() {
                return false;
            }
            if matches!((cmd, name), ("tail", "follow") | ("sort", "output")) {
                return false;
            }
            // Long options that consume an argument value (the --lines 50 form; --lines=50 carries its own value)
            let takes_value = matches!(
                (cmd, name),
                ("head" | "tail", "lines" | "bytes")
                    | (
                        "sort",
                        "key" | "field-separator" | "buffer-size" | "temporary-directory"
                    )
                    | ("uniq", "skip-fields" | "skip-chars" | "check-chars")
            );
            if takes_value && !long.contains('=') {
                i += 1;
            }
            continue;
        }
        // Short option: the first character decides the semantics; further characters count as an attached value (-n50), otherwise consume the next token
        let mut chars = flag.chars();
        let Some(first) = chars.next() else {
            return false;
        };
        let attached_value = chars.next().is_some();
        // tail -f/-F is a long-running follow; sort -o writes an output file
        if matches!((cmd, first), ("tail", 'f') | ("tail", 'F') | ("sort", 'o')) {
            return false;
        }
        let takes_value = matches!(
            (cmd, first),
            ("head" | "tail", 'n' | 'c')
                | ("sort", 'k' | 't' | 'S' | 'T')
                | ("uniq", 'f' | 's' | 'w')
        );
        if takes_value && !attached_value {
            i += 1;
        }
    }
    if files.is_empty() {
        return false; // no file argument = read stdin
    }
    // uniq's second positional argument is the output file (uniq in out)
    if cmd == "uniq" && files.len() > 1 {
        return false;
    }
    files.iter().all(|file| {
        !file.contains(['*', '?', '['])
            && !file.starts_with('/')
            && normalize_within(cwd, file)
                .is_some_and(|resolved| resolved.starts_with(cwd) && !is_sensitive_file(&resolved))
    })
}

/// Lexical normalization (no disk access): absolute paths collapse directly; relative paths are joined onto cwd, then `.`/`..` collapsed.
/// `..` escaping the starting root returns None. No canonicalize — the target of this check is "the model's casual misread";
/// symlink disguise is an adversarial scenario, left to the approval/permission layer.
fn normalize_within(cwd: &Path, arg: &str) -> Option<PathBuf> {
    let raw = Path::new(arg);
    let components: Vec<std::path::Component> = if raw.is_absolute() {
        raw.components().collect()
    } else {
        cwd.components().chain(raw.components()).collect()
    };
    let mut stack: Vec<std::path::Component> = Vec::with_capacity(components.len());
    for component in components {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => match stack.last() {
                Some(std::path::Component::Normal(_)) => {
                    stack.pop();
                }
                _ => return None,
            },
            other => stack.push(other),
        }
    }
    Some(stack.into_iter().collect())
}
