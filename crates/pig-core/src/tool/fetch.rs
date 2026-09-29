use super::*;

const MAX_FETCH_BODY: usize = 2 * 1024 * 1024;
const MAX_FETCH_OUTPUT: usize = 50000;

/// 数值 IP 判定：未指定/环回/私网/链路本地/文档段/CGNAT/benchmark/组播。
/// FetchURL 的字面 host 与 DNS 解析结果共用。
pub fn is_private_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let b = v4.octets();
            v4.is_unspecified()
                || v4.is_loopback()
                || v4.is_private() // 10/8、172.16/12、192.168/16
                || v4.is_link_local() // 169.254/16
                || v4.is_documentation()
                || (b[0] == 100 && (64..=127).contains(&b[1])) // 100.64.0.0/10 CGNAT
                || (b[0] == 198 && (b[1] == 18 || b[1] == 19)) // 198.18.0.0/15 benchmark
                || v4.is_multicast()
        }
        std::net::IpAddr::V6(v6) => {
            let b = v6.octets();
            v6.is_unspecified()
                || v6.is_loopback()
                || (b[0] & 0xfe) == 0xfc // fc00::/7 unique local
                || (b[0] == 0xfe && (b[1] & 0xc0) == 0x80) // fe80::/10 link-local
                || v6.is_multicast()
        }
    }
}

/// SSRF 防护：IP 字面量走数值判定（见 is_private_ip）；
/// 域名拒绝 localhost 家族（含 *.localhost）与单段主机名（内网短名）。
/// DNS 解析出的地址由 is_private_ip 逐跳校验。
pub fn is_private_host(host: &str) -> bool {
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return is_private_ip(&ip);
    }
    host == "localhost" || host.ends_with(".localhost") || !host.contains('.')
}

/// 从 HTML 提取正文：剔除 script/style/noscript/svg/template，优先 main/article
/// 否则 body；块级元素之间换行，行内空白压缩，连续空行折叠。
pub fn extract_text(html: &str) -> String {
    use scraper::{Html, Selector};
    const SKIP: &[&str] = &["script", "style", "noscript", "svg", "template"];
    const BLOCK: &[&str] = &[
        "address",
        "article",
        "aside",
        "blockquote",
        "br",
        "dd",
        "details",
        "div",
        "dl",
        "dt",
        "fieldset",
        "figcaption",
        "figure",
        "footer",
        "form",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "header",
        "hr",
        "li",
        "main",
        "nav",
        "ol",
        "p",
        "pre",
        "section",
        "table",
        "td",
        "th",
        "tr",
        "ul",
    ];
    let document = Html::parse_document(html);
    let mut root = None;
    for name in ["main", "article", "body"] {
        let selector = Selector::parse(name).expect("合法选择器");
        if let Some(el) = document.select(&selector).next() {
            root = Some(el);
            break;
        }
    }
    let Some(root) = root else {
        return String::new();
    };
    // 栈遍历（None = 块级元素闭合，补换行）；(*root) 解引用到 NodeRef 以遍历文本节点
    let mut out = String::new();
    let mut stack: Vec<Option<_>> = (*root)
        .children()
        .map(Some)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    while let Some(item) = stack.pop() {
        let Some(node) = item else {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            continue;
        };
        match node.value() {
            scraper::Node::Text(text) => {
                // 标签之间的纯空白是排版噪音，丢弃（行内空白后续统一压缩）
                if !text.text.trim().is_empty() {
                    out.push_str(&text.text);
                }
            }
            scraper::Node::Element(el) => {
                let name = el.name();
                if SKIP.contains(&name) {
                    continue;
                }
                let block = BLOCK.contains(&name);
                if block && !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                if block {
                    stack.push(None);
                }
                stack.extend(
                    node.children()
                        .map(Some)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev(),
                );
            }
            _ => {}
        }
    }
    let mut lines: Vec<String> = Vec::new();
    for line in out.lines() {
        let collapsed = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if collapsed.is_empty() && lines.last().is_none_or(|l| l.is_empty()) {
            continue;
        }
        lines.push(collapsed);
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

pub(crate) struct FetchUrl;

impl Tool for FetchUrl {
    fn name(&self) -> &'static str {
        "FetchURL"
    }

    fn read_only(&self) -> bool {
        true
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "FetchURL",
                "description": "抓取公开网页并提取正文（HTML 自动清洗为纯文本，JSON/纯文本原样返回）。不支持需要登录的页面。URL 不允许内嵌凭据，域名会先做 DNS 私网校验。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "url": { "type": "string", "description": "要抓取的 http/https URL" }
                    },
                    "required": ["url"]
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
            let url = args["url"].as_str().ok_or("缺少参数 url")?;
            let mut current = reqwest::Url::parse(url).map_err(|e| format!("URL 无效: {e}"))?;
            // 手动跟随重定向：每跳都完整重做 凭据/字面 IP/DNS 校验并钉死解析结果
            let mut hops = 0;
            let mut response = loop {
                let client = pinned_client(&current).await?;
                let response = client
                    .get(current.clone())
                    .send()
                    .await
                    .map_err(|e| format!("请求失败: {e}"))?;
                if !response.status().is_redirection() {
                    break response;
                }
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok());
                let Some(location) = location else {
                    break response; // 3xx 无 Location：当终态（HTTP 状态检查会拦下）
                };
                hops += 1;
                if hops > 5 {
                    return Err("重定向次数过多".to_string());
                }
                current = current
                    .join(location)
                    .map_err(|e| format!("重定向 URL 无效: {e}"))?;
            };
            let status = response.status();
            if !status.is_success() {
                return Err(format!("HTTP {status}"));
            }
            let content_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|s| {
                    s.split(';')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_ascii_lowercase()
                })
                .unwrap_or_default();
            // 流式读体，上限 2MB
            let mut body: Vec<u8> = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|e| format!("读取响应失败: {e}"))?
            {
                let remaining = MAX_FETCH_BODY.saturating_sub(body.len());
                if chunk.len() > remaining {
                    body.extend_from_slice(&chunk[..remaining]);
                    break;
                }
                body.extend_from_slice(&chunk);
            }
            let text = String::from_utf8_lossy(&body).to_string();
            let mut out = match content_type.as_str() {
                "text/html" => extract_text(&text),
                "text/plain" | "text/markdown" | "application/json" => text,
                "" => return Err("响应缺少 Content-Type，无法判定内容类型".to_string()),
                other => return Err(format!("不支持的内容类型: {other}")),
            };
            if out.is_empty() {
                out = "（页面无可提取文本）".to_string();
            }
            if out.chars().count() > MAX_FETCH_OUTPUT {
                out = out.chars().take(MAX_FETCH_OUTPUT).collect();
                out.push_str("\n\n（已截断，仅显示前 50000 字符）");
            }
            Ok(ToolEffect::plain(out))
        })
    }
}

/// FetchURL 的 URL 静态校验：scheme 白名单 + 内嵌凭据拒绝 + 字面 host 私网判定
///（域名的 DNS 校验在 pinned_client 里做）。
pub fn check_fetch_url(url: &reqwest::Url) -> Result<(), String> {
    match url.scheme() {
        "http" | "https" => {}
        scheme => return Err(format!("仅支持 http/https URL（收到 {scheme}:）")),
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("URL 不允许内嵌凭据".to_string());
    }
    let host = url.host_str().ok_or("URL 缺少主机名")?;
    if is_private_host(host) {
        return Err(format!("不允许访问本机/私网地址: {host}"));
    }
    Ok(())
}

/// 每跳新建 pinned client：静态校验后，域名先解析（spawn_blocking + ToSocketAddrs），
/// 所有结果逐个过 is_private_ip，任一私网即拒绝；全公网则 resolve_to_addrs 钉死，
/// 防 check-to-connect 之间的 DNS rebinding。
/// 注意：使用系统代理时代理自行解析 DNS，钉生不对代理生效，属已知取舍。
async fn pinned_client(url: &reqwest::Url) -> Result<reqwest::Client, String> {
    use std::net::ToSocketAddrs as _;
    check_fetch_url(url)?;
    let host = url.host_str().expect("check_fetch_url 已校验");
    let mut builder = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("pig-code FetchURL/0.1 (coding agent)")
        .redirect(reqwest::redirect::Policy::none()); // 重定向手动跟随，每跳重验
    let is_ip_literal = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<std::net::IpAddr>()
        .is_ok();
    if !is_ip_literal {
        let port = url
            .port_or_known_default()
            .unwrap_or(if url.scheme() == "https" { 443 } else { 80 });
        let host_owned = host.to_string();
        let resolved = tokio::task::spawn_blocking(move || {
            (host_owned.as_str(), port)
                .to_socket_addrs()
                .map(|addrs| addrs.collect::<Vec<_>>())
        })
        .await
        .map_err(|e| format!("DNS 解析失败: {host}: {e}"))?
        .map_err(|_| format!("DNS 解析失败: {host}"))?;
        if resolved.is_empty() {
            return Err(format!("DNS 解析失败: {host}"));
        }
        if resolved.iter().any(|addr| is_private_ip(&addr.ip())) {
            return Err(format!("域名解析到私网/保留地址，已拒绝: {host}"));
        }
        builder = builder.resolve_to_addrs(host, &resolved);
    }
    builder.build().map_err(|e| e.to_string())
}
