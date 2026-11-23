//! Model-agnostic client-side Deferred Tools (`ToolSearch`).
//!
//! When a session exposes many runtime tools (MCP servers and plugins), sending
//! every JSON schema on every turn wastes context tokens. Once the runtime tool
//! count exceeds [`DEFAULT_DEFERRED_TOOL_THRESHOLD`], non-activated runtime
//! tools are withheld from the request `tools` array and discoverable through
//! the built-in read-only `ToolSearch` tool.
//!
//! Activated tool names are stored append-only so newly loaded tools appear at
//! the tail of the `tools` list without reshuffling previously cached entries.
//! If a model calls a known deferred tool directly without searching first, the
//! execution path still resolves and auto-activates it through the normal
//! permission gate.

use std::collections::HashSet;

use super::*;

pub const TOOL_SEARCH_TOOL: &str = "ToolSearch";
/// Runtime MCP/plugin tool count above which deferred loading engages.
pub const DEFAULT_DEFERRED_TOOL_THRESHOLD: usize = 16;
const DEFAULT_MAX_RESULTS: usize = 8;
const HARD_MAX_RESULTS: usize = 20;
const MAX_CATALOG_NAMES_IN_DESCRIPTION: usize = 40;

#[derive(Clone, Debug)]
pub struct DeferredToolEntry {
    pub name: String,
    pub description: String,
    pub parameters: Vec<String>,
    pub schema: serde_json::Value,
}

impl DeferredToolEntry {
    pub fn from_tool(tool: &dyn Tool) -> Self {
        let schema = schema_with_strict(tool);
        let description = schema["function"]["description"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let mut parameters: Vec<String> = schema["function"]["parameters"]["properties"]
            .as_object()
            .map(|props| props.keys().cloned().collect())
            .unwrap_or_default();
        parameters.sort();
        Self {
            name: tool.name().to_string(),
            description,
            parameters,
            schema,
        }
    }
}

/// Session-scoped registry tracking deferred runtime tools and the append-only
/// set of activated tool names.
#[derive(Clone, Debug)]
pub struct DeferredToolRegistry {
    threshold: usize,
    catalog: Vec<DeferredToolEntry>,
    /// Append-only ordered list of activated runtime tool names (prefix-cache
    /// friendly: newly activated tools are appended at the tail).
    activated: Vec<String>,
}

impl Default for DeferredToolRegistry {
    fn default() -> Self {
        Self::with_threshold(DEFAULT_DEFERRED_TOOL_THRESHOLD)
    }
}

impl DeferredToolRegistry {
    pub fn with_threshold(threshold: usize) -> Self {
        Self {
            threshold,
            catalog: Vec::new(),
            activated: Vec::new(),
        }
    }

    /// Replace the current runtime-tool catalog (MCP + filtered plugins).
    pub fn sync_catalog(&mut self, entries: Vec<DeferredToolEntry>) {
        self.catalog = entries;
        // Drop activated names that no longer exist in the live runtime set
        // while keeping relative order of surviving activated names intact.
        let valid: HashSet<&str> = self.catalog.iter().map(|e| e.name.as_str()).collect();
        self.activated.retain(|name| valid.contains(name.as_str()));
    }

    /// True when runtime tool count exceeds the deferral threshold.
    pub fn is_deferral_active(&self) -> bool {
        self.catalog.len() > self.threshold
    }

    /// Activate one runtime tool by exact name (append-only). Returns `true`
    /// if the tool exists in the catalog and was newly activated.
    pub fn activate(&mut self, name: &str) -> bool {
        if !self.catalog.iter().any(|entry| entry.name == name) {
            return false;
        }
        if self.activated.iter().any(|existing| existing == name) {
            return false;
        }
        self.activated.push(name.to_string());
        true
    }

    /// Activated runtime tool schemas in stable append-only order.
    pub fn activated_schemas(&self) -> Vec<serde_json::Value> {
        self.activated
            .iter()
            .filter_map(|name| {
                self.catalog
                    .iter()
                    .find(|entry| entry.name == *name)
                    .map(|entry| entry.schema.clone())
            })
            .collect()
    }

    pub fn activated_names(&self) -> &[String] {
        &self.activated
    }

    pub fn total_catalog_len(&self) -> usize {
        self.catalog.len()
    }

    /// Search deferred runtime tools by keyword query or `select:a,b` exact
    /// selector, activating every returned match.
    pub fn search_and_activate(
        &mut self,
        query: &str,
        max_results: usize,
    ) -> Vec<DeferredToolEntry> {
        let limit = max_results.clamp(1, HARD_MAX_RESULTS);
        let trimmed = query.trim();
        let matched: Vec<DeferredToolEntry> = if let Some(selection) = trimmed
            .strip_prefix("select:")
            .or_else(|| trimmed.strip_prefix("SELECT:"))
        {
            let wanted: Vec<&str> = selection
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            let mut out = Vec::new();
            for name in wanted {
                if let Some(entry) = self
                    .catalog
                    .iter()
                    .find(|e| e.name.eq_ignore_ascii_case(name))
                    && !out.iter().any(|e: &DeferredToolEntry| e.name == entry.name)
                {
                    out.push(entry.clone());
                }
                if out.len() >= limit {
                    break;
                }
            }
            out
        } else {
            let tokens: Vec<String> = trimmed
                .to_lowercase()
                .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_' && ch != '-')
                .filter(|tok| !tok.is_empty())
                .map(str::to_string)
                .collect();
            let query_lower = trimmed.to_lowercase();

            let mut scored: Vec<(u32, &DeferredToolEntry)> = self
                .catalog
                .iter()
                .filter_map(|entry| {
                    let score = score_entry(entry, &query_lower, &tokens);
                    (score > 0 || tokens.is_empty()).then_some((score, entry))
                })
                .collect();
            scored.sort_by(|(score_a, a), (score_b, b)| {
                score_b.cmp(score_a).then_with(|| a.name.cmp(&b.name))
            });
            scored
                .into_iter()
                .take(limit)
                .map(|(_, entry)| entry.clone())
                .collect()
        };

        for entry in &matched {
            self.activate(&entry.name);
        }
        matched
    }

    /// Compact summary embedded in the `ToolSearch` tool description when
    /// deferral is active so the model knows which deferred tools exist.
    pub fn catalog_summary(&self) -> String {
        if self.catalog.is_empty() {
            return String::new();
        }
        let mut names: Vec<&str> = self.catalog.iter().map(|e| e.name.as_str()).collect();
        names.sort_unstable();
        let total = names.len();
        if total <= MAX_CATALOG_NAMES_IN_DESCRIPTION {
            format!("Available deferred tools ({total}): {}", names.join(", "))
        } else {
            let shown = &names[..MAX_CATALOG_NAMES_IN_DESCRIPTION];
            format!(
                "Available deferred tools ({total}, showing first {MAX_CATALOG_NAMES_IN_DESCRIPTION}): {}, ... (+{} more; search by keyword)",
                shown.join(", "),
                total - MAX_CATALOG_NAMES_IN_DESCRIPTION
            )
        }
    }

    /// Build the `ToolSearch` wire schema augmented with the live deferred
    /// tool catalog summary.
    pub fn tool_search_schema(&self) -> serde_json::Value {
        let mut schema = ToolSearchTool.schema();
        let summary = self.catalog_summary();
        if !summary.is_empty()
            && let Some(desc) = schema["function"]["description"].as_str()
        {
            schema["function"]["description"] =
                serde_json::Value::String(format!("{desc}\n\n{summary}"));
        }
        schema
    }
}

fn score_entry(entry: &DeferredToolEntry, query_lower: &str, tokens: &[String]) -> u32 {
    if tokens.is_empty() {
        return 1;
    }
    let name_lower = entry.name.to_lowercase();
    let desc_lower = entry.description.to_lowercase();
    let params_lower: Vec<String> = entry.parameters.iter().map(|p| p.to_lowercase()).collect();

    let mut score = 0u32;
    if !query_lower.is_empty() {
        if name_lower == query_lower {
            score += 200;
        } else if name_lower.contains(query_lower) {
            score += 80;
        }
    }

    for token in tokens {
        if name_lower == *token {
            score += 100;
        } else if name_lower.contains(token.as_str()) {
            score += 30;
        }
        if desc_lower.contains(token.as_str()) {
            score += 10;
        }
        if params_lower.iter().any(|p| p.contains(token.as_str())) {
            score += 6;
        }
    }
    score
}

pub(super) struct ToolSearchTool;

impl Tool for ToolSearchTool {
    fn name(&self) -> &'static str {
        TOOL_SEARCH_TOOL
    }

    fn read_only(&self) -> bool {
        true
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": TOOL_SEARCH_TOOL,
                "description": "Search and load deferred MCP/plugin tools by keyword or exact name (`select:tool_a,tool_b`). Matched tools are activated for this session so their full schemas become callable on the next step.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Keywords to match against tool names, descriptions, and parameters, or `select:name1,name2` to load exact tool names."
                        },
                        "max_results": {
                            "type": "integer",
                            "description": "Maximum number of tools to load (default 8, max 20)."
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
        ctx: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ToolEffect, String>> + Send + 'a>> {
        Box::pin(async move {
            let query = args["query"]
                .as_str()
                .ok_or("Missing required parameter: query")?;
            let max_results = args["max_results"]
                .as_u64()
                .map(|v| v as usize)
                .unwrap_or(DEFAULT_MAX_RESULTS);

            let matches = {
                let mut registry = ctx
                    .state
                    .deferred_tools
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                registry.search_and_activate(query, max_results)
            };

            if matches.is_empty() {
                return Ok(ToolEffect::plain(format!(
                    "No deferred tools matched `{query}`."
                )));
            }

            let mut lines = vec![format!(
                "Loaded {} tool(s) for this session (their full schemas are now available to call):",
                matches.len()
            )];
            for entry in matches {
                let params = if entry.parameters.is_empty() {
                    "(no parameters)".to_string()
                } else {
                    format!("params: {}", entry.parameters.join(", "))
                };
                let desc = entry
                    .description
                    .lines()
                    .next()
                    .unwrap_or("")
                    .trim();
                if desc.is_empty() {
                    lines.push(format!("- `{}` — {}", entry.name, params));
                } else {
                    lines.push(format!("- `{}` — {} ({})", entry.name, desc, params));
                }
            }
            Ok(ToolEffect::plain(lines.join("\n")))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_entry(name: &str, desc: &str, params: &[&str]) -> DeferredToolEntry {
        let mut props = serde_json::Map::new();
        for p in params {
            props.insert((*p).to_string(), serde_json::json!({ "type": "string" }));
        }
        DeferredToolEntry {
            name: name.to_string(),
            description: desc.to_string(),
            parameters: params.iter().map(|s| (*s).to_string()).collect(),
            schema: serde_json::json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": desc,
                    "parameters": { "type": "object", "properties": props }
                }
            }),
        }
    }

    #[test]
    fn threshold_gates_deferral_and_preserves_append_only_activation_order() {
        let mut registry = DeferredToolRegistry::with_threshold(2);
        registry.sync_catalog(vec![
            fake_entry("mcp__gh__list_prs", "List pull requests", &["repo"]),
            fake_entry("mcp__gh__create_issue", "Create GitHub issue", &["title"]),
        ]);
        assert!(!registry.is_deferral_active());

        registry.sync_catalog(vec![
            fake_entry("mcp__gh__list_prs", "List pull requests", &["repo"]),
            fake_entry("mcp__gh__create_issue", "Create GitHub issue", &["title"]),
            fake_entry("mcp__db__query", "Run SQL query", &["sql"]),
        ]);
        assert!(registry.is_deferral_active());

        let first = registry.search_and_activate("sql query", 5);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].name, "mcp__db__query");

        let second = registry.search_and_activate("select:mcp__gh__create_issue,mcp__db__query", 5);
        assert_eq!(second.len(), 2);
        // Append-only: mcp__db__query stays first so prefix cache does not shift.
        assert_eq!(
            registry.activated_names(),
            &["mcp__db__query", "mcp__gh__create_issue"]
        );
    }

    #[tokio::test]
    async fn tool_search_tool_loads_matches_via_session_state() {
        let (tx, _rx) = async_channel::unbounded();
        let state = crate::task::SessionToolState::new("s-search".into(), tx, vec![], vec![]);
        {
            let mut reg = state.deferred_tools.lock().unwrap();
            *reg = DeferredToolRegistry::with_threshold(1);
            reg.sync_catalog(vec![
                fake_entry("mcp__slack__post_message", "Send a Slack message", &["channel", "text"]),
                fake_entry("mcp__jira__get_ticket", "Fetch a Jira ticket", &["key"]),
            ]);
        }
        let mut tracker = ChangeTracker::default();
        let cwd = std::env::temp_dir();
        let ctx = ToolContext {
            cwd: &cwd,
            tracker: &mut tracker,
            state: &state,
            fs_grant: None,
            sandbox_override: crate::task::SandboxOverride::default(),
            sandbox_profile: crate::task::SandboxProfile::WorkspaceWrite,
            additional_permissions: crate::task::AdditionalPermissions::default(),
        };
        let effect = ToolSearchTool
            .execute(serde_json::json!({ "query": "slack" }), ctx)
            .await
            .expect("tool search");
        assert!(effect.output.contains("mcp__slack__post_message"));
        let activated = state
            .deferred_tools
            .lock()
            .unwrap()
            .activated_names()
            .to_vec();
        assert_eq!(activated, vec!["mcp__slack__post_message"]);
    }
}
