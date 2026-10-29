//! MCP tool naming: `mcp__<server>__<tool>`, charset [a-zA-Z0-9_-]; names over 64 chars are truncated and
//! given an 8-digit hash suffix to avoid collisions. Note sanitization is irreversible: distinct original names may sanitize to the same string (e.g. "a b"/"a_b").

use std::collections::HashSet;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Mutex, OnceLock};

/// Total length cap for combined names (aligned with mainstream function-calling tool name limits)
const MAX_TOOL_NAME: usize = 64;

/// Combined name `mcp__<server>__<tool>`; over-length triggers truncation + hash suffix (the hash uses the original names, maximizing distinctness)
pub fn tool_name(server: &str, tool: &str) -> String {
    let full = format!("mcp__{}__{}", sanitize(server), sanitize(tool));
    if full.len() <= MAX_TOOL_NAME {
        return full;
    }
    let mut hasher = DefaultHasher::new();
    (server, tool).hash(&mut hasher);
    let suffix = hasher.finish() as u32;
    let keep = MAX_TOOL_NAME - 9; // "_" + 8 hex digits; full is already sanitized to ASCII, so byte truncation is safe
    format!("{}_{suffix:08x}", &full[..keep])
}

/// Tool::name's signature needs &'static str: deduped by string then Box::leak; each distinct name leaks only once
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

/// Sanitize a single segment: illegal characters replaced with `_`; an empty segment falls back to "x"
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
        assert_eq!(tool_name("üö", "read"), "mcp______read");
    }

    #[test]
    fn long_name_truncated_with_hash() {
        let server = "s".repeat(40);
        let tool = "t".repeat(40);
        let name = tool_name(&server, &tool);
        assert!(name.len() <= MAX_TOOL_NAME, "{name} ({} chars)", name.len());
        assert!(name.starts_with("mcp__"));
        // Stable for the same name
        assert_eq!(name, tool_name(&server, &tool));
        // Distinct original names -> distinct suffixes
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
