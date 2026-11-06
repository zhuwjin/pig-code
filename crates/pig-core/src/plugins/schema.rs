//! The JSON Schema subset plugin tools declare for their arguments and results.
//!
//! The host validates every call before it reaches the backend and every result
//! before it reaches the model. The subset is deliberately small and closed: a
//! schema that uses another keyword is rejected at install time, so a plugin
//! cannot believe it is validated when the host silently ignores a constraint.
//!
//! Supported: `type` (string or list), `enum`, `const`, `properties`,
//! `required`, `additionalProperties`, `items`, `minItems`/`maxItems`,
//! `minLength`/`maxLength`, `pattern`, `minimum`/`maximum`,
//! `exclusiveMinimum`/`exclusiveMaximum`, and `allOf`/`anyOf`/`oneOf`.

use serde_json::Value;

/// Keywords the validator implements; anything else fails install validation.
const SUPPORTED: &[&str] = &[
    "$schema",
    "title",
    "description",
    "default",
    "examples",
    "type",
    "enum",
    "const",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "minItems",
    "maxItems",
    "minLength",
    "maxLength",
    "pattern",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "allOf",
    "anyOf",
    "oneOf",
];

/// Checks that `schema` is a schema the host can enforce.
pub(crate) fn check_supported(schema: &Value) -> Result<(), String> {
    match schema {
        Value::Bool(_) => Ok(()),
        Value::Object(fields) => {
            for (key, value) in fields {
                if !SUPPORTED.contains(&key.as_str()) {
                    return Err(format!("unsupported JSON Schema keyword `{key}`"));
                }
                match key.as_str() {
                    "properties" => {
                        let properties = value
                            .as_object()
                            .ok_or_else(|| "`properties` must be an object".to_owned())?;
                        for (name, property) in properties {
                            check_supported(property)
                                .map_err(|error| format!("properties.{name}: {error}"))?;
                        }
                    }
                    "items" | "additionalProperties" => check_supported(value)?,
                    "allOf" | "anyOf" | "oneOf" => {
                        let alternatives = value
                            .as_array()
                            .ok_or_else(|| format!("`{key}` must be an array"))?;
                        if alternatives.is_empty() {
                            return Err(format!("`{key}` must not be empty"));
                        }
                        for (index, alternative) in alternatives.iter().enumerate() {
                            check_supported(alternative)
                                .map_err(|error| format!("{key}[{index}]: {error}"))?;
                        }
                    }
                    "type" => {
                        for name in type_names(value)? {
                            if !matches!(
                                name,
                                "object"
                                    | "array"
                                    | "string"
                                    | "number"
                                    | "integer"
                                    | "boolean"
                                    | "null"
                            ) {
                                return Err(format!("unknown JSON Schema type `{name}`"));
                            }
                        }
                    }
                    "pattern" => {
                        let pattern = value
                            .as_str()
                            .ok_or_else(|| "`pattern` must be a string".to_owned())?;
                        regex::Regex::new(pattern)
                            .map_err(|error| format!("invalid `pattern`: {error}"))?;
                    }
                    _ => {}
                }
            }
            Ok(())
        }
        _ => Err("a JSON Schema must be an object or a boolean".into()),
    }
}

/// Validates `value` against `schema`. The returned issues are
/// `path: message`, capped so a broken call cannot flood the model.
pub(crate) fn validate(schema: &Value, value: &Value) -> Result<(), Vec<String>> {
    let mut issues = Vec::new();
    validate_at(schema, value, "$", &mut issues);
    issues.truncate(8);
    if issues.is_empty() {
        Ok(())
    } else {
        Err(issues)
    }
}

fn validate_at(schema: &Value, value: &Value, path: &str, issues: &mut Vec<String>) {
    if issues.len() >= 8 {
        return;
    }
    match schema {
        Value::Bool(true) => {}
        Value::Bool(false) => issues.push(format!("{path}: no value is allowed here")),
        Value::Object(fields) => {
            if let Some(expected) = fields.get("type")
                && !matches_type(expected, value)
            {
                let expected = type_names(expected)
                    .map(|names| names.join(" | "))
                    .unwrap_or_default();
                issues.push(format!(
                    "{path}: expected {expected}, found {}",
                    json_type(value)
                ));
                return;
            }
            if let Some(constant) = fields.get("const")
                && value != constant
            {
                issues.push(format!("{path}: must equal {constant}"));
            }
            if let Some(choices) = fields.get("enum").and_then(Value::as_array)
                && !choices.contains(value)
            {
                issues.push(format!("{path}: must be one of {choices:?}"));
            }
            if let Some(alternatives) = fields.get("allOf").and_then(Value::as_array) {
                for alternative in alternatives {
                    validate_at(alternative, value, path, issues);
                }
            }
            if let Some(alternatives) = fields.get("anyOf").and_then(Value::as_array)
                && !alternatives
                    .iter()
                    .any(|schema| validate(schema, value).is_ok())
            {
                issues.push(format!("{path}: does not match any allowed shape"));
            }
            if let Some(alternatives) = fields.get("oneOf").and_then(Value::as_array) {
                let matches = alternatives
                    .iter()
                    .filter(|schema| validate(schema, value).is_ok())
                    .count();
                if matches != 1 {
                    issues.push(format!(
                        "{path}: must match exactly one allowed shape ({matches} matched)"
                    ));
                }
            }
            match value {
                Value::Object(object) => {
                    if let Some(required) = fields.get("required").and_then(Value::as_array) {
                        for name in required.iter().filter_map(Value::as_str) {
                            if !object.contains_key(name) {
                                issues.push(format!("{path}: missing required `{name}`"));
                            }
                        }
                    }
                    let properties = fields.get("properties").and_then(Value::as_object);
                    if let Some(properties) = properties {
                        for (name, property) in properties {
                            if let Some(field) = object.get(name) {
                                validate_at(property, field, &format!("{path}.{name}"), issues);
                            }
                        }
                    }
                    if let Some(additional) = fields.get("additionalProperties") {
                        for (name, field) in object {
                            if properties.is_some_and(|properties| properties.contains_key(name)) {
                                continue;
                            }
                            match additional {
                                Value::Bool(false) => {
                                    issues.push(format!("{path}: unexpected `{name}`"))
                                }
                                other => {
                                    validate_at(other, field, &format!("{path}.{name}"), issues)
                                }
                            }
                        }
                    }
                }
                Value::Array(items) => {
                    if let Some(min) = fields.get("minItems").and_then(Value::as_u64)
                        && (items.len() as u64) < min
                    {
                        issues.push(format!("{path}: needs at least {min} items"));
                    }
                    if let Some(max) = fields.get("maxItems").and_then(Value::as_u64)
                        && (items.len() as u64) > max
                    {
                        issues.push(format!("{path}: allows at most {max} items"));
                    }
                    if let Some(item_schema) = fields.get("items") {
                        for (index, item) in items.iter().enumerate() {
                            validate_at(item_schema, item, &format!("{path}[{index}]"), issues);
                        }
                    }
                }
                Value::String(text) => {
                    if let Some(min) = fields.get("minLength").and_then(Value::as_u64)
                        && (text.chars().count() as u64) < min
                    {
                        issues.push(format!("{path}: needs at least {min} characters"));
                    }
                    if let Some(max) = fields.get("maxLength").and_then(Value::as_u64)
                        && (text.chars().count() as u64) > max
                    {
                        issues.push(format!("{path}: allows at most {max} characters"));
                    }
                    if let Some(pattern) = fields.get("pattern").and_then(Value::as_str)
                        && let Ok(pattern) = regex::Regex::new(pattern)
                        && !pattern.is_match(text)
                    {
                        issues.push(format!("{path}: does not match `{pattern}`"));
                    }
                }
                Value::Number(number) => {
                    let number = number.as_f64().unwrap_or_default();
                    if let Some(min) = fields.get("minimum").and_then(Value::as_f64)
                        && number < min
                    {
                        issues.push(format!("{path}: must be at least {min}"));
                    }
                    if let Some(max) = fields.get("maximum").and_then(Value::as_f64)
                        && number > max
                    {
                        issues.push(format!("{path}: must be at most {max}"));
                    }
                    if let Some(min) = fields.get("exclusiveMinimum").and_then(Value::as_f64)
                        && number <= min
                    {
                        issues.push(format!("{path}: must be greater than {min}"));
                    }
                    if let Some(max) = fields.get("exclusiveMaximum").and_then(Value::as_f64)
                        && number >= max
                    {
                        issues.push(format!("{path}: must be less than {max}"));
                    }
                }
                _ => {}
            }
        }
        _ => issues.push(format!("{path}: the schema is not valid")),
    }
}

fn type_names(value: &Value) -> Result<Vec<&str>, String> {
    match value {
        Value::String(name) => Ok(vec![name.as_str()]),
        Value::Array(names) => names
            .iter()
            .map(|name| {
                name.as_str()
                    .ok_or_else(|| "`type` entries must be strings".to_owned())
            })
            .collect(),
        _ => Err("`type` must be a string or an array of strings".into()),
    }
}

fn matches_type(expected: &Value, value: &Value) -> bool {
    let Ok(names) = type_names(expected) else {
        return false;
    };
    names.iter().any(|expected| match *expected {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => false,
    })
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(number) if number.as_i64().is_some() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn object_constraints_and_nested_paths_are_reported() {
        let schema = json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "minLength": 1},
                "limit": {"type": "integer", "minimum": 1, "maximum": 10}
            },
            "required": ["path"],
            "additionalProperties": false
        });
        assert!(validate(&schema, &json!({"path": "a", "limit": 3})).is_ok());
        let issues = validate(&schema, &json!({"limit": 11, "extra": true})).unwrap_err();
        assert!(
            issues
                .iter()
                .any(|issue| issue.contains("missing required `path`"))
        );
        assert!(issues.iter().any(|issue| issue.contains("$.limit")));
        assert!(
            issues
                .iter()
                .any(|issue| issue.contains("unexpected `extra`"))
        );
        let issues = validate(&schema, &json!({"path": ""})).unwrap_err();
        assert!(issues[0].contains("$.path"));
    }

    #[test]
    fn arrays_unions_and_patterns_validate() {
        let schema = json!({
            "type": "object",
            "properties": {
                "tags": {"type": "array", "items": {"type": "string"}, "maxItems": 2},
                "mode": {"anyOf": [{"const": "fast"}, {"const": "safe"}]},
                "id": {"type": "string", "pattern": "^[a-z]+$"}
            }
        });
        assert!(
            validate(
                &schema,
                &json!({"tags": ["a", "b"], "mode": "safe", "id": "abc"})
            )
            .is_ok()
        );
        let issues = validate(
            &schema,
            &json!({"tags": ["a", "b", "c"], "mode": "other", "id": "ABC"}),
        )
        .unwrap_err();
        assert!(issues.iter().any(|issue| issue.contains("at most 2")));
        assert!(issues.iter().any(|issue| issue.contains("$.mode")));
        assert!(issues.iter().any(|issue| issue.contains("$.id")));
    }

    #[test]
    fn unsupported_keywords_fail_the_install_check() {
        assert!(check_supported(&json!({"type": "object"})).is_ok());
        assert!(
            check_supported(&json!({"$ref": "#/definitions/a"}))
                .unwrap_err()
                .contains("$ref")
        );
        assert!(
            check_supported(&json!({"type": "object", "properties": {"a": {"fancy": true}}}))
                .unwrap_err()
                .contains("properties.a")
        );
        assert!(
            check_supported(&json!({"type": "nope"}))
                .unwrap_err()
                .contains("type")
        );
        assert!(
            check_supported(&json!({"pattern": "([bad"}))
                .unwrap_err()
                .contains("pattern")
        );
    }
}
