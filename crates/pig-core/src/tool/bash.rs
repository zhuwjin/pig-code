use super::*;

/// 保守的破坏性命令黑名单（非 AST，只拦明确形态；命中即拒绝并说明理由）。
/// 宁可误拦也不放行，误拦文案引导用户手动执行。返回 Some(原因) 表示应拦截。
pub fn is_dangerous_command(command: &str) -> Option<&'static str> {
    let tokens: Vec<&str> = command.split_whitespace().collect();
    let compact: String = command.chars().filter(|c| !c.is_whitespace()).collect();

    // fork 炸弹：:(){ :|:& };:
    if compact.contains(":(){") && compact.contains("|:&") {
        return Some("fork 炸弹");
    }

    // 命令位判定：首 token，或跟在 ; & | && || sudo then do if ! ( 之后
    let in_command_position = |i: usize| {
        if i == 0 {
            return true;
        }
        let prev = tokens[i - 1];
        prev.ends_with(';')
            || prev.ends_with('&')
            || matches!(
                prev,
                "|" | "&&" | "||" | ";" | "(" | "sudo" | "then" | "do" | "if" | "!"
            )
    };
    fn base_name(token: &str) -> &str {
        token.rsplit('/').next().unwrap_or(token)
    }

    for (i, token) in tokens.iter().enumerate() {
        let base = base_name(token);
        if !in_command_position(i) {
            continue;
        }
        // rm -rf/-fr 且目标为 / /* ~ ~/ $HOME .（普通 rm -rf node_modules 放行）
        if base == "rm" {
            let mut recursive_force = false;
            let mut dangerous_target = false;
            for t in &tokens[i + 1..] {
                if t.starts_with('-') && !t.starts_with("--") {
                    let flags = t.trim_start_matches('-');
                    if (flags.contains('r') || flags.contains('R')) && flags.contains('f') {
                        recursive_force = true;
                    }
                } else if matches!(*t, "/" | "/*" | "~" | "~/" | "$HOME" | ".") {
                    dangerous_target = true;
                }
            }
            if recursive_force && dangerous_target {
                return Some("rm -rf 指向根/家/当前目录");
            }
        }
        // 磁盘格式化/分区
        if base == "mkfs" || base.starts_with("mkfs.") || base == "fdisk" {
            return Some("磁盘格式化/分区操作");
        }
        if base == "diskutil"
            && tokens
                .get(i + 1)
                .is_some_and(|next| next.starts_with("erase"))
        {
            return Some("磁盘格式化/分区操作");
        }
        // dd 写块设备（of=/dev/…，字符设备白名单放行）
        if base == "dd" {
            for t in &tokens[i + 1..] {
                if let Some(target) = t.strip_prefix("of=") {
                    if target.starts_with("/dev/")
                        && !matches!(
                            target,
                            "/dev/null" | "/dev/zero" | "/dev/random" | "/dev/urandom"
                        )
                    {
                        return Some("dd 写入块设备");
                    }
                }
            }
        }
        // 关机/重启
        if matches!(base, "shutdown" | "reboot" | "halt" | "poweroff") {
            return Some("关机/重启操作");
        }
        if base == "systemctl"
            && tokens
                .get(i + 1)
                .is_some_and(|next| matches!(*next, "poweroff" | "reboot" | "halt" | "kexec"))
        {
            return Some("关机/重启操作");
        }
        if base == "init"
            && tokens
                .get(i + 1)
                .is_some_and(|next| matches!(*next, "0" | "6"))
        {
            return Some("关机/重启操作");
        }
        // 递归改权/改属根目录
        if base == "chmod" || base == "chown" {
            let recursive = tokens[i + 1..]
                .iter()
                .any(|t| t.starts_with('-') && t.trim_start_matches('-').contains('R'));
            let root_target = tokens[i + 1..].iter().any(|t| *t == "/");
            let is_777 = tokens[i + 1..].iter().any(|t| *t == "777");
            if recursive && root_target && (base == "chown" || is_777) {
                return Some("递归改权/改属根目录");
            }
        }
        // git push --force / -f 不拦（常见操作，审批模式兜底）
    }
    None
}

impl Tool for Bash {
    fn name(&self) -> &'static str {
        "Bash"
    }

    fn is_shell(&self) -> bool {
        true
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "Bash",
                "description": "执行 shell 命令并返回 stdout/stderr 与退出码。工作目录为工作区根。高风险命令会弹窗请用户确认。Windows 下优先用 Git Bash（Unix 语法），未安装时回退 cmd /C——以系统提示 env 块的 Shell 标注为准。注入 NO_COLOR=1 / TERM=dumb / GIT_TERMINAL_PROMPT=0（git 不会交互提问挂死）。timeout 默认 60s 最大 300s，超时自动转后台任务继续跑（输出不丢）；输出超 30KB 时完整内容落盘 .pigcode/tool-results/ 并返回头尾预览，累计超 16MiB 强制停止。长时命令（dev server/watch/长构建）也可用 run_in_background 直接后台运行。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "要执行的命令" },
                        "run_in_background": { "type": "boolean", "description": "true 时后台运行，立即返回 task_id（默认 false）" },
                        "timeout": { "type": "integer", "description": "超时秒数，默认 60，最大 300；超时后命令自动转入后台继续运行，不丢输出" }
                    },
                    "required": ["command"]
                }
            }
        })
    }

    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ToolEffect, String>> + Send + 'a>> {
        Box::pin(async move {
            let command = args["command"].as_str().ok_or("缺少参数 command")?;
            // 黑名单（is_dangerous_command）的拦截在会话层：命中 → 强制审批弹窗，
            // 用户 Allow 才走到这里；execute 层不再硬拒。
            if args["run_in_background"].as_bool().unwrap_or(false) {
                let task_id = crate::task::spawn_background(ctx.state, ctx.cwd, command);
                return Ok(ToolEffect::plain(format!(
                    "已在后台启动，task_id: {task_id}。用 TaskOutput 查看输出，TaskStop 停止。"
                )));
            }
            let secs = args["timeout"].as_u64().unwrap_or(60).clamp(1, 300);
            match crate::task::run_foreground(
                ctx.state,
                ctx.cwd,
                command,
                std::time::Duration::from_secs(secs),
            )
            .await
            {
                crate::task::ForegroundOutcome::SpawnFailed { error } => {
                    Err(format!("启动命令失败: {error}"))
                }
                crate::task::ForegroundOutcome::TimedOut { task_id } => {
                    Ok(ToolEffect::plain(format!(
                        "命令超过 {secs}s 未结束，已转入后台任务 {task_id}（输出持续保留）。用 TaskOutput 查看，TaskStop 停止。"
                    )))
                }
                crate::task::ForegroundOutcome::Completed {
                    output,
                    code,
                    spill_path,
                } => {
                    let mut text = output;
                    if text.chars().count() <= MAX_BASH_OUTPUT {
                        // 小输出不留痕：清掉 spill 文件
                        if let Some(path) = &spill_path {
                            let _ = std::fs::remove_file(path);
                        }
                        text.push_str(&format!("\n[exit code: {code}]"));
                    } else {
                        // 头尾预览 + 全量在 spill 文件（注册表 output 有 64KB 滚动上限，不作数）
                        let total = text.chars().count();
                        let head: String = text.chars().take(4096).collect();
                        let tail = crate::task::tail_chars(&text, 1024);
                        let spill_display = spill_path
                            .as_ref()
                            .map(|path| {
                                path.strip_prefix(ctx.cwd)
                                    .map(|relative| relative.to_string_lossy().replace('\\', "/"))
                                    .unwrap_or_else(|_| path.display().to_string())
                            })
                            .unwrap_or_else(|| "（未知路径）".to_string());
                        text = format!(
                            "{head}\n\n[...中间省略...]\n\n{tail}\n\n[输出过长（共 {total} 字符），完整输出已保存到 {spill_display}，可用 Read 分页查看]\n[exit code: {code}]"
                        );
                    }
                    Ok(ToolEffect::plain(text))
                }
            }
        })
    }
}
