use super::replay::{AssistantReplay, AssistantReplayItem};

/// Maximum normalized sources kept per assistant step.
const MAX_REPLAY_SOURCES: usize = 32;
const MAX_SOURCE_URL_CHARS: usize = 2048;
const MAX_SOURCE_TITLE_CHARS: usize = 300;
const MAX_SOURCE_SNIPPET_CHARS: usize = 400;

/// Derive the normalized UI sources from a persisted native replay. This is a
/// pure projection: it accepts only http/https URLs, preserves first-seen
/// order, fills missing title/snippet on duplicates, and never copies opaque
/// encrypted fields. Compatible endpoints keep their native replay even when
/// only the text is replayed on the wire, so the UI still sees sources.
pub fn replay_sources(replay: &AssistantReplay) -> Vec<pig_protocol::Source> {
    let mut sources: Vec<pig_protocol::Source> = Vec::new();
    for item in &replay.items {
        match item {
            AssistantReplayItem::Text { citations, .. } => {
                for citation in citations {
                    push_source(
                        &mut sources,
                        &citation.url,
                        &citation.title,
                        citation
                            .snippet
                            .as_deref()
                            .or(citation.cited_text.as_deref()),
                        citation.start_index,
                        citation.end_index,
                    );
                }
            }
            AssistantReplayItem::ResponsesWebSearchCall { action, .. } => {
                for source in action
                    .get("sources")
                    .and_then(|sources| sources.as_array())
                    .into_iter()
                    .flatten()
                {
                    push_source(
                        &mut sources,
                        source["url"].as_str().unwrap_or_default(),
                        source["title"].as_str().unwrap_or_default(),
                        source["snippet"].as_str(),
                        None,
                        None,
                    );
                }
            }
            AssistantReplayItem::AnthropicWebSearchResult { content, .. } => {
                for result in content.as_array().into_iter().flatten() {
                    if result["type"] != "web_search_result" {
                        continue;
                    }
                    push_source(
                        &mut sources,
                        result["url"].as_str().unwrap_or_default(),
                        result["title"].as_str().unwrap_or_default(),
                        result["snippet"].as_str(),
                        None,
                        None,
                    );
                }
            }
            _ => {}
        }
    }
    sources
}

fn push_source(
    sources: &mut Vec<pig_protocol::Source>,
    url: &str,
    title: &str,
    snippet: Option<&str>,
    start_index: Option<u32>,
    end_index: Option<u32>,
) {
    let url = url.trim();
    // A longer URL is dropped whole: truncating it would make the UI open a
    // different (and likely invalid) address.
    if url.chars().count() > MAX_SOURCE_URL_CHARS || !is_http_url(url) {
        return;
    }
    let title = truncate_chars(title.trim(), MAX_SOURCE_TITLE_CHARS);
    let snippet = snippet
        .map(str::trim)
        .filter(|snippet| !snippet.is_empty())
        .map(|snippet| truncate_chars(snippet, MAX_SOURCE_SNIPPET_CHARS));
    if let Some(existing) = sources.iter_mut().find(|source| source.url == url) {
        if existing.title.is_empty() && !title.is_empty() {
            existing.title = title;
        }
        if existing.snippet.is_none() {
            existing.snippet = snippet;
        }
        if existing.start_index.is_none() {
            existing.start_index = start_index;
        }
        if existing.end_index.is_none() {
            existing.end_index = end_index;
        }
        return;
    }
    if sources.len() >= MAX_REPLAY_SOURCES {
        return;
    }
    sources.push(pig_protocol::Source {
        url: url.to_string(),
        title,
        snippet,
        start_index,
        end_index,
    });
}

fn is_http_url(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    text.chars().take(max_chars).collect()
}
