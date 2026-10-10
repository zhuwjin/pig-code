use super::*;

/// WebSearch total output character budget (same tier as Grep/Glob)
const MAX_SEARCH_OUTPUT: usize = 30 * 1024;
const DEFAULT_MAX_RESULTS: u64 = 5;
const MAX_RESULTS_LIMIT: u64 = 10;

/// WebSearch: agent-level web search (distinct from the provider's server-side native search cap_web_search).
/// v1 connects directly to two fixed HTTPS search APIs (endpoints hardcoded, no SSRF surface): the environment variable
/// TAVILY_API_KEY takes priority (Tavily), otherwise BRAVE_API_KEY (Brave); with neither, an error with guidance is returned.
pub struct WebSearch;

enum SearchBackend {
    Tavily { api_key: String },
    Brave { api_key: String },
}

impl SearchBackend {
    fn detect() -> Result<Self, String> {
        if let Ok(key) = std::env::var("TAVILY_API_KEY")
            && !key.trim().is_empty()
        {
            return Ok(Self::Tavily { api_key: key });
        }
        if let Ok(key) = std::env::var("BRAVE_API_KEY")
            && !key.trim().is_empty()
        {
            return Ok(Self::Brave { api_key: key });
        }
        Err(
            "WebSearch is not configured: set the TAVILY_API_KEY (tavily.com) or \
             BRAVE_API_KEY (brave.com/search/api) environment variable, then restart the session."
                .to_string(),
        )
    }

    fn provider_name(&self) -> &'static str {
        match self {
            Self::Tavily { .. } => "Tavily",
            Self::Brave { .. } => "Brave",
        }
    }
}

/// One search result (the shape after normalizing both providers)
struct SearchHit {
    title: String,
    url: String,
    snippet: String,
}

/// Whether a search backend key is present. The registration gate: without
/// a key the tool could only ever return the configuration error, so it stays
/// out of the model's toolset entirely (the detect() error receipt path
/// remains for races with environment changes after registration).
pub(crate) fn backend_configured() -> bool {
    ["TAVILY_API_KEY", "BRAVE_API_KEY"]
        .iter()
        .any(|key| std::env::var(key).is_ok_and(|value| !value.trim().is_empty()))
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("pig-code WebSearch/0.1 (coding agent)")
        .build()
        .map_err(|e| e.to_string())
}

async fn search_tavily(api_key: &str, query: &str, max: u64) -> Result<Vec<SearchHit>, String> {
    let response = client()?
        .post("https://api.tavily.com/search")
        .json(&serde_json::json!({
            "api_key": api_key,
            "query": query,
            "max_results": max,
            "search_depth": "basic",
        }))
        .send()
        .await
        .map_err(|e| format!("Tavily request failed: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("Tavily returned {}", response.status()));
    }
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("Failed to parse Tavily response: {e}"))?;
    Ok(body["results"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|item| SearchHit {
                    title: item["title"].as_str().unwrap_or("").to_string(),
                    url: item["url"].as_str().unwrap_or("").to_string(),
                    snippet: item["content"].as_str().unwrap_or("").to_string(),
                })
                .collect()
        })
        .unwrap_or_default())
}

async fn search_brave(api_key: &str, query: &str, max: u64) -> Result<Vec<SearchHit>, String> {
    let response = client()?
        .get("https://api.search.brave.com/res/v1/web/search")
        .header("X-Subscription-Token", api_key)
        .header(reqwest::header::ACCEPT, "application/json")
        .query(&[("q", query), ("count", &max.to_string())])
        .send()
        .await
        .map_err(|e| format!("Brave request failed: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("Brave returned {}", response.status()));
    }
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("Failed to parse Brave response: {e}"))?;
    Ok(body["web"]["results"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|item| SearchHit {
                    title: item["title"].as_str().unwrap_or("").to_string(),
                    url: item["url"].as_str().unwrap_or("").to_string(),
                    snippet: item["description"].as_str().unwrap_or("").to_string(),
                })
                .collect()
        })
        .unwrap_or_default())
}

/// Numbered-list rendering + budget truncation (first come first served within the budget; the remaining count is annotated past it)
fn format_hits(provider: &str, query: &str, hits: &[SearchHit]) -> String {
    let mut output = format!(
        "Search results for \"{query}\" ({provider}, {} results)\n\n",
        hits.len()
    );
    for (ix, hit) in hits.iter().enumerate() {
        let entry = format!(
            "{}. {}\n   {}\n   {}\n\n",
            ix + 1,
            hit.title,
            hit.url,
            hit.snippet
        );
        if output.len() + entry.len() > MAX_SEARCH_OUTPUT {
            output.push_str(&format!(
                "[... {} more result(s) omitted over the output budget]",
                hits.len() - ix
            ));
            break;
        }
        output.push_str(&entry);
    }
    output
}

impl Tool for WebSearch {
    fn name(&self) -> &'static str {
        "WebSearch"
    }

    fn read_only(&self) -> bool {
        true
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "WebSearch",
                "description": "Search the web: returns a title/URL/snippet list (requires the TAVILY_API_KEY or BRAVE_API_KEY environment variable). Use it for up-to-date information, documentation, and error messages; follow up with FetchURL on a result URL to read the full page.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Search query" },
                        "max_results": {
                            "type": "integer",
                            "description": "Number of results to return (default 5, max 10)"
                        }
                    },
                    "required": ["query"]
                }
            }
        })
    }

    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        _ctx: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ToolEffect, String>> + Send + 'a>> {
        Box::pin(async move {
            let query = args["query"].as_str().unwrap_or("").trim();
            if query.is_empty() {
                return Err("WebSearch is missing the query parameter".to_string());
            }
            let max = args["max_results"]
                .as_u64()
                .unwrap_or(DEFAULT_MAX_RESULTS)
                .clamp(1, MAX_RESULTS_LIMIT);
            let backend = SearchBackend::detect()?;
            let hits = match &backend {
                SearchBackend::Tavily { api_key } => search_tavily(api_key, query, max).await?,
                SearchBackend::Brave { api_key } => search_brave(api_key, query, max).await?,
            };
            if hits.is_empty() {
                return Ok(ToolEffect::plain(format!(
                    "No results for \"{query}\" ({})",
                    backend.provider_name()
                )));
            }
            Ok(ToolEffect::plain(format_hits(
                backend.provider_name(),
                query,
                &hits,
            )))
        })
    }
}
