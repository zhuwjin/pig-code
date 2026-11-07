use std::{ops::Range, sync::OnceLock};

use nsh_base::editor::EditorToken;
use nsh_core::prelude::*;
use serde::Serialize;
use syntect::{
    parsing::{ParseState, ScopeStack, SyntaxSet},
    util::LinesWithEndings,
};

static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();

#[derive(Debug, PartialEq, Eq, Serialize)]
struct HighlightSpan {
    start: usize,
    end: usize,
    token: &'static str,
}

/// Read-only syntax highlighting for the virtual code view.
#[class]
pub struct CodeHighlighter {}

#[class]
impl CodeHighlighter {
    /// Creates a stateless highlighter.
    pub fn new() -> Self {
        Self {}
    }

    /// Returns JSON token ranges in UTF-16 code units, computed off the UI thread.
    pub async fn highlight(
        this: Instance<Self>,
        code: String,
        language: String,
    ) -> Result<String, String> {
        let _ = &this;
        tokio::task::spawn_blocking(move || highlight_json(&code, &language))
            .await
            .map_err(|error| error.to_string())?
    }
}

fn highlight_json(code: &str, language: &str) -> Result<String, String> {
    serde_json::to_string(&highlight_spans(code, language)).map_err(|error| error.to_string())
}

fn highlight_spans(code: &str, language: &str) -> Vec<HighlightSpan> {
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
        .find_map(|scope| EditorToken::from_scope(&scope.build_string()))
    else {
        return;
    };
    let token = match token {
        EditorToken::Comment => "comment",
        EditorToken::Keyword => "keyword",
        EditorToken::String => "string",
        EditorToken::Number => "number",
        EditorToken::Title => "title",
        EditorToken::Attr => "attr",
        EditorToken::Built => "built",
        EditorToken::Deletion => "deletion",
    };
    if let Some(last) = spans.last_mut()
        && last.end == code[..range.start].encode_utf16().count()
        && last.token == token
    {
        last.end = code[..range.end].encode_utf16().count();
        return;
    }
    spans.push(HighlightSpan {
        start: code[..range.start].encode_utf16().count(),
        end: code[..range.end].encode_utf16().count(),
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

    #[test]
    fn json_keeps_the_native_contract() {
        let value: serde_json::Value =
            serde_json::from_str(&highlight_json("let x = 42;", "rust").unwrap()).unwrap();
        assert!(value.is_array());
        assert!(value[0].get("start").is_some());
        assert!(value[0].get("end").is_some());
        assert!(value[0].get("token").is_some());
    }
}
