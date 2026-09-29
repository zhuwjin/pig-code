use super::*;

/// 保守只读命令判定（AutoEdit 直通用，宁漏不放）：单条简单命令——
/// 无管道/重定向/链式/命令替换/多行。形态门之后：
/// - 吐文件类命令（cat/head/tail/sort/uniq）走参数级判定（readonly_dump_command）：
///   敏感文件、越出工作区、glob、stdin、follow 模式、写输出选项一律不放行；
/// - 其余首词在白名单；git 再看子命令白名单（branch/remote/tag 仅无参列表形态）。
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
        // branch/remote/tag 仅纯列表形态（无第三个参数）
        if matches!(second, "branch" | "remote" | "tag") && tokens.len() == 2 {
            return true;
        }
    }
    false
}

/// 吐文件类命令的参数级只读判定。形态门已保证无管道/替换，token 即参数，
/// 无引号歧义；选项表只列常见形态，未识别选项按布尔处理——误判方向是
/// 「多弹一次审批」而非漏放（fail-closed）。拒绝的形态：
/// tail -f/-F（跟随输出长驻）、sort -o/--output 与 uniq 的第二文件参数（写输出）、
/// glob 字符（展开结果不可知）、`-`/`--`/无文件参数（stdin 语义）、
/// 路径越出工作区（含 Windows 下 MSYS 绝对路径 /x）、敏感文件名。
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
            // 长选项：name 或 name=value
            let name = long.split('=').next().unwrap_or("");
            if name.is_empty() {
                return false;
            }
            if matches!((cmd, name), ("tail", "follow") | ("sort", "output")) {
                return false;
            }
            // 吃一个参数值的长选项（--lines 50 形态；--lines=50 自带值）
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
        // 短选项：首字符定语义；后续字符存在视为附着值（-n50），否则吃下一 token
        let mut chars = flag.chars();
        let Some(first) = chars.next() else {
            return false;
        };
        let attached_value = chars.next().is_some();
        // tail -f/-F 跟随输出长驻；sort -o 写输出文件
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
        return false; // 无文件参数 = 读 stdin
    }
    // uniq 的第二个位置参数是输出文件（uniq in out）
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

/// 词法归一（不碰盘）：绝对路径直接折叠；相对路径从 cwd 起拼接后折叠 `.`/`..`。
/// `..` 越过起始根返回 None。不做 canonicalize——判定目标是「模型顺手的误读」，
/// 符号链接伪装属于对抗场景，交由审批/权限层。
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
