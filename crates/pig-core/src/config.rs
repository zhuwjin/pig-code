use std::path::PathBuf;

use pig_protocol::{AppConfig, ModelConfig, ProviderConfig};
use serde::Deserialize;

/// 旧格式（M2-M7）：[provider] 单供应商
#[derive(Deserialize)]
struct LegacyConfig {
    provider: LegacyProvider,
}

#[derive(Deserialize)]
struct LegacyProvider {
    base_url: String,
    api_key: String,
    model: String,
    #[serde(default = "default_context_window")]
    context_window: u64,
    #[serde(default = "default_max_output_tokens")]
    max_output_tokens: u64,
}

fn default_context_window() -> u64 {
    128_000
}

fn default_max_output_tokens() -> u64 {
    8_192
}

pub fn default_path() -> PathBuf {
    crate::data_dir().join("config.toml")
}

/// 供应商 id 重复会让所有按 id 的查找命中第一个：模型解析落到错误供应商
/// 的兜底模型、会话 meta/label 张冠李戴。加载时大声提醒，别让人查半天。
fn warn_duplicate_provider_ids(config: &AppConfig) {
    let mut seen = std::collections::HashSet::new();
    for provider in &config.providers {
        if !seen.insert(&provider.id) {
            eprintln!(
                "[config] 警告：供应商 id 重复 \"{}\"（{} 与其他供应商共用，请在设置里删掉重建受影响的供应商）",
                provider.id, provider.name
            );
        }
    }
}

pub fn load(path: &std::path::Path) -> Result<AppConfig, String> {
    // 文件不存在 = 空配置（不算错误）；只有解析失败才报错
    if !path.exists() {
        return Ok(AppConfig::default());
    }
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("读取配置失败 {}: {e}", path.display()))?;
    // 新格式优先
    if let Ok(mut config) = toml::from_str::<AppConfig>(&raw) {
        if !config.providers.is_empty() {
            warn_duplicate_provider_ids(&config);
            expand_env_keys(&mut config);
            return Ok(config);
        }
    }
    // 旧格式迁移
    if let Ok(legacy) = toml::from_str::<LegacyConfig>(&raw) {
        let config = migrate(legacy.provider);
        let _ = save(path, &config);
        let mut config = config;
        expand_env_keys(&mut config);
        return Ok(config);
    }
    Err(format!(
        "解析配置失败 {}: 既不是新格式也不是旧格式",
        path.display()
    ))
}

fn migrate(legacy: LegacyProvider) -> AppConfig {
    let model_id = legacy.model.clone();
    AppConfig {
        providers: vec![ProviderConfig {
            id: "default".to_string(),
            name: "默认供应商".to_string(),
            base_url: legacy.base_url,
            api_key: legacy.api_key,
            api_format: pig_protocol::ApiFormat::OpenAiChat,
            enabled: true,
            models: vec![ModelConfig::new(
                &model_id,
                legacy.context_window,
                legacy.max_output_tokens,
            )],
        }],
        default_provider: "default".to_string(),
        default_model: model_id,
    }
}

fn expand_env_keys(config: &mut AppConfig) {
    for provider in &mut config.providers {
        provider.api_key = expand_env(&provider.api_key);
        provider.base_url = provider.base_url.trim_end_matches('/').to_string();
    }
}

pub fn expand_env(value: &str) -> String {
    let mut out = value.to_string();
    while let Some(start) = out.find("${") {
        let Some(end) = out[start..].find('}') else {
            break;
        };
        let var = out[start + 2..start + end].to_string();
        let replacement = std::env::var(&var).unwrap_or_default();
        out.replace_range(start..start + end + 1, &replacement);
    }
    out
}

pub fn save(path: &std::path::Path, config: &AppConfig) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let raw = toml::to_string_pretty(config).map_err(|e| format!("序列化配置失败: {e}"))?;
    std::fs::write(path, raw).map_err(|e| format!("写入配置失败 {}: {e}", path.display()))
}
