//! models.dev model-metadata registry: api.json fetching, disk cache, lookup by model ID.
//! The index is keyed by "bare model ID" (provider prefix stripped); when the same ID appears
//! across providers, official data wins (aggregator mirrors often carry extra fields). The cache
//! stores the compact indexed form rather than the 5MB raw text — loading needs no full re-parse.

use std::collections::HashMap;
use std::path::Path;

use pig_protocol::ModelRegistryInfo;

pub const API_URL: &str = "https://models.dev/api.json?type=all";

/// Official (first-party) providers: when the same ID is listed by multiple providers, their data wins.
/// models.dev lists 200+ providers, many of them aggregators, so the same model ID appears repeatedly.
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

/// Parse raw api.json -> bare-model-ID index. Bad JSON returns an empty map (the caller treats it as a miss and refetches).
pub fn parse_api_json(raw: &str) -> HashMap<String, ModelRegistryInfo> {
    let Ok(root) = serde_json::from_str::<serde_json::Value>(raw) else {
        return HashMap::new();
    };
    let Some(providers) = root.as_object() else {
        return HashMap::new();
    };
    // Two passes: official providers enter the table first; aggregators do not overwrite
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
                // Keys look like "claude-opus-4-5" or "deepseek/deepseek-v4-flash"; take the last segment as the lookup key
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
    // only the effort type carries discrete levels
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
    // modalities.input (e.g. ["text","image"]); missing field -> empty (the UI leaves capability checkboxes untouched based on this)
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
    // structured_output is missing for ~30% of models; None = the data source did not provide it
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

/// Cache format version: bump on field changes; older formats count as no cache (immediate refetch instead of waiting for throttle expiry)
const CACHE_SCHEMA: u64 = 3;

/// Disk cache: index + fetch timestamp (for throttling). Corrupt/missing/version-mismatch -> None.
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

/// Fetch api.json and parse it into an index (not persisted — the caller decides when to write).
/// The 4.9MB response commonly hits transient drops on slow networks; retry once on failure.
pub async fn fetch_index() -> Result<HashMap<String, ModelRegistryInfo>, String> {
    let fetch = || async {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(15))
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .map_err(|e| format!("Failed to build HTTP client: {e}"))?;
        let response = client
            .get(API_URL)
            .send()
            .await
            .map_err(|e| format!("Failed to request models.dev: {e}"))?;
        let raw = response
            .text()
            .await
            .map_err(|e| format!("Failed to read models.dev response: {e}"))?;
        Ok::<_, String>(parse_api_json(&raw))
    };
    match fetch().await {
        Ok(index) => Ok(index),
        Err(first) => match fetch().await {
            Ok(index) => Ok(index),
            Err(second) => Err(format!("{first}; retry: {second}")),
        },
    }
}

/// Current unix seconds (for cache timestamps/throttling)
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
        let flash = index
            .get("deepseek-v4-flash")
            .expect("queryable by bare ID");
        assert_eq!(
            flash.reasoning_levels,
            vec!["low".to_string(), "high".to_string(), "max".to_string()]
        );
        assert_eq!(flash.context, Some(1_000_000));
        assert_eq!(flash.output, Some(384_000));
        assert_eq!(flash.full_id, "someaggregator/deepseek/deepseek-v4-flash");
        assert!(flash.input.is_none());
        // Text-only input: no image/pdf
        assert_eq!(flash.input_modalities, vec!["text".to_string()]);

        let gpt = index.get("gpt-4o").unwrap();
        assert!(!gpt.reasoning);
        assert_eq!(gpt.input, Some(127_000));
        assert!(gpt.reasoning_levels.is_empty());
        assert!(gpt.input_modalities.contains(&"image".to_string()));
        assert_eq!(gpt.structured_output, Some(true));
        // deepseek-v4-flash has no structured_output -> None
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
        let (fetched_at, loaded) = load_cache(&path).expect("cache readable");
        assert_eq!(fetched_at, 42);
        assert_eq!(loaded, index);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
