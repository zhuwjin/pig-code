//! Command-line network probe (triggered by PIG_NET_TEST=1 in pig-app, opens
//! no window): reads the app's real config and probes each enabled
//! provider's models for connectivity one by one (ping + a real streaming
//! request), printing the results, for network troubleshooting.

use pig_provider::{
    CallControl, ChatMsg, ProviderEvent, ResolvedModel, stream_chat, test_provider,
};

use crate::config;

pub fn net_test_blocking(config_path: &std::path::Path) {
    let config = match config::load(config_path) {
        Ok(config) => config,
        Err(e) => {
            println!(
                "[net-test] Failed to read config ({}): {e:?}",
                config_path.display()
            );
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
            let api_key = config::expand_env(&provider.api_key);
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

            // The exact same streaming path as sending a message
            let resolved = ResolvedModel {
                base_url: provider.base_url.clone(),
                api_key,
                model: model.id.clone(),
                context_window: model.context_window,
                max_output_tokens: model.max_output_tokens,
                api_format: provider.api_format,
                reasoning_params: None,
                cap_structured: model.cap_structured,
                cap_strict_tools: model.cap_strict_tools,
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
                CallControl::new(cancel),
            ));
            let mut outcome = "Finished".to_string();
            while let Ok(event) = rx.try_recv() {
                match event {
                    ProviderEvent::Failed(e) => outcome = format!("Failed: {e:?}"),
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
        println!("[net-test] no enabled provider model");
    }
}
