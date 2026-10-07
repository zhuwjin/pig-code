use super::*;

/// ^[a-zA-Z0-9-]{3,50}$ (all ASCII, so byte length equals char count)
pub(crate) fn valid_name(name: &str) -> bool {
    (3..=50).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Strip the paired quotes around a value ("..." or '...')
pub(crate) fn strip_quotes(value: &str) -> &str {
    let bytes = value.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

/// model field: empty / inherit / main = inherit the parent session (None)
pub(crate) fn parse_model_value(raw: &str) -> Option<String> {
    let value = raw.trim();
    if value.is_empty() || value == "inherit" || value == "main" {
        None
    } else {
        Some(value.to_string())
    }
}

/// Parse a subagent Markdown profile: `---`-delimited frontmatter + body (system prompt).
/// The frontmatter is parsed line by line by hand (no serde_yaml dependency, same approach
/// as ZCode): supports `key: value` scalars, `key:` + a following indented `- item` list,
/// an inline `[a, b]` list, `#` comment lines, and quote stripping around values; unknown
/// fields are ignored.
pub fn parse_agent_markdown(content: &str) -> Result<AgentProfile, String> {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let lines: Vec<&str> = content.lines().collect();
    if lines.first().map(|line| line.trim()) != Some("---") {
        return Err(
            "Invalid subagent profile format: the first line must be --- (frontmatter start)"
                .to_string(),
        );
    }

    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    let mut tools: Option<Vec<String>> = None;
    let mut model: Option<String> = None;
    let mut thought_level: Option<String> = None;
    let mut max_turns: Option<usize> = None;
    let mut inject_agents_md = true;

    let mut i = 1;
    let mut closed = false;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        if trimmed == "---" {
            closed = true;
            i += 1;
            break;
        }
        // Skip blank lines and # comment lines
        if trimmed.is_empty() || trimmed.starts_with('#') {
            i += 1;
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            i += 1; // unrecognized line: skip leniently
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        // Value shapes: inline [a, b] list / following indented `- item` list / scalar
        let mut list: Option<Vec<String>> = None;
        let scalar = strip_quotes(value).to_string();
        let mut consumed = 1;
        if value.starts_with('[') && value.ends_with(']') {
            list = Some(
                value[1..value.len() - 1]
                    .split(',')
                    .map(|item| strip_quotes(item.trim()).to_string())
                    .filter(|item| !item.is_empty())
                    .collect(),
            );
        } else if value.is_empty() {
            let mut items = Vec::new();
            while i + consumed < lines.len() {
                let raw = lines[i + consumed];
                let t = raw.trim();
                if (raw.starts_with(' ') || raw.starts_with('\t')) && t.starts_with('-') {
                    let item = strip_quotes(t[1..].trim());
                    if !item.is_empty() {
                        items.push(item.to_string());
                    }
                    consumed += 1;
                } else {
                    break;
                }
            }
            if !items.is_empty() {
                list = Some(items);
            }
        }
        i += consumed;
        match key {
            "name" => name = Some(scalar),
            "description" => description = Some(scalar),
            "tools" => {
                tools = list.or_else(|| (!scalar.is_empty()).then(|| vec![scalar.clone()]));
            }
            "model" => model = parse_model_value(&scalar),
            "thoughtLevel" | "thought_level" => {
                thought_level = (!scalar.is_empty()).then_some(scalar);
            }
            "maxTurns" | "max_turns" => {
                // 0/negative/non-numeric all error (negatives covered by the usize parse failure)
                let turns: usize = scalar.parse().map_err(|_| {
                    format!("maxTurns must be a positive integer, got \"{scalar}\"")
                })?;
                if turns == 0 {
                    return Err(
                        "maxTurns must be a positive integer (0 is not allowed)".to_string()
                    );
                }
                max_turns = Some(turns);
            }
            "injectAgentsMd" | "inject_agents_md" => {
                inject_agents_md = match scalar.as_str() {
                    "true" => true,
                    "false" => false,
                    _ => {
                        return Err(format!(
                            "injectAgentsMd only accepts true/false, got \"{scalar}\""
                        ));
                    }
                };
            }
            _ => {} // unknown fields are ignored
        }
    }
    if !closed {
        return Err(
            "Invalid subagent profile format: frontmatter is missing its closing --- line"
                .to_string(),
        );
    }

    let name = name
        .filter(|n| !n.is_empty())
        .ok_or_else(|| "frontmatter is missing required field name".to_string())?;
    if !valid_name(&name) {
        return Err(format!(
            "Invalid subagent name \"{name}\": must match ^[a-zA-Z0-9-]{{3,50}}$ \
             (3-50 ASCII letters, digits, or hyphens)"
        ));
    }
    let description = description
        .filter(|d| !d.is_empty())
        .ok_or_else(|| "frontmatter is missing required field description".to_string())?;
    let body = lines[i..].join("\n").trim().to_string();
    if body.is_empty() {
        return Err("Subagent profile body (system prompt) is empty".to_string());
    }
    Ok(AgentProfile {
        name,
        description,
        tools,
        model,
        thought_level,
        max_turns,
        inject_agents_md,
        system_prompt: body,
        source: AgentSource::BuiltIn, // placeholder: load_profiles rewrites it per source directory
    })
}
