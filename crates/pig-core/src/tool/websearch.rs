use super::*;

/// WebSearch 输出总字符预算（与 Grep/Glob 同级）
const MAX_SEARCH_OUTPUT: usize = 30 * 1024;
const DEFAULT_MAX_RESULTS: u64 = 5;
const MAX_RESULTS_LIMIT: u64 = 10;

/// WebSearch：agent 级网络搜索（区别于 provider 服务端原生搜索 cap_web_search）。
/// v1 直连两个固定 HTTPS 搜索 API（端点写死，无 SSRF 面）：环境变量
/// TAVILY_API_KEY 优先（Tavily），否则 BRAVE_API_KEY（Brave）；都没有则报错引导。
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
            "WebSearch 未配置搜索服务：请设置环境变量 TAVILY_API_KEY（tavily.com）或 \
             BRAVE_API_KEY（brave.com/search/api）后重启会话。"
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

/// 一条搜索结果（两 provider 归一化后的形状）
struct SearchHit {
    title: String,
    url: String,
    snippet: String,
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
        .map_err(|e| format!("Tavily 请求失败: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("Tavily 返回 {}", response.status()));
    }
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("Tavily 响应解析失败: {e}"))?;
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
        .map_err(|e| format!("Brave 请求失败: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("Brave 返回 {}", response.status()));
    }
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("Brave 响应解析失败: {e}"))?;
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

/// 编号列表渲染 + 预算截断（预算内先到先得，超了标注剩余条数）
fn format_hits(provider: &str, query: &str, hits: &[SearchHit]) -> String {
    let mut output = format!("「{query}」的搜索结果（{provider}，{} 条）\n\n", hits.len());
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
                "[... 剩余 {} 条超出输出预算已省略]",
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
                "description": "联网搜索：返回标题/URL/摘要列表（需要环境变量 TAVILY_API_KEY 或 BRAVE_API_KEY）。用于查最新资讯、文档、报错信息；拿到 URL 后用 FetchURL 读全文。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "搜索关键词" },
                        "max_results": {
                            "type": "integer",
                            "description": "返回条数（默认 5，上限 10）"
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
                return Err("WebSearch 缺少参数 query".to_string());
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
                    "「{query}」未搜到结果（{}）",
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
