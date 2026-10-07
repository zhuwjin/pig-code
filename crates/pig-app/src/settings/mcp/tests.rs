use super::{
    McpSource, McpTransport, api_key_configured, delete_mcp_server, format_timeout, merge_servers,
    parse_object_json, parse_servers, pretty_object, set_mcp_disabled, upsert_mcp_server,
};
use serde_json::json;
use std::path::PathBuf;

#[test]
fn parse_stdio_and_remote() {
    let raw = r#"{
        "mcpServers": {
            "fs": {"command": "npx", "args": ["-y", "@mcp/fs"], "env": {"A": "1"}, "timeoutMs": 60000},
            "remote": {"url": "https://mcp.example.com/mcp", "headers": {"A": "b"}}
        }
    }"#;
    let servers = parse_servers(raw, McpSource::User);
    assert_eq!(servers.len(), 2);
    let fs = servers.iter().find(|s| s.name == "fs").expect("fs");
    assert_eq!(
        fs.transport,
        Some(McpTransport::Stdio {
            command: "npx".to_string(),
            args: vec!["-y".to_string(), "@mcp/fs".to_string()],
        })
    );
    assert_eq!(fs.timeout_ms, Some(60_000));
    assert!(fs.invalid_reason.is_none());
    // raw keeps the full entry (the basis for faithful edit write-back)
    assert_eq!(fs.raw.get("env"), Some(&json!({"A": "1"})));
    let remote = servers.iter().find(|s| s.name == "remote").expect("remote");
    assert_eq!(
        remote.transport,
        Some(McpTransport::Remote {
            url: "https://mcp.example.com/mcp".to_string()
        })
    );
    assert_eq!(remote.timeout_ms, None);
}

#[test]
fn invalid_entries_kept_with_reason() {
    let raw = r#"{"mcpServers": {
        "good": {"command": "srv"},
        "nocmd": {"args": []},
        "empty": {"command": "  "},
        "badurl": {"url": "ftp://x"},
        "sse": {"type": "sse", "url": "https://x.example/sse"},
        "unknowntype": {"type": "grpc", "url": "https://x.example/mcp"}
    }}"#;
    let servers = parse_servers(raw, McpSource::Project);
    // Invalid entries are kept (the page annotates the reason, the dialog can
    // fix them), no longer silently dropped
    assert_eq!(servers.len(), 6);
    let good = servers.iter().find(|s| s.name == "good").expect("good");
    assert!(good.invalid_reason.is_none());
    for name in ["nocmd", "empty"] {
        let server = servers.iter().find(|s| s.name == name).expect(name);
        assert!(
            server.invalid_reason.is_some(),
            "{name} should have an invalid reason"
        );
        assert!(server.transport.is_none());
    }
    let badurl = servers.iter().find(|s| s.name == "badurl").expect("badurl");
    assert!(
        badurl
            .invalid_reason
            .as_deref()
            .is_some_and(|r| r.contains("http"))
    );
    let sse = servers.iter().find(|s| s.name == "sse").expect("sse");
    assert!(
        sse.invalid_reason
            .as_deref()
            .is_some_and(|r| r.contains("SSE"))
    );
    let unknown = servers
        .iter()
        .find(|s| s.name == "unknowntype")
        .expect("unknowntype");
    assert!(
        unknown
            .invalid_reason
            .as_deref()
            .is_some_and(|r| r.contains("grpc"))
    );
    assert!(parse_servers("not json", McpSource::User).is_empty());
    assert!(parse_servers("{}", McpSource::User).is_empty());
}

#[test]
fn parse_disabled_flag() {
    let raw = r#"{"mcpServers": {
        "on": {"command": "srv"},
        "off": {"command": "srv", "disabled": true},
        "explicit": {"command": "srv", "disabled": false}
    }}"#;
    let servers = parse_servers(raw, McpSource::User);
    let off = servers.iter().find(|s| s.name == "off").expect("off");
    assert!(off.disabled);
    for name in ["on", "explicit"] {
        let server = servers.iter().find(|s| s.name == name).expect(name);
        assert!(!server.disabled);
    }
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

// ---------- mcp.json writes ----------

/// Isolated temp directory per test case (cleaned up afterwards)
fn temp_mcp_path(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pig-mcp-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("mcp.json")
}

fn cleanup(path: &std::path::Path) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn upsert_disable_delete_roundtrip() {
    let path = temp_mcp_path("roundtrip");
    // Missing file → created (with parent directories), pretty-printed plus a
    // trailing newline
    upsert_mcp_server(
        &path,
        "fs",
        json!({"command": "npx", "args": ["-y", "@mcp/fs"]}),
    )
    .unwrap();
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(raw.ends_with('\n'));
    let servers = parse_servers(&raw, McpSource::User);
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0].name, "fs");
    assert!(!servers[0].disabled);

    // Disable / enable: write disabled:true, then remove the key
    set_mcp_disabled(&path, "fs", true).unwrap();
    let servers = parse_servers(&std::fs::read_to_string(&path).unwrap(), McpSource::User);
    assert!(servers[0].disabled);
    assert_eq!(servers[0].raw.get("disabled"), Some(&json!(true)));
    set_mcp_disabled(&path, "fs", false).unwrap();
    let servers = parse_servers(&std::fs::read_to_string(&path).unwrap(), McpSource::User);
    assert!(!servers[0].disabled);
    assert_eq!(servers[0].raw.get("disabled"), None);

    // Append a remote entry plus delete the first one
    upsert_mcp_server(
        &path,
        "remote",
        json!({"url": "https://mcp.example.com/mcp"}),
    )
    .unwrap();
    let servers = parse_servers(&std::fs::read_to_string(&path).unwrap(), McpSource::User);
    assert_eq!(servers.len(), 2);
    delete_mcp_server(&path, "fs").unwrap();
    let servers = parse_servers(&std::fs::read_to_string(&path).unwrap(), McpSource::User);
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0].name, "remote");

    // Deleting a missing entry doesn't error
    delete_mcp_server(&path, "missing").unwrap();
    cleanup(&path);
}

#[test]
fn upsert_preserves_file_level_fields() {
    let path = temp_mcp_path("preserve");
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&json!({
            "other": {"keep": true},
            "mcpServers": {"a": {"command": "srv"}}
        }))
        .unwrap(),
    )
    .unwrap();
    upsert_mcp_server(&path, "b", json!({"url": "https://b.example/mcp"})).unwrap();
    let file: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(file.get("other"), Some(&json!({"keep": true})));
    assert!(file["mcpServers"].get("a").is_some());
    assert!(file["mcpServers"].get("b").is_some());
    cleanup(&path);
}

#[test]
fn write_rejects_invalid_existing_file() {
    let path = temp_mcp_path("reject");
    std::fs::write(&path, "not json").unwrap();
    let err = upsert_mcp_server(&path, "a", json!({"command": "srv"})).unwrap_err();
    assert!(err.to_string().contains("拒绝改写"));
    // Content stays untouched
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "not json");
    cleanup(&path);
}

#[test]
fn object_json_parsing() {
    assert!(parse_object_json("", "env vars").unwrap().is_empty());
    assert!(parse_object_json("  {}  ", "env vars").unwrap().is_empty());
    let map = parse_object_json(r#"{"A": "1", "B": "2"}"#, "env vars").unwrap();
    assert_eq!(map.len(), 2);
    assert_eq!(map.get("A"), Some(&json!("1")));
    assert!(parse_object_json(r#"["a"]"#, "env vars").is_err());
    assert!(parse_object_json("{bad", "env vars").is_err());
    let err = parse_object_json("42", "headers").unwrap_err();
    assert!(
        err.contains("headers"),
        "error message should carry the field label: {err}"
    );
}

#[test]
fn pretty_object_shapes() {
    assert_eq!(pretty_object(None), "{}");
    assert_eq!(pretty_object(Some(&json!({}))), "{}");
    assert_eq!(pretty_object(Some(&json!("x"))), "{}");
    assert_eq!(
        pretty_object(Some(&json!({"A": "1"}))),
        "{\n  \"A\": \"1\"\n}"
    );
}
