use super::*;

#[derive(Deserialize)]
pub(crate) struct CompleteResponse {
    choices: Option<Vec<CompleteChoice>>,
    content: Option<Vec<CompleteBlock>>,
}

#[derive(Deserialize)]
pub(crate) struct CompleteChoice {
    message: Option<CompleteMessage>,
}

#[derive(Deserialize)]
pub(crate) struct CompleteMessage {
    content: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct CompleteBlock {
    text: Option<String>,
}

/// 非流式一次性请求（compaction 摘要）。
pub async fn complete_text(
    config: &ResolvedModel,
    user_content: String,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<String, String> {
    let client = http_client();
    match config.api_format {
        ApiFormat::OpenAiChat => {
            let body = serde_json::json!({
                "model": config.model,
                "messages": [{"role": "user", "content": user_content}],
                "stream": false,
                "max_tokens": config.max_output_tokens,
            });
            let response = tokio::select! {
                result = client
                    .post(format!("{}/chat/completions", config.base_url))
                    .bearer_auth(&config.api_key)
                    .json(&body)
                    .send() => result.map_err(net_err)?,
                _ = cancel.cancelled() => return Err("已取消".to_string()),
            };
            let status = response.status();
            if !status.is_success() {
                let detail = response.text().await.unwrap_or_default();
                let detail: String = detail.chars().take(300).collect();
                return Err(format!("HTTP {status}: {detail}"));
            }
            let parsed: CompleteResponse = response
                .json()
                .await
                .map_err(|e| format!("解析响应失败: {e}"))?;
            parsed
                .choices
                .and_then(|mut c| c.pop())
                .and_then(|c| c.message)
                .and_then(|m| m.content)
                .filter(|content| !content.is_empty())
                .ok_or_else(|| "响应无内容".to_string())
        }
        ApiFormat::AnthropicMessages => {
            let body = serde_json::json!({
                "model": config.model,
                "max_tokens": config.max_output_tokens,
                "stream": false,
                "messages": [{"role": "user", "content": user_content}],
            });
            let response = tokio::select! {
                result = client
                    .post(anthropic_url(&config.base_url))
                    .header("x-api-key", &config.api_key)
                    .header("anthropic-version", "2023-06-01")
                    .json(&body)
                    .send() => result.map_err(net_err)?,
                _ = cancel.cancelled() => return Err("已取消".to_string()),
            };
            let status = response.status();
            if !status.is_success() {
                let detail = response.text().await.unwrap_or_default();
                let detail: String = detail.chars().take(300).collect();
                return Err(format!("HTTP {status}: {detail}"));
            }
            let parsed: CompleteResponse = response
                .json()
                .await
                .map_err(|e| format!("解析响应失败: {e}"))?;
            parsed
                .content
                .and_then(|blocks| blocks.into_iter().find_map(|b| b.text))
                .filter(|text| !text.is_empty())
                .ok_or_else(|| "响应无内容".to_string())
        }
    }
}

/// 连通性测试：最小请求，2xx 即通过。
pub async fn test_provider(
    base_url: &str,
    api_key: &str,
    format: ApiFormat,
    model: &str,
) -> Result<String, String> {
    let client = http_client();
    let send = async {
        match format {
            ApiFormat::OpenAiChat => {
                client
                    .post(format!(
                        "{}/chat/completions",
                        base_url.trim_end_matches('/')
                    ))
                    .bearer_auth(api_key)
                    .json(&serde_json::json!({
                        "model": model,
                        "messages": [{"role": "user", "content": "ping"}],
                        "max_tokens": 1,
                        "stream": false,
                    }))
                    .send()
                    .await
            }
            ApiFormat::AnthropicMessages => {
                client
                    .post(anthropic_url(base_url.trim_end_matches('/')))
                    .header("x-api-key", api_key)
                    .header("anthropic-version", "2023-06-01")
                    .json(&serde_json::json!({
                        "model": model,
                        "max_tokens": 1,
                        "messages": [{"role": "user", "content": "ping"}],
                        "stream": false,
                    }))
                    .send()
                    .await
            }
        }
    };
    let response = tokio::time::timeout(std::time::Duration::from_secs(10), send)
        .await
        .map_err(|_| "连接超时（10s）".to_string())?
        .map_err(net_err)?;
    let status = response.status();
    if status.is_success() {
        Ok(format!("连接成功（HTTP {status}）"))
    } else {
        let detail = response.text().await.unwrap_or_default();
        let detail: String = detail.chars().take(200).collect();
        Err(format!("HTTP {status}: {detail}"))
    }
}

/// 命令行网络探针（pig-app 的 PIG_NET_TEST=1 触发，不开窗口）：
/// 读取应用真实配置，逐个测试已启用供应商的模型连通性（ping + 真实流式请求），
/// 打印结果，用于网络排障。
pub fn net_test_blocking(config_path: &std::path::Path) {
    let config = match crate::config::load(config_path) {
        Ok(config) => config,
        Err(e) => {
            println!("[net-test] 读取配置失败（{}）: {e}", config_path.display());
            return;
        }
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let mut tested = 0;
    for provider in config.providers.iter().filter(|p| p.enabled) {
        for model in &provider.models {
            tested += 1;
            let api_key = crate::config::expand_env(&provider.api_key);
            let result = rt.block_on(test_provider(
                &provider.base_url,
                &api_key,
                provider.api_format,
                &model.id,
            ));
            println!(
                "[net-test] ping {}/{} ({}) → {result:?}",
                provider.name, model.id, provider.base_url
            );

            // 与发送消息完全相同的流式路径
            let resolved = ResolvedModel {
                base_url: provider.base_url.clone(),
                api_key,
                model: model.id.clone(),
                context_window: model.context_window,
                max_output_tokens: model.max_output_tokens,
                api_format: provider.api_format,
                reasoning_params: None,
                cap_web_search: model.cap_web_search,
                web_search_tool: model.web_search_tool.clone(),
                input_image: model.input_image,
                provider_name: provider.name.clone(),
            };
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let cancel = tokio_util::sync::CancellationToken::new();
            rt.block_on(stream_chat(
                resolved,
                vec![ChatMsg::user("ping".to_string())],
                Vec::new(),
                tx,
                cancel,
            ));
            let mut outcome = "Finished".to_string();
            while let Ok(event) = rx.try_recv() {
                match event {
                    ProviderEvent::Failed(e) => outcome = format!("Failed: {e}"),
                    ProviderEvent::ToolCalls(_) => outcome = "ToolCalls".to_string(),
                    _ => {}
                }
            }
            println!(
                "[net-test] stream {}/{} → {outcome}",
                provider.name, model.id
            );
        }
    }
    if tested == 0 {
        println!("[net-test] 没有已启用的供应商模型");
    }
}
