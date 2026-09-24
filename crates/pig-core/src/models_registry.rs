//! models.dev 模型元数据注册表：api.json 拉取、磁盘缓存、按模型 ID 查询。
//! 索引按「裸模型 ID」（去掉 provider 前缀）建；同名 ID 多供应商时官方
//! 数据优先（聚合商的镜像常带私货字段）。缓存存索引化后的精简格式，
//! 而非 5MB 原文——加载不需要重新解析全文。

use std::collections::HashMap;
use std::path::Path;

use pig_protocol::ModelRegistryInfo;

pub const API_URL: &str = "https://models.dev/api.json?type=all";

/// 官方（第一方）供应商：同 ID 多方收录时优先取它们的数据。
/// models.dev 收录 200+ 供应商，其中大量是聚合商，同一模型 ID 重复出现。
const OFFICIAL_PROVIDERS: &[&str] = &[
    "openai",
    "anthropic",
    "google",
    "deepseek",
    "alibaba",
    "moonshot",
    "zhipu",
    "xai",
    "minimax",
    "mistral",
    "meta",
    "cohere",
    "amazon",
    "microsoft",
    "perplexity",
    "nvidia",
];

/// 解析 api.json 原文 → 裸模型 ID 索引。坏 JSON 返回空表（调用方按未命中走重拉）。
pub fn parse_api_json(raw: &str) -> HashMap<String, ModelRegistryInfo> {
    let Ok(root) = serde_json::from_str::<serde_json::Value>(raw) else {
        return HashMap::new();
    };
    let Some(providers) = root.as_object() else {
        return HashMap::new();
    };
    // 两轮遍历：官方供应商先入表占位，聚合商不覆盖
    let mut index = HashMap::new();
    for round in [true, false] {
        for (pid, provider) in providers {
            let official = OFFICIAL_PROVIDERS.contains(&pid.as_str());
            if official != round {
                continue;
            }
            let Some(models) = provider.get("models").and_then(|m| m.as_object()) else {
                continue;
            };
            for (mid, model) in models {
                // key 形如 "claude-opus-4-5" 或 "deepseek/deepseek-v4-flash"，取末段作查询键
                let bare = mid.rsplit('/').next().unwrap_or(mid.as_str());
                if !round && index.contains_key(bare) {
                    continue;
                }
                let entry = parse_entry(pid, mid, model);
                index.entry(bare.to_string()).or_insert(entry);
            }
        }
    }
    index
}

fn parse_entry(pid: &str, mid: &str, model: &serde_json::Value) -> ModelRegistryInfo {
    let limit = model.get("limit");
    let num = |field: &str| limit.and_then(|l| l.get(field)).and_then(|v| v.as_u64());
    let reasoning = model
        .get("reasoning")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    // reasoning_options: [{type:"toggle"}, {type:"effort",values:[...]}, {type:"budget_tokens"}]
    // 只有 effort 类型带离散等级
    let reasoning_levels = model
        .get("reasoning_options")
        .and_then(|v| v.as_array())
        .map(|options| {
            options
                .iter()
                .filter(|o| o.get("type").and_then(|t| t.as_str()) == Some("effort"))
                .filter_map(|o| o.get("values").and_then(|v| v.as_array()))
                .flatten()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    // modalities.input（如 ["text","image"]）；字段缺失 → 空（UI 据此不动能力勾选）
    let input_modalities = model
        .get("modalities")
        .and_then(|m| m.get("input"))
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    // structured_output 字段约 30% 模型缺失，None = 数据源未给
    let structured_output = model.get("structured_output").and_then(|v| v.as_bool());
    ModelRegistryInfo {
        full_id: format!("{pid}/{mid}"),
        context: num("context"),
        input: num("input"),
        output: num("output"),
        reasoning,
        reasoning_levels,
        input_modalities,
        structured_output,
    }
}

/// 缓存格式版本：字段增删时 +1，旧格式视为无缓存（立即重拉而不是等节流过期）
const CACHE_SCHEMA: u64 = 3;

/// 磁盘缓存：索引 + 拉取时间戳（节流用）。损坏/缺失/版本不符 → None。
pub fn load_cache(cache_path: &Path) -> Option<(u64, HashMap<String, ModelRegistryInfo>)> {
    let raw = std::fs::read_to_string(cache_path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    if value.get("schema").and_then(|v| v.as_u64()) != Some(CACHE_SCHEMA) {
        return None;
    }
    let fetched_at = value.get("fetched_at").and_then(|v| v.as_u64())?;
    let models = value.get("models")?.as_object()?;
    let index = models
        .iter()
        .filter_map(|(id, v)| {
            let info: ModelRegistryInfo = serde_json::from_value(v.clone()).ok()?;
            Some((id.clone(), info))
        })
        .collect();
    Some((fetched_at, index))
}

pub fn save_cache(cache_path: &Path, fetched_at: u64, index: &HashMap<String, ModelRegistryInfo>) {
    let value = serde_json::json!({
        "schema": CACHE_SCHEMA,
        "fetched_at": fetched_at,
        "models": index,
    });
    if let Some(dir) = cache_path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(raw) = serde_json::to_vec(&value) {
        let _ = std::fs::write(cache_path, raw);
    }
}

/// 拉取 api.json 并解析成索引（不落盘——写盘由调用方决定时机）。
/// 4.9MB 响应走慢网络常见瞬断，失败重试一次。
pub async fn fetch_index() -> Result<HashMap<String, ModelRegistryInfo>, String> {
    let fetch = || async {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(15))
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .map_err(|e| format!("构建 HTTP 客户端失败: {e}"))?;
        let response = client
            .get(API_URL)
            .send()
            .await
            .map_err(|e| format!("请求 models.dev 失败: {e}"))?;
        let raw = response
            .text()
            .await
            .map_err(|e| format!("读取 models.dev 响应失败: {e}"))?;
        Ok::<_, String>(parse_api_json(&raw))
    };
    match fetch().await {
        Ok(index) => Ok(index),
        Err(first) => match fetch().await {
            Ok(index) => Ok(index),
            Err(second) => Err(format!("{first}; 重试: {second}")),
        },
    }
}

/// 当前 unix 秒（缓存时间戳/节流用）
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_limits_reasoning_and_strips_provider_prefix() {
        let raw = r#"{
            "someaggregator": {
                "models": {
                    "deepseek/deepseek-v4-flash": {
                        "reasoning": true,
                        "reasoning_options": [
                            {"type": "toggle"},
                            {"type": "effort", "values": ["low", "high", "max"]},
                            {"type": "budget_tokens", "min": 1024}
                        ],
                        "limit": {"context": 1000000, "output": 384000},
                        "modalities": {"input": ["text"], "output": ["text"]}
                    },
                    "gpt-4o": {
                        "reasoning": false,
                        "structured_output": true,
                        "limit": {"context": 128000, "input": 127000, "output": 16384},
                        "modalities": {"input": ["text", "image"], "output": ["text"]}
                    }
                }
            }
        }"#;
        let index = parse_api_json(raw);
        let flash = index.get("deepseek-v4-flash").expect("裸 ID 可查");
        assert_eq!(
            flash.reasoning_levels,
            vec!["low".to_string(), "high".to_string(), "max".to_string()]
        );
        assert_eq!(flash.context, Some(1_000_000));
        assert_eq!(flash.output, Some(384_000));
        assert_eq!(flash.full_id, "someaggregator/deepseek/deepseek-v4-flash");
        assert!(flash.input.is_none());
        // 纯文本输入：不含 image/pdf
        assert_eq!(flash.input_modalities, vec!["text".to_string()]);

        let gpt = index.get("gpt-4o").unwrap();
        assert!(!gpt.reasoning);
        assert_eq!(gpt.input, Some(127_000));
        assert!(gpt.reasoning_levels.is_empty());
        assert!(gpt.input_modalities.contains(&"image".to_string()));
        assert_eq!(gpt.structured_output, Some(true));
        // deepseek-v4-flash 未给 structured_output → None
        assert_eq!(flash.structured_output, None);
    }

    #[test]
    fn missing_modalities_field_leaves_list_empty() {
        let raw = r#"{"p": {"models": {"m": {"reasoning": false}}}}"#;
        let index = parse_api_json(raw);
        assert!(index["m"].input_modalities.is_empty());
    }

    #[test]
    fn official_provider_wins_over_aggregator_on_same_bare_id() {
        let raw = r#"{
            "someaggregator": {
                "models": {"m1": {"reasoning": false, "limit": {"context": 1}}}
            },
            "openai": {
                "models": {"m1": {"reasoning": true, "limit": {"context": 2, "output": 3}}}
            }
        }"#;
        let index = parse_api_json(raw);
        let m1 = index.get("m1").unwrap();
        assert_eq!(m1.full_id, "openai/m1");
        assert_eq!(m1.context, Some(2));
    }

    #[test]
    fn bad_json_yields_empty_index() {
        assert!(parse_api_json("not json").is_empty());
        assert!(parse_api_json("{}").is_empty());
    }

    #[test]
    fn cache_roundtrip() {
        let dir = std::env::temp_dir().join(format!("pig-registry-{}", std::process::id()));
        let path = dir.join("cache.json");
        let mut index = HashMap::new();
        index.insert(
            "m1".to_string(),
            ModelRegistryInfo {
                full_id: "openai/m1".into(),
                context: Some(128_000),
                input: None,
                output: Some(16_384),
                reasoning: true,
                reasoning_levels: vec!["low".into(), "high".into()],
                input_modalities: vec!["text".into(), "image".into()],
                structured_output: Some(true),
            },
        );
        save_cache(&path, 42, &index);
        let (fetched_at, loaded) = load_cache(&path).expect("缓存可读");
        assert_eq!(fetched_at, 42);
        assert_eq!(loaded, index);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
