use super::*;

/// resolve_checked 的核心：纯路径解析 + 边界/符号链接检查，结构化返回越界形态。
fn resolve_core(cwd: &Path, path: &str, create_parents: bool) -> Result<PathBuf, BoundFailure> {
    let raw = Path::new(path);
    let full = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        cwd.join(raw)
    };
    let cwd_canonical = cwd
        .canonicalize()
        .map_err(|e| BoundFailure::Other(format!("工作目录无效 {}: {e}", cwd.display())))?;
    let parent = full.parent().unwrap_or(cwd);
    if create_parents {
        std::fs::create_dir_all(parent)
            .map_err(|e| BoundFailure::Other(format!("创建目录失败 {}: {e}", parent.display())))?;
    }
    let parent_canonical = parent
        .canonicalize()
        .map_err(|e| BoundFailure::Other(format!("目录不存在 {}: {e}", parent.display())))?;
    if !parent_canonical.starts_with(&cwd_canonical) {
        let file_name = full
            .file_name()
            .ok_or_else(|| BoundFailure::Other(format!("无效路径: {path}")))?;
        return Err(BoundFailure::ParentOutside {
            resolved: parent_canonical.join(file_name),
        });
    }
    let file_name = full
        .file_name()
        .ok_or_else(|| BoundFailure::Other(format!("无效路径: {path}")))?;
    let resolved = parent_canonical.join(file_name);
    let is_symlink = std::fs::symlink_metadata(&resolved)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);
    if is_symlink || resolved.exists() {
        match resolved.canonicalize() {
            Ok(full_canonical) => {
                if !full_canonical.starts_with(&cwd_canonical) {
                    return Err(BoundFailure::TargetOutside { resolved });
                }
            }
            Err(_) if is_symlink => return Err(BoundFailure::DanglingSymlink),
            Err(e) => {
                return Err(BoundFailure::Other(format!(
                    "路径无效 {}: {e}",
                    resolved.display()
                )));
            }
        }
    }
    Ok(resolved)
}

/// 解析相对 cwd 的路径并防止越出工作目录。
/// 父目录 canonicalize 之外，目标本身也要校验符号链接：指向工作区外的拒绝；
/// 悬空链接（目标不存在，无法 canonicalize）fail-closed 拒绝——否则 Write 会
/// 顺着链接在工作区外创建文件。
pub fn resolve_checked(cwd: &Path, path: &str, create_parents: bool) -> Result<PathBuf, String> {
    resolve_core(cwd, path, create_parents).map_err(|failure| match failure {
        BoundFailure::ParentOutside { .. } => format!("路径越出工作目录: {path}"),
        BoundFailure::TargetOutside { .. } => format!("路径越出工作目录（符号链接）: {path}"),
        BoundFailure::DanglingSymlink => {
            format!("符号链接指向不存在的目标，无法确认安全性，已拒绝: {path}")
        }
        BoundFailure::Other(message) => message,
    })
}

/// kimi writesOnlyPlanFile：Write/Edit 的目标路径是否落在计划目录
///（`.pigcode/plans/` 内的 `.md` 文件；提示词引导模型写 `plan-<session_id>.md`
/// 自然命中，判定不钉死文件名）。纯词法归一比较（不触碰文件系统——计划目录
/// 可能尚不存在，resolve_checked 的父目录 canonicalize 会失败）；相对/绝对
/// 路径都认，暂不支持经符号链接 cwd 的绝对路径比较（可接受边界）
pub fn is_plan_file_write(cwd: &Path, arguments: &str) -> bool {
    let args: serde_json::Value = serde_json::from_str(arguments).unwrap_or_default();
    let Some(path) = args["path"].as_str() else {
        return false;
    };
    let cwd_canonical = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let raw = Path::new(path);
    let full = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        cwd_canonical.join(raw)
    };
    // 判定放宽到 plans 目录内的 md 文件（不钉死文件名：kimi 的 slug 机制同理
    // 是「计划目录可写」，提示词引导的 plan-<sid>.md 自然命中）
    let plans_dir = cwd_canonical.join(".pigcode/plans");
    let full = lexical_normalize(&full);
    full.starts_with(lexical_normalize(&plans_dir))
        && full.extension().is_some_and(|ext| ext == "md")
}

/// 词法归一（`.` 去掉、`..` 弹一级；不解符号链接不查文件系统）
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// 区外读/写策略：Read 看 fs_read_outside 开关，Write 看 fs_write_outside
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FsAccess {
    Read,
    Write,
}

/// 路径是否落在系统 tmp 下（macOS /tmp→/private/tmp，两边都 canonical 后比）。
/// 只对**绝对路径**请求生效（相对 ../ 逃逸落在 tmp 也视为越界，避免 tempdir
/// 工作区旁的目录成为后门）。
fn is_tmp_absolute_path(raw: &str) -> bool {
    let raw_path = Path::new(raw);
    if !raw_path.is_absolute() {
        return false;
    }
    // canonical 比较：请求路径可能不存在（Write 新文件），退化为父目录 canonical
    let candidate = raw_path
        .canonicalize()
        .or_else(|_| raw_path.parent().unwrap_or(raw_path).canonicalize())
        .unwrap_or_else(|_| raw_path.to_path_buf());
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(tmp) = std::env::temp_dir().canonicalize() {
        roots.push(tmp);
    }
    if let Ok(tmp) = Path::new("/tmp").canonicalize() {
        roots.push(tmp);
    }
    roots.iter().any(|root| candidate.starts_with(root))
}

/// resolve_checked 的策略入口（文件工具统一走这里）：工作区内与显式 tmp 绝对路径
/// 永远放行；其余区外按会话开关；开关关时报错带模式菜单引导。悬空链接始终拒绝。
/// 敏感文件检查（is_sensitive_file）在 resolve 之后由工具无条件执行，不受开关影响。
pub fn resolve_with_access(
    state: &SessionToolState,
    cwd: &Path,
    path: &str,
    create_parents: bool,
    access: FsAccess,
) -> Result<PathBuf, String> {
    match resolve_core(cwd, path, create_parents) {
        Ok(resolved) => Ok(resolved),
        Err(BoundFailure::Other(message)) => Err(message),
        Err(BoundFailure::DanglingSymlink) => Err(format!(
            "符号链接指向不存在的目标，无法确认安全性，已拒绝: {path}"
        )),
        Err(
            BoundFailure::ParentOutside { resolved } | BoundFailure::TargetOutside { resolved },
        ) => {
            if is_tmp_absolute_path(path) {
                return Ok(resolved);
            }
            // 额外只读根（data_dir/sessions：子代理 result.md/上下文记录）始终可读——
            // 与 tmp 并列的豁免、同口径只对绝对路径请求生效（相对 ../ 逃逸不算）；
            // 只放 Read，Write 不放。resolved 已是 resolve_core 判定后的路径
            //（父目录 canonical + 文件名），直接与构造期 canonical 化的 root 比前缀。
            // 敏感文件检查（is_sensitive_file）在 resolve 之后由工具照旧执行、不受
            // 豁免影响；config.toml 在 data_dir 根部不在 sessions/ 下，天然排除。
            if access == FsAccess::Read
                && Path::new(path).is_absolute()
                && state
                    .extra_read_roots
                    .iter()
                    .any(|root| resolved.starts_with(root))
            {
                return Ok(resolved);
            }
            let allowed = match access {
                FsAccess::Read => state.fs_read_outside.load(Ordering::Relaxed),
                FsAccess::Write => state.fs_write_outside.load(Ordering::Relaxed),
            };
            if allowed {
                // 同一个闸：开关开时指向区外的 symlink 也放行
                Ok(resolved)
            } else {
                let toggle = match access {
                    FsAccess::Read => "允许读取工作区外文件",
                    FsAccess::Write => "允许写入工作区外文件",
                };
                Err(format!(
                    "路径越出工作目录: {path}（可在输入框模式菜单开启「{toggle}」）"
                ))
            }
        }
    }
}

/// 敏感文件判定（大小写不敏感，只看文件名）：.env 家族 / SSH 私钥 / 云凭据。
pub fn is_sensitive_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    // .env 与 .env.*（模板类豁免）
    if (lower == ".env" || lower.starts_with(".env."))
        && !matches!(
            lower.as_str(),
            ".env.example" | ".env.sample" | ".env.template"
        )
    {
        return true;
    }
    // SSH 私钥：精确名或 id_xxx[-_.] 变体；.pub 公钥一律豁免
    if !lower.ends_with(".pub") {
        for prefix in ["id_rsa", "id_ed25519", "id_ecdsa", "id_dsa"] {
            if lower == prefix {
                return true;
            }
            if let Some(rest) = lower.strip_prefix(prefix)
                && rest
                    .chars()
                    .next()
                    .is_some_and(|c| matches!(c, '-' | '_' | '.'))
            {
                return true;
            }
        }
    }
    // 云凭据：~/.aws/credentials、~/.gcp/credentials
    if lower == "credentials" {
        let parent = path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(str::to_ascii_lowercase);
        if parent.as_deref() == Some(".aws") || parent.as_deref() == Some(".gcp") {
            return true;
        }
    }
    false
}

/// 敏感文件拒绝文案（Read/Write/Edit 共用）
pub(crate) fn sensitive_file_error(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string());
    format!("已拒绝访问敏感文件: {name}（.env / 私钥 / 云凭据不会进入模型上下文）")
}
