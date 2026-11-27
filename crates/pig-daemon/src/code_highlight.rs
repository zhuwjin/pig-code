use std::{ops::Range, sync::OnceLock};

use serde::Serialize;
use syntect::{
    parsing::{ParseState, ScopeStack, SyntaxSet},
    util::LinesWithEndings,
};

static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct HighlightSpan {
    pub start: usize,
    pub end: usize,
    pub token: &'static str,
}

fn token_from_scope(scope: &str) -> Option<&'static str> {
    if scope.starts_with("comment") {
        return Some("comment");
    }
    if scope.starts_with("string") {
        return Some("string");
    }
    if scope.starts_with("keyword") || scope.starts_with("storage") {
        return Some("keyword");
    }
    if scope.starts_with("constant.numeric")
        || scope.starts_with("constant.language")
        || scope.starts_with("constant.character")
    {
        return Some("number");
    }
    if scope.starts_with("entity.name")
        || scope.starts_with("markup.heading")
        || scope.starts_with("meta.function")
    {
        return Some("title");
    }
    if scope.starts_with("entity.other.attribute-name")
        || scope.starts_with("variable.parameter")
        || scope.starts_with("support.type.property-name")
    {
        return Some("attr");
    }
    if scope.starts_with("support.function")
        || scope.starts_with("support.type")
        || scope.starts_with("support.class")
        || scope.starts_with("support.constant")
    {
        return Some("built");
    }
    if scope.starts_with("markup.deleted") || scope.starts_with("invalid") {
        return Some("deletion");
    }
    None
}

pub fn highlight_spans(code: &str, language: &str) -> Vec<HighlightSpan> {
    let file_name = language.rsplit(['/', '\\']).next().unwrap_or(language);
    let language = match file_name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() && !extension.is_empty() => extension,
        _ => file_name,
    };
    let lower = language.trim().to_ascii_lowercase();
    if ["", "text", "txt", "plain", "plaintext"].contains(&lower.as_str()) {
        return Vec::new();
    }
    let alias = match lower.as_str() {
        "shell" | "console" | "shellscript" => "sh",
        "golang" => "go",
        "jsx" => "tsx",
        "mjs" | "cjs" => "js",
        "c++" => "cpp",
        "c#" | "csharp" => "cs",
        "f#" | "fsharp" => "fs",
        "jsonc" | "json5" => "json",
        "objective-c" | "objc" => "m",
        "docker" => "Dockerfile",
        "make" => "Makefile",
        "vue" | "svelte" => "html",
        _ => lower.as_str(),
    };
    let set = SYNTAXES.get_or_init(two_face::syntax::extra_newlines);
    let Some(syntax) = set
        .find_syntax_by_token(alias)
        .filter(|syntax| syntax.name != "Plain Text")
    else {
        return Vec::new();
    };
    let mut parser = ParseState::new(syntax);
    let mut scopes = ScopeStack::new();
    let mut spans = Vec::new();
    let mut offset = 0;
    for line in LinesWithEndings::from(code) {
        if let Ok(operations) = parser.parse_line(line, set) {
            let mut cursor = 0;
            for (index, operation) in operations {
                push_span(&mut spans, code, offset + cursor..offset + index, &scopes);
                let _ = scopes.apply(&operation);
                cursor = index;
            }
            push_span(
                &mut spans,
                code,
                offset + cursor..offset + line.len(),
                &scopes,
            );
        }
        offset += line.len();
    }
    spans
}

fn push_span(spans: &mut Vec<HighlightSpan>, code: &str, range: Range<usize>, scopes: &ScopeStack) {
    if range.is_empty() {
        return;
    }
    let Some(token) = scopes
        .as_slice()
        .iter()
        .rev()
        .find_map(|scope| token_from_scope(&scope.build_string()))
    else {
        return;
    };
    let start_utf16 = code[..range.start].encode_utf16().count();
    let end_utf16 = code[..range.end].encode_utf16().count();
    if let Some(last) = spans.last_mut()
        && last.end == start_utf16
        && last.token == token
    {
        last.end = end_utf16;
        return;
    }
    spans.push(HighlightSpan {
        start: start_utf16,
        end: end_utf16,
        token,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlights_file_names_and_plain_text() {
        assert!(!highlight_spans("# Title\nlet x = 42;", "docs/PLUGIN_SYSTEM.md").is_empty());
        assert!(!highlight_spans("fn main() { let x = 42; }", "main.rs").is_empty());
        assert!(highlight_spans("plain output", "text").is_empty());
        assert!(highlight_spans("plain output", "unknown-language").is_empty());
    }

    #[test]
    fn reports_utf16_offsets_for_non_ascii_code() {
        let code = "let 内容 = \"你好\";\n// 注释";
        let spans = highlight_spans(code, "main.rs");
        assert!(!spans.is_empty());
        for span in &spans {
            assert!(span.start < span.end);
            assert!(span.end <= code.encode_utf16().count());
        }
        let comment = spans.iter().find(|span| span.token == "comment").unwrap();
        let start = code.find("// 注释").unwrap();
        assert_eq!(comment.start, code[..start].encode_utf16().count());
        assert_eq!(comment.end, code.encode_utf16().count());
    }
}
