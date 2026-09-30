use super::{
    McpSource, McpTransport, api_key_configured, format_timeout, merge_servers, parse_servers,
};

#[test]
fn parse_stdio_and_remote() {
    let raw = r#"{
        "mcpServers": {
            "fs": {"command": "npx", "args": ["-y", "@mcp/fs"], "timeoutMs": 60000},
            "remote": {"url": "https://mcp.example.com/sse", "headers": {"A": "b"}}
        }
    }"#;
    let servers = parse_servers(raw, McpSource::User);
    assert_eq!(servers.len(), 2);
    let fs = servers.iter().find(|s| s.name == "fs").expect("fs");
    assert_eq!(
        fs.transport,
        McpTransport::Stdio {
            command: "npx".to_string(),
            args: vec!["-y".to_string(), "@mcp/fs".to_string()],
        }
    );
    assert_eq!(fs.timeout_ms, Some(60_000));
    let remote = servers.iter().find(|s| s.name == "remote").expect("remote");
    assert_eq!(
        remote.transport,
        McpTransport::Remote {
            url: "https://mcp.example.com/sse".to_string()
        }
    );
    assert_eq!(remote.timeout_ms, None);
}

#[test]
fn skips_invalid_entries() {
    let raw = r#"{"mcpServers": {
        "good": {"command": "srv"},
        "empty": {"command": "  "},
        "nocmd": {"args": []},
        "badtype": {"command": 42}
    }}"#;
    let servers = parse_servers(raw, McpSource::Project);
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0].name, "good");
    assert!(parse_servers("not json", McpSource::User).is_empty());
    assert!(parse_servers("{}", McpSource::User).is_empty());
}

#[test]
fn project_overrides_user() {
    let user = parse_servers(
        r#"{"mcpServers": {"a": {"command": "ua"}, "b": {"command": "ub"}}}"#,
        McpSource::User,
    );
    let project = parse_servers(
        r#"{"mcpServers": {"b": {"command": "pb"}, "c": {"url": "https://x"}}}"#,
        McpSource::Project,
    );
    let merged = merge_servers(user, project);
    let rows: Vec<(&str, McpSource, bool)> = merged
        .iter()
        .map(|s| (s.name.as_str(), s.source, s.overrides_user))
        .collect();
    assert_eq!(
        rows,
        vec![
            ("a", McpSource::User, false),
            ("b", McpSource::Project, true),
            ("c", McpSource::Project, false),
        ]
    );
}

#[test]
fn timeout_formatting() {
    assert_eq!(format_timeout(None), "超时 30s（默认）");
    assert_eq!(format_timeout(Some(60_000)), "超时 60s");
    assert_eq!(format_timeout(Some(1500)), "超时 1500ms");
}

#[test]
fn env_key_configured_semantics() {
    assert!(!api_key_configured(None));
    assert!(!api_key_configured(Some(String::new())));
    assert!(!api_key_configured(Some("  ".to_string())));
    assert!(api_key_configured(Some("tvly-xxx".to_string())));
}
