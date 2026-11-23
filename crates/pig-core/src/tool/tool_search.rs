//! Model-agnostic fixed-proxy runtime tools (`ToolSearch` + `CallTool`).
//!
//! Built-in tools stay directly in the request `tools[]` array. Whenever a
//! session exposes any runtime tools (MCP servers or plugin tools), those tools
//! are NEVER inserted into `tools[]`: instead, `tools[]` stays completely frozen
//! with two fixed proxy tools:
//! - [`TOOL_SEARCH_TOOL`] (`ToolSearch`): searches the live MCP/plugin catalog
//!   and returns each matching tool's full JSON `parameters` schema in the tool
//!   result.
//! - [`CALL_TOOL`] (`CallTool`): invokes any runtime MCP/plugin tool by
//!   `{ name, arguments }`. The permission gate (`session/gate.rs`) unwraps
//!   `CallTool` to the real target tool before PreToolUse/PostToolUse hooks,
//!   deny/allow rules, approval dialogs, and schema validation.
//!
//! Because `tools[]` never mutates across turns (even if an MCP server
//! reconnects or adds tools), LLM prefix caches stay 100% intact across all
//! providers and models.

use super::*;

pub const TOOL_SEARCH_TOOL: &str = "ToolSearch";
pub const CALL_TOOL: &str = "CallTool";

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

/// Session-scoped runtime tool catalog backing `ToolSearch` and `CallTool`.
///
/// The initial non-empty catalog summary is pinned once for the session so the
/// `ToolSearch` wire schema in `tools[]` stays byte-identical across turns and
/// never busts the prompt prefix cache. Live searches always query the latest
/// synced `catalog`.
#[derive(Clone, Debug, Default)]
pub struct DeferredToolRegistry {
    catalog: Vec<DeferredToolEntry>,
    pinned_summary: Option<String>,
}

impl DeferredToolRegistry {
    /// Replace the live runtime-tool catalog (MCP + filtered plugins).
    pub fn sync_catalog(&mut self, entries: Vec<DeferredToolEntry>) {
        self.catalog = entries;
        if self.pinned_summary.is_none() && !self.catalog.is_empty() {
            self.pinned_summary = Some(Self::format_catalog_summary(&self.catalog));
        }
    }

    /// True when any runtime (MCP or plugin) tools are available for this
    /// session.
    pub fn has_runtime_tools(&self) -> bool {
        !self.catalog.is_empty()
    }

    pub fn total_catalog_len(&self) -> usize {
        self.catalog.len()
    }

    /// Search live runtime tools by keyword query, `select:a,b` exact selector,
    /// or empty / `*` to list tools.
    pub fn search(&self, query: &str, max_results: usize) -> Vec<DeferredToolEntry> {
        let limit = max_results.clamp(1, HARD_MAX_RESULTS);
        let trimmed = query.trim();

        if let Some(selection) = trimmed
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
            return out;
        }

        if trimmed.is_empty() || trimmed == "*" {
            let mut all = self.catalog.clone();
            all.sort_by(|a, b| a.name.cmp(&b.name));
            all.truncate(limit);
            return all;
        }

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
    }

    fn format_catalog_summary(catalog: &[DeferredToolEntry]) -> String {
        if catalog.is_empty() {
            return String::new();
        }
        let mut names: Vec<&str> = catalog.iter().map(|e| e.name.as_str()).collect();
        names.sort_unstable();
        let total = names.len();
        if total <= MAX_CATALOG_NAMES_IN_DESCRIPTION {
            format!("Available MCP/plugin tools ({total}): {}", names.join(", "))
        } else {
            let shown = &names[..MAX_CATALOG_NAMES_IN_DESCRIPTION];
            format!(
                "Available MCP/plugin tools ({total}, showing first {MAX_CATALOG_NAMES_IN_DESCRIPTION}): {}, ... (+{} more; search by keyword)",
                shown.join(", "),
                total - MAX_CATALOG_NAMES_IN_DESCRIPTION
            )
        }
    }

    /// Build the frozen `ToolSearch` wire schema augmented with the session-
    /// pinned catalog summary.
    pub fn tool_search_schema(&self) -> serde_json::Value {
        let mut schema = ToolSearchTool.schema();
        if let Some(summary) = &self.pinned_summary
            && !summary.is_empty()
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

/// Unwrap a `CallTool` invocation into `(target_tool_name, target_arguments_json)`.
///
/// Returns `None` when `call.name != CALL_TOOL`. Accepts omitted `arguments`
/// (normalized to `"{}"`) and tolerates a JSON-encoded object string if a model
/// stringifies the `arguments` field.
pub fn unwrap_call_tool(call: &ToolCall) -> Option<Result<(String, String), String>> {
    if call.name != CALL_TOOL {
        return None;
    }
    let parsed: serde_json::Value = match serde_json::from_str(&call.arguments) {
        Ok(value) => value,
        Err(error) => {
            return Some(Err(format!(
                "Invalid CallTool arguments JSON: {error}"
            )));
        }
    };
    let Some(name) = parsed
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Some(Err(
            "CallTool requires a non-empty string `name` parameter identifying the MCP/plugin tool."
                .to_string(),
        ));
    };
    let args_value = match parsed.get("arguments") {
        None | Some(serde_json::Value::Null) => serde_json::json!({}),
        Some(obj @ serde_json::Value::Object(_)) => obj.clone(),
        Some(serde_json::Value::String(raw)) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                serde_json::json!({})
            } else {
                match serde_json::from_str::<serde_json::Value>(trimmed) {
                    Ok(obj @ serde_json::Value::Object(_)) => obj,
                    _ => {
                        return Some(Err(format!(
                            "CallTool `arguments` for `{name}` must be a JSON object."
                        )));
                    }
                }
            }
        }
        Some(_) => {
            return Some(Err(format!(
                "CallTool `arguments` for `{name}` must be a JSON object."
            )));
        }
    };
    let args_json = serde_json::to_string(&args_value).unwrap_or_else(|_| "{}".to_string());
    Some(Ok((name.to_string(), args_json)))
}

/// Validate target tool arguments against the tool's declared JSON Schema
/// (`function.parameters`), returning an actionable error carrying the full
/// parameter schema when required fields are missing or top-level types do not
/// match.
pub fn validate_tool_arguments(
    tool_schema: &serde_json::Value,
    arguments_json: &str,
) -> Result<(), String> {
    let tool_name = tool_schema["function"]["name"]
        .as_str()
        .unwrap_or("tool");
    let params_schema = &tool_schema["function"]["parameters"];
    let args: serde_json::Value = serde_json::from_str(arguments_json).map_err(|error| {
        format!(
            "Invalid JSON arguments for `{tool_name}`: {error}\n\nExpected parameters schema:\n{}",
            serde_json::to_string_pretty(params_schema).unwrap_or_default()
        )
    })?;
    let Some(obj) = args.as_object() else {
        return Err(format!(
            "Arguments for `{tool_name}` must be a JSON object.\n\nExpected parameters schema:\n{}",
            serde_json::to_string_pretty(params_schema).unwrap_or_default()
        ));
    };

    let mut errors = Vec::new();
    if let Some(required) = params_schema.get("required").and_then(|v| v.as_array()) {
        for req in required.iter().filter_map(|v| v.as_str()) {
            if !obj.contains_key(req) {
                errors.push(format!("missing required parameter `{req}`"));
            }
        }
    }
    if let Some(props) = params_schema.get("properties").and_then(|v| v.as_object()) {
        for (key, value) in obj {
            if let Some(prop_schema) = props.get(key)
                && !matches_json_schema_type(value, prop_schema)
            {
                let expected = prop_schema
                    .get("type")
                    .map(|t| t.to_string())
                    .unwrap_or_else(|| "valid value".to_string());
                errors.push(format!(
                    "parameter `{key}` does not match expected type {expected}"
                ));
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Invalid arguments for `{tool_name}`: {}.\n\nExpected parameters schema:\n{}",
            errors.join("; "),
            serde_json::to_string_pretty(params_schema).unwrap_or_default()
        ))
    }
}

fn matches_json_schema_type(value: &serde_json::Value, prop_schema: &serde_json::Value) -> bool {
    let Some(schema_type) = prop_schema.get("type") else {
        return true;
    };
    let matches_one = |ty: &str| match ty {
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "integer" => value.is_i64() || value.is_u64(),
        "number" => value.is_number(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "null" => value.is_null(),
        _ => true,
    };
    match schema_type {
        serde_json::Value::String(ty) => matches_one(ty),
        serde_json::Value::Array(types) => types
            .iter()
            .filter_map(|v| v.as_str())
            .any(matches_one),
        _ => true,
    }
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
                "description": "Search available MCP and plugin tools by keyword, exact name (`select:tool_a,tool_b`), or `*` to list them. Returns each matching tool's full JSON parameter schema so you can invoke it with `CallTool`.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Keywords to match against tool names, descriptions, and parameters; `select:name1,name2` for exact names; or `*` to list tools."
                        },
                        "max_results": {
                            "type": "integer",
                            "description": "Maximum number of tools to return (default 8, max 20)."
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
                let registry = ctx
                    .state
                    .deferred_tools
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                registry.search(query, max_results)
            };

            if matches.is_empty() {
                return Ok(ToolEffect::plain(format!(
                    "No MCP or plugin tools matched `{query}`."
                )));
            }

            let mut sections = vec![format!(
                "Found {} tool(s). Invoke any of them via `CallTool` with `{{\"name\": \"...\", \"arguments\": {{...}}}}`:",
                matches.len()
            )];
            for entry in matches {
                let params_json = serde_json::to_string_pretty(&entry.schema["function"]["parameters"])
                    .unwrap_or_else(|_| "{}".to_string());
                let desc = entry.description.trim();
                let mut block = format!("### `{}`", entry.name);
                if !desc.is_empty() {
                    block.push('\n');
                    block.push_str(desc);
                }
                block.push_str("\nParameters JSON Schema:\n```json\n");
                block.push_str(&params_json);
                block.push_str("\n```");
                sections.push(block);
            }
            Ok(ToolEffect::plain(sections.join("\n\n")))
        })
    }
}

pub(super) struct CallToolProxy;

impl Tool for CallToolProxy {
    fn name(&self) -> &'static str {
        CALL_TOOL
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": CALL_TOOL,
                "description": "Invoke an MCP or plugin tool by name with a JSON arguments object. Use `ToolSearch` first to inspect the tool's exact name and JSON parameter schema. Built-in tools (Read, Edit, Write, ApplyPatch, ExecCommand, etc.) should be called directly instead of through CallTool.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Exact name of the MCP or plugin tool to invoke."
                        },
                        "arguments": {
                            "type": "object",
                            "description": "JSON object of arguments matching the tool's parameter schema (omit or pass {} if the tool takes no parameters)."
                        }
                    },
                    "required": ["name"]
                }
            }
        })
    }

    fn execute<'a>(
        &'a self,
        _args: serde_json::Value,
        _ctx: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ToolEffect, String>> + Send + 'a>> {
        Box::pin(async move {
            Err("CallTool must be dispatched through the session permission gate".to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_entry(name: &str, desc: &str, required: &[&str], params: &[(&str, &str)]) -> DeferredToolEntry {
        let mut props = serde_json::Map::new();
        for (k, ty) in params {
            props.insert((*k).to_string(), serde_json::json!({ "type": ty }));
        }
        DeferredToolEntry {
            name: name.to_string(),
            description: desc.to_string(),
            parameters: params.iter().map(|(k, _)| (*k).to_string()).collect(),
            schema: serde_json::json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": desc,
                    "parameters": {
                        "type": "object",
                        "properties": props,
                        "required": required
                    }
                }
            }),
        }
    }

    #[test]
    fn catalog_summary_stays_pinned_while_search_sees_live_updates() {
        let mut registry = DeferredToolRegistry::default();
        assert!(!registry.has_runtime_tools());

        registry.sync_catalog(vec![fake_entry(
            "mcp__gh__list_prs",
            "List pull requests",
            &["repo"],
            &[("repo", "string")],
        )]);
        assert!(registry.has_runtime_tools());
        let first_schema = registry.tool_search_schema();
        assert!(first_schema["function"]["description"]
            .as_str()
            .unwrap()
            .contains("mcp__gh__list_prs"));

        // Adding a new tool later does NOT mutate the pinned ToolSearch wire
        // schema (preserving prefix cache), yet live search finds it.
        registry.sync_catalog(vec![
            fake_entry(
                "mcp__gh__list_prs",
                "List pull requests",
                &["repo"],
                &[("repo", "string")],
            ),
            fake_entry(
                "mcp__db__query",
                "Run SQL query",
                &["sql"],
                &[("sql", "string")],
            ),
        ]);
        assert_eq!(registry.tool_search_schema(), first_schema);
        let hits = registry.search("sql", 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "mcp__db__query");
    }

    #[tokio::test]
    async fn tool_search_returns_full_json_schema_and_call_tool_instructions() {
        let (tx, _rx) = async_channel::unbounded();
        let state = crate::task::SessionToolState::new("s-search".into(), tx, vec![], vec![]);
        state
            .deferred_tools
            .lock()
            .unwrap()
            .sync_catalog(vec![fake_entry(
                "mcp__slack__post_message",
                "Send a Slack message",
                &["channel", "text"],
                &[("channel", "string"), ("text", "string")],
            )]);

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
        assert!(effect.output.contains("CallTool"));
        assert!(effect.output.contains("\"required\""));
        assert!(effect.output.contains("\"channel\""));
    }

    #[test]
    fn unwrap_call_tool_and_validate_arguments_work() {
        let call = ToolCall {
            id: "c1".into(),
            name: CALL_TOOL.into(),
            arguments: r#"{"name":"mcp__db__query","arguments":{"sql":"SELECT 1","limit":10}}"#.into(),
        };
        let (name, args) = unwrap_call_tool(&call).unwrap().expect("unwrap");
        assert_eq!(name, "mcp__db__query");

        let entry = fake_entry(
            "mcp__db__query",
            "Run SQL",
            &["sql"],
            &[("sql", "string"), ("limit", "integer")],
        );
        validate_tool_arguments(&entry.schema, &args).expect("valid args");

        let missing = validate_tool_arguments(&entry.schema, "{}").unwrap_err();
        assert!(missing.contains("missing required parameter `sql`"), "{missing}");
        assert!(missing.contains("Expected parameters schema"), "{missing}");

        let bad_type =
            validate_tool_arguments(&entry.schema, r#"{"sql":"SELECT 1","limit":"ten"}"#)
                .unwrap_err();
        assert!(bad_type.contains("`limit`"), "{bad_type}");

        // Omitted arguments defaults to `{}` and stringified JSON object is tolerated.
        let bare = ToolCall {
            id: "c2".into(),
            name: CALL_TOOL.into(),
            arguments: r#"{"name":"mcp__db__ping"}"#.into(),
        };
        assert_eq!(
            unwrap_call_tool(&bare).unwrap().unwrap(),
            ("mcp__db__ping".to_string(), "{}".to_string())
        );
    }
}
