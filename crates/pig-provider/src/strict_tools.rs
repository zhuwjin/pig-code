//! Strict tool schemas: rewrite a permissive JSON Schema into the subset the
//! strict modes accept (OpenAI strict function calling; Anthropic strict
//! tool_use), so the endpoint guarantees well-formed tool arguments by
//! constrained decoding. Algorithm from pi's constrained-sampling (all
//! properties required, `additionalProperties: false`, optional fields become
//! nullable unions) with ZCode's degradation: constraint keywords the subset
//! rejects (min/max/length/pattern/…) are FOLDED into the description — the
//! model still reads them and engine-side validation still enforces them —
//! and shapes that cannot be expressed return None, in which case the caller
//! sends the original schema without the strict flag (per-tool degradation;
//! MCP passthrough schemas never break a request).

use serde_json::{Map, Value};

/// Keywords the strict subset rejects but whose meaning survives as prose,
/// with their folded phrasing (ZCode's wording).
/// How one folded keyword renders as prose
type FoldPhrase = fn(&Value) -> String;
const FOLDED_KEYWORDS: &[(&str, FoldPhrase)] = &[
    ("minimum", |v| format!("minimum {v}")),
    ("maximum", |v| format!("maximum {v}")),
    ("exclusiveMinimum", |v| format!("greater than {v}")),
    ("exclusiveMaximum", |v| format!("less than {v}")),
    ("multipleOf", |v| format!("multiple of {v}")),
    ("minLength", |v| format!("at least {v} characters")),
    ("maxLength", |v| format!("at most {v} characters")),
    ("pattern", |v| format!("must match /{v}/")),
    ("minItems", |v| format!("at least {v} items")),
    ("maxItems", |v| format!("at most {v} items")),
];

/// Constructs the strict subset has no place for; a schema carrying any of
/// them degrades to non-strict (sent verbatim, no strict flag).
const INELIGIBLE_KEYWORDS: &[&str] = &[
    "$ref",
    "$defs",
    "definitions",
    "prefixItems",
    "allOf",
    "oneOf",
    "not",
    "if",
    "then",
    "else",
    "patternProperties",
    "propertyNames",
    "dependentSchemas",
    "dependencies",
    "unevaluatedProperties",
];

/// Whether the schema already admits null (a null entry in a type array, a
/// null variant in anyOf, or const/enum null) — such properties stay
/// untouched instead of being wrapped in a nullable union.
fn allows_null(node: &Value) -> bool {
    let Some(obj) = node.as_object() else {
        return false;
    };
    if let Some(Value::String(t)) = obj.get("type")
        && t == "null"
    {
        return true;
    }
    if let Some(Value::Array(types)) = obj.get("type")
        && types.iter().any(|t| t == "null")
    {
        return true;
    }
    if obj.get("const") == Some(&Value::Null) {
        return true;
    }
    if let Some(Value::Array(values)) = obj.get("enum")
        && values.iter().any(|v| v == &Value::Null)
    {
        return true;
    }
    obj.get("anyOf")
        .and_then(|v| v.as_array())
        .is_some_and(|variants| variants.iter().any(allows_null))
}

/// One node -> its strict-subset equivalent. None = inexpressible.
fn strict_node(node: &Value) -> Option<Value> {
    let obj = node.as_object()?;
    if INELIGIBLE_KEYWORDS.iter().any(|key| obj.contains_key(*key)) {
        return None;
    }
    let mut out = obj.clone();

    // Fold rejected constraint keywords into the description
    let mut folded: Vec<String> = Vec::new();
    for (key, phrase) in FOLDED_KEYWORDS {
        if let Some(value) = out.remove(*key) {
            folded.push(phrase(&value));
        }
    }
    if !folded.is_empty() {
        let note = format!("Constraints: {}.", folded.join("; "));
        let description = match out.remove("description") {
            Some(Value::String(existing)) if !existing.is_empty() => {
                format!("{existing} {note}")
            }
            _ => note,
        };
        out.insert("description".to_string(), Value::String(description));
    }

    // items: the tuple form (an array) has no strict equivalent; the single
    // schema form recurses
    if let Some(items) = out.remove("items") {
        if items.is_array() {
            return None;
        }
        out.insert("items".to_string(), strict_node(&items)?);
    }
    // anyOf variants recurse (pig's own nullable wrapper is anyOf[T, null])
    if let Some(Value::Array(variants)) = out.get_mut("anyOf") {
        for variant in variants.iter_mut() {
            *variant = strict_node(variant)?;
        }
    }

    if out.contains_key("properties") {
        // properties require a plain object type
        match out.get("type") {
            Some(Value::String(t)) if t == "object" => {}
            None => {
                out.insert("type".to_string(), Value::String("object".to_string()));
            }
            _ => return None,
        }
        let properties = out.remove("properties")?.as_object()?.clone();
        let original_required: Vec<String> = out
            .remove("required")
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();

        let mut strict_properties = Map::new();
        for (key, property) in &properties {
            // Originally-optional properties (absent from required) become
            // nullable unions — that is how optionality survives when
            // everything must be listed as required
            let strict_property = if original_required.contains(key) || allows_null(property) {
                strict_node(property)?
            } else {
                serde_json::json!({
                    "anyOf": [strict_node(property)?, {"type": "null"}]
                })
            };
            strict_properties.insert(key.clone(), strict_property);
        }
        // Every property is required, nothing extra allowed. The list is
        // sorted explicitly: serde_json Map iteration order depends on the
        // preserve_order feature (workspace builds unify it on), and stable
        // bytes keep the tools prefix deterministic
        let mut keys: Vec<&String> = properties.keys().collect();
        keys.sort();
        out.insert(
            "required".to_string(),
            Value::Array(keys.into_iter().map(|k| Value::String(k.clone())).collect()),
        );
        out.insert("additionalProperties".to_string(), Value::Bool(false));
        out.insert("properties".to_string(), Value::Object(strict_properties));
    }

    Some(Value::Object(out))
}

/// Rewrite a tool's parameters schema into the strict subset. None = the
/// schema cannot be expressed; the caller then sends it verbatim WITHOUT the
/// strict flag (per-tool degradation).
pub(crate) fn strictify_tool_schema(schema: &Value) -> Option<Value> {
    strict_node(schema)
}

#[cfg(test)]
mod tests {
    use super::strictify_tool_schema;
    use serde_json::json;

    fn required(schema: &serde_json::Value) -> Vec<String> {
        schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect()
    }

    /// Optional fields become nullable unions, everything lands in required,
    /// additionalProperties closes the object
    #[test]
    fn optional_fields_become_nullable_and_all_required() {
        let schema = json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "file path"},
                "offset": {"type": "integer"},
                "limit": {"type": "integer"}
            },
            "required": ["path"]
        });
        let strict = strictify_tool_schema(&schema).expect("expressible");
        assert_eq!(required(&strict), ["limit", "offset", "path"]);
        assert_eq!(strict["additionalProperties"], false);
        // path stays plain; the optional two become anyOf[T, null]
        assert_eq!(strict["properties"]["path"]["type"], "string");
        assert_eq!(
            strict["properties"]["offset"]["anyOf"][0]["type"],
            "integer"
        );
        assert_eq!(strict["properties"]["offset"]["anyOf"][1]["type"], "null");
    }

    /// Constraint keywords fold into the description as prose (the model
    /// still reads them; engine-side validation still enforces them)
    #[test]
    fn constraints_fold_into_description() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "minLength": 2, "maxLength": 30, "pattern": "^[a-z]+$"},
                "count": {"type": "integer", "minimum": 1, "maximum": 10}
            },
            "required": ["name", "count"]
        });
        let strict = strictify_tool_schema(&schema).expect("expressible");
        let name = &strict["properties"]["name"];
        assert!(name.get("minLength").is_none() && name.get("pattern").is_none());
        let description = name["description"].as_str().unwrap();
        assert!(
            description.contains("at least 2 characters"),
            "{description}"
        );
        assert!(description.contains("must match"), "{description}");
        assert!(
            strict["properties"]["count"]["description"]
                .as_str()
                .unwrap()
                .contains("minimum 1")
        );
    }

    /// Nested objects recurse; an already-nullable property is not wrapped
    /// twice
    #[test]
    fn nested_objects_recurse() {
        let schema = json!({
            "type": "object",
            "properties": {
                "region": {
                    "type": "object",
                    "properties": {"x": {"type": "integer"}, "y": {"type": "integer"}},
                    "required": ["x", "y"]
                },
                "note": {"anyOf": [{"type": "string"}, {"type": "null"}]}
            },
            "required": ["region"]
        });
        let strict = strictify_tool_schema(&schema).expect("expressible");
        let region = &strict["properties"]["region"];
        assert_eq!(required(region), ["x", "y"]);
        assert_eq!(region["additionalProperties"], false);
        // already nullable: unchanged shape (still an anyOf, not double-wrapped)
        assert_eq!(
            strict["properties"]["note"]["anyOf"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    /// Inexpressible shapes degrade: $ref / allOf / tuple items
    #[test]
    fn inexpressible_shapes_return_none() {
        assert!(strictify_tool_schema(&json!({"$ref": "#/defs/x"})).is_none());
        assert!(
            strictify_tool_schema(&json!({
                "type": "object",
                "properties": {"a": {"allOf": [{"type": "string"}]}},
                "required": ["a"]
            }))
            .is_none()
        );
        assert!(
            strictify_tool_schema(&json!({
                "type": "object",
                "properties": {"t": {"type": "array", "items": [{"type": "string"}, {"type": "integer"}]}},
                "required": ["t"]
            }))
            .is_none()
        );
    }
}
