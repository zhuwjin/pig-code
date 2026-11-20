use std::sync::OnceLock;

use super::*;

const MAX_FETCH_BODY: usize = 2 * 1024 * 1024;
const MAX_FETCH_OUTPUT: usize = 50000;

/// Cloud instance metadata / link-local IP check. Even when local/LAN fetches are
/// permitted for developer workflows, cloud metadata services are never legitimate
/// coding-agent targets and must be blocked to prevent SSRF credential theft.
pub fn is_metadata_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let b = v4.octets();
            v4.is_link_local() // 169.254.0.0/16 (AWS/GCP/Azure IMDS, ECS 169.254.170.2)
                || b == [100, 100, 100, 200] // Alibaba Cloud metadata
        }
        std::net::IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_metadata_ip(&std::net::IpAddr::V4(v4));
            }
            let b = v6.octets();
            // fe80::/10 link-local or fd00:ec2::254 (AWS IPv6 IMDS)
            (b[0] == 0xfe && (b[1] & 0xc0) == 0x80)
                || v6.segments() == [0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254]
        }
    }
}

/// Static host check for cloud metadata endpoints.
pub fn is_metadata_host(host: &str) -> bool {
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return is_metadata_ip(&ip);
    }
    matches!(
        host.as_str(),
        "metadata.google.internal"
            | "metadata.goog"
            | "metadata.azure.internal"
            | "metadata.tencentyun.com"
            | "instance-data"
            | "instance-data.ec2.internal"
    )
}

fn fetch_client() -> Result<reqwest::Client, String> {
    static CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .pool_idle_timeout(std::time::Duration::from_secs(60))
                .user_agent("pig-code FetchURL/0.1 (coding agent)")
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|e| e.to_string())
        })
        .clone()
}

async fn check_resolved_metadata(url: &reqwest::Url) -> Result<(), String> {
    let Some(host) = url.host_str() else {
        return Err("URL is missing a host".to_string());
    };
    let port = url.port_or_known_default().unwrap_or(80);
    let clean_host = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = clean_host.parse::<std::net::IpAddr>() {
        if is_metadata_ip(&ip) {
            return Err(format!(
                "Blocked SSRF target: {ip} is a link-local or cloud metadata address"
            ));
        }
        return Ok(());
    }
    if let Ok(addrs) = tokio::net::lookup_host((clean_host, port)).await {
        for addr in addrs {
            if is_metadata_ip(&addr.ip()) {
                return Err(format!(
                    "Blocked SSRF target: {clean_host} resolved to link-local or cloud metadata address {}",
                    addr.ip()
                ));
            }
        }
    }
    Ok(())
}

/// Numeric IP check: unspecified/loopback/private/link-local/documentation/CGNAT/benchmark/multicast.
/// Shared by FetchURL's literal host and DNS resolution results.
pub fn is_private_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let b = v4.octets();
            v4.is_unspecified()
                || v4.is_loopback()
                || v4.is_private() // 10/8, 172.16/12, 192.168/16
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

/// SSRF protection: IP literals go through the numeric check (see is_private_ip);
/// domains reject the localhost family (including *.localhost) and single-label host names (intranet short names).
/// Addresses from DNS resolution are checked hop by hop by is_private_ip.
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

/// Extract the main text from HTML: drop script/style/noscript/svg/template, prefer main/article,
/// otherwise body; newlines between block elements, inline whitespace collapsed, consecutive blank lines folded.
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
        let selector = Selector::parse(name).expect("valid selector");
        if let Some(el) = document.select(&selector).next() {
            root = Some(el);
            break;
        }
    }
    let Some(root) = root else {
        return String::new();
    };
    // Stack traversal (None = a block element closed, append a newline); (*root) dereferences to NodeRef to walk text nodes
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
                // Pure whitespace between tags is typographic noise; drop it (inline whitespace is collapsed uniformly later)
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
                "description": "Fetch a web page and extract its main text (HTML is cleaned to plain text; JSON/plain text is returned as-is; non-UTF-8 pages such as GBK/Big5/Shift-JIS are decoded per their charset). Supports http and https — local/LAN addresses (localhost, 192.168.x.x, etc.) are allowed over http. Pages that require login are not supported, and URLs must not embed credentials.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "url": { "type": "string", "description": "The http/https URL to fetch" }
                    },
                    "required": ["url"]
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
            if ctx.sandbox_profile != crate::task::SandboxProfile::Unrestricted
                && crate::task::network_blocked_for(ctx.state)
            {
                return Err(
                    "Network access is blocked by the sandbox policy; FetchURL cannot make outbound requests."
                        .to_string(),
                );
            }
            let url = args["url"]
                .as_str()
                .ok_or("Missing required parameter: url")?;
            let mut current = reqwest::Url::parse(url).map_err(|e| format!("Invalid URL: {e}"))?;
            check_fetch_url(&current)?;
            check_resolved_metadata(&current).await?;
            // A local desktop tool allows localhost/LAN http, while blocking link-local / cloud
            // metadata targets, non-http schemes, and embedded credentials across every redirect hop.
            let client = fetch_client()?;
            let mut hops = 0;
            let mut response = loop {
                let response = client
                    .get(current.clone())
                    .send()
                    .await
                    .map_err(|e| format!("Request failed: {e}"))?;
                if !response.status().is_redirection() {
                    break response;
                }
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok());
                let Some(location) = location else {
                    break response; // 3xx without Location: treat as terminal (the HTTP status check will catch it)
                };
                hops += 1;
                if hops > 10 {
                    return Err("Too many redirects".to_string());
                }
                current = current
                    .join(location)
                    .map_err(|e| format!("Invalid redirect URL: {e}"))?;
                check_fetch_url(&current)?;
                check_resolved_metadata(&current).await?;
            };
            let status = response.status();
            if !status.is_success() {
                return Err(format!("HTTP {status}"));
            }
            // Keep the full Content-Type (including the charset parameter) and the mime separately
            let content_type_full = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_ascii_lowercase();
            let content_type = content_type_full
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            let charset_param = content_type_full
                .split(';')
                .skip(1)
                .find_map(|part| part.trim().strip_prefix("charset=").map(str::to_string));
            // Stream the body with a 2MB cap
            let mut body: Vec<u8> = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|e| format!("Failed to read response: {e}"))?
            {
                let remaining = MAX_FETCH_BODY.saturating_sub(body.len());
                if chunk.len() > remaining {
                    body.extend_from_slice(&chunk[..remaining]);
                    break;
                }
                body.extend_from_slice(&chunk);
            }
            let is_html = content_type == "text/html";
            let text = decode_body(&body, charset_param.as_deref(), is_html);
            let mut out = match content_type.as_str() {
                "text/html" => extract_text(&text),
                "text/plain" | "text/markdown" | "application/json" => text,
                "" => {
                    return Err(
                        "Response has no Content-Type; cannot determine the content type"
                            .to_string(),
                    );
                }
                other => return Err(format!("Unsupported content type: {other}")),
            };
            if out.is_empty() {
                out = "(no extractable text on the page)".to_string();
            }
            if out.chars().count() > MAX_FETCH_OUTPUT {
                out = out.chars().take(MAX_FETCH_OUTPUT).collect();
                out.push_str("\n\n(Truncated; showing the first 50000 characters only)");
            }
            Ok(ToolEffect::plain(out))
        })
    }
}

/// Static URL validation for FetchURL: scheme allowlist + embedded-credential rejection.
/// The local desktop tool allows private-network access (localhost/LAN http is a hard requirement; see the is_private_ip/host
/// helpers — the checks are kept for callers to reuse as needed, no longer a FetchURL blocking condition).
pub fn check_fetch_url(url: &reqwest::Url) -> Result<(), String> {
    match url.scheme() {
        "http" | "https" => {}
        scheme => {
            return Err(format!(
                "Only http/https URLs are supported (got {scheme}:)"
            ));
        }
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("URLs must not embed credentials".to_string());
    }
    let host = url.host_str().ok_or("URL is missing a host")?;
    if is_metadata_host(host) {
        return Err(format!(
            "Blocked SSRF target: {host} is a link-local or cloud metadata address"
        ));
    }
    Ok(())
}

/// Response body decoding: charset priority = Content-Type parameter, HTML meta sniffing (first 2KB),
/// lossy UTF-8 fallback; encoding_rs covers labels like GBK/GB18030/Big5/Shift-JIS/EUC-KR,
/// and BOM is handled by the corresponding Encoding::decode. A leading BOM character is stripped along the way.
fn decode_body(body: &[u8], charset_param: Option<&str>, is_html: bool) -> String {
    let label = charset_param.map(str::to_string).or_else(|| {
        if is_html {
            sniff_html_charset(body)
        } else {
            None
        }
    });
    let text = match label
        .as_deref()
        .and_then(|l| encoding_rs::Encoding::for_label(l.as_bytes()))
    {
        Some(encoding) => {
            let (text, _, _) = encoding.decode(body);
            text.into_owned()
        }
        None => String::from_utf8_lossy(body).into_owned(),
    };
    text.strip_prefix('\u{feff}').unwrap_or(&text).to_string()
}

/// HTML head meta charset sniffing: find the first `charset` occurrence and take the label word after it
/// (covers both <meta charset="gbk"> and the charset= form inside http-equiv content).
fn sniff_html_charset(body: &[u8]) -> Option<String> {
    let head = String::from_utf8_lossy(&body[..body.len().min(2048)]);
    let lower = head.to_ascii_lowercase();
    let rest = lower.split("charset").nth(1)?;
    let rest = rest.trim_start_matches([' ', '=', '"', '\'']);
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .unwrap_or(rest.len());
    let label = &rest[..end];
    (!label.is_empty()).then(|| label.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_fetch_url_blocks_cloud_metadata_and_link_local() {
        for blocked in [
            "http://169.254.169.254/latest/meta-data/",
            "http://169.254.170.2/v2/credentials",
            "http://100.100.100.200/latest/meta-data/",
            "http://[fe80::1]/",
            "http://[fd00:ec2::254]/latest/meta-data/",
            "http://[::ffff:169.254.169.254]/",
            "http://metadata.google.internal/computeMetadata/v1/",
            "http://metadata.goog/",
        ] {
            let url = reqwest::Url::parse(blocked).expect("valid url");
            let err = check_fetch_url(&url).expect_err(&format!("{blocked} must be blocked"));
            assert!(err.contains("SSRF"), "unexpected error for {blocked}: {err}");
        }
    }

    #[tokio::test]
    async fn fetch_url_respects_sandbox_network_block() {
        let dir = std::env::temp_dir();
        let mut tracker = ChangeTracker::default();
        let state = SessionToolState::for_test();
        state
            .sandbox_project
            .write()
            .unwrap()
            .network_block = Some(true);
        let tool = FetchUrl;
        let result = tool
            .execute(
                serde_json::json!({ "url": "http://127.0.0.1:9/" }),
                ToolContext {
                    cwd: &dir,
                    tracker: &mut tracker,
                    state: &state,
                    fs_grant: None,
                    sandbox_override: Default::default(),
                    sandbox_profile: crate::task::SandboxProfile::ReadOnly,
                    additional_permissions: Default::default(),
                },
            )
            .await;
        let Err(err) = result else {
            panic!("FetchURL must fail when sandbox network is blocked");
        };
        assert!(err.contains("Network access is blocked"), "{err}");
    }
}
