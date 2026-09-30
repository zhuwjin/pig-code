//! MCP 工具命名：`mcp__<server>__<tool>`，字符集 [a-zA-Z0-9_-]；超 64 字符截断并
//! 带 8 位 hash 后缀防碰撞。注意清洗不可逆：不同原名可能清洗成同串（如 "a b"/"a_b"）。

use std::collections::HashSet;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Mutex, OnceLock};

/// 组合名总长上限（对齐主流 function-calling 工具名限制）
const MAX_TOOL_NAME: usize = 64;

/// 组名 `mcp__<server>__<tool>`；超长截断 + hash 后缀（hash 用原始名，最大化区分度）
pub fn tool_name(server: &str, tool: &str) -> String {
    let full = format!("mcp__{}__{}", sanitize(server), sanitize(tool));
    if full.len() <= MAX_TOOL_NAME {
        return full;
    }
    let mut hasher = DefaultHasher::new();
    (server, tool).hash(&mut hasher);
    let suffix = hasher.finish() as u32;
    let keep = MAX_TOOL_NAME - 9; // "_" + 8 位 hex；full 已清洗为 ASCII，字节截断安全
    format!("{}_{suffix:08x}", &full[..keep])
}

/// Tool::name 签名要 &'static str：同串去重后 Box::leak，每种名字只泄漏一次
pub fn intern(name: String) -> &'static str {
    static NAMES: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let names = NAMES.get_or_init(|| Mutex::new(HashSet::new()));
    let mut guard = names.lock().expect("mcp name intern lock");
    if let Some(&existing) = guard.get(name.as_str()) {
        return existing;
    }
    let leaked: &'static str = Box::leak(name.into_boxed_str());
    guard.insert(leaked);
    leaked
}

/// 单段清洗：非法字符替换为 `_`；空段兜底 "x"
fn sanitize(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "x".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_replaces_illegal_chars() {
        assert_eq!(
            tool_name("my server", "read file!"),
            "mcp__my_server__read_file_"
        );
        assert_eq!(tool_name("fs", "read"), "mcp__fs__read");
        assert_eq!(tool_name("", ""), "mcp__x__x");
        assert_eq!(tool_name("中文", "read"), "mcp______read");
    }

    #[test]
    fn long_name_truncated_with_hash() {
        let server = "s".repeat(40);
        let tool = "t".repeat(40);
        let name = tool_name(&server, &tool);
        assert!(name.len() <= MAX_TOOL_NAME, "{name} ({} chars)", name.len());
        assert!(name.starts_with("mcp__"));
        // 同名稳定
        assert_eq!(name, tool_name(&server, &tool));
        // 不同原名 → 不同后缀
        let other = tool_name(&server, &format!("{}x", "t".repeat(39)));
        assert_ne!(name, other);
        assert!(other.len() <= MAX_TOOL_NAME);
    }

    #[test]
    fn intern_dedupes() {
        let a = intern("mcp__fs__read".to_string());
        let b = intern("mcp__fs__read".to_string());
        assert!(std::ptr::eq(a, b));
    }
}
