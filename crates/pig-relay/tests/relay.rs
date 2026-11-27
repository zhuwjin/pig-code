use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pig_daemon::{
    DaemonClient, DaemonConfig, DaemonEndpoint, DaemonMessage, RemoteConnectConfig,
    list_relay_devices,
};
use pig_protocol::{
    ConversationItem, Event, ExecMode, MessageDelivery, Op, RelayConnectionStatus, RelayDeviceInfo,
};
use pig_provider::mock;
use pig_relay::RelayServerConfig;
use serde_json::{Value, json};
use tokio::sync::oneshot;
use uuid::Uuid;

struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn new_temp_dir(label: &str) -> TempDirGuard {
    let dir = std::env::temp_dir().join(format!(
        "pig-relay-e2e-{label}-{}-{}",
        std::process::id(),
        Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    TempDirGuard(dir)
}

async fn spawn_test_daemon(root: &Path, mock_port: u16) -> (DaemonEndpoint, PathBuf, PathBuf) {
    let workspace = root.join("workspace");
    let data_dir = root.join("data");
    std::fs::create_dir_all(&workspace).expect("create workspace");
    std::fs::create_dir_all(&data_dir).expect("create data dir");
    std::fs::write(
        workspace.join(mock::MOCK_FILE_NAME),
        mock::MOCK_FILE_CONTENT,
    )
    .expect("write mock fixture");
    let config_path = root.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"default_provider = "mock"
default_model = "mock-model"

[[providers]]
id = "mock"
name = "Mock"
base_url = "http://127.0.0.1:{mock_port}/v1"
api_key = "mock-key"
api_format = "OpenAiChat"
enabled = true

[[providers.models]]
id = "mock-model"
context_window = 128000
max_output_tokens = 8192
"#
        ),
    )
    .expect("write daemon config");

    let (ready_tx, ready_rx) = oneshot::channel();
    let cfg = DaemonConfig {
        data_dir: data_dir.clone(),
        cwd: workspace.clone(),
        config_path: Some(config_path),
        selftest: true,
    };
    tokio::spawn(async move {
        let _ = pig_daemon::serve_with_ready(cfg, Some(ready_tx)).await;
    });
    let endpoint = tokio::time::timeout(Duration::from_secs(10), ready_rx)
        .await
        .expect("daemon start timeout")
        .expect("daemon ready");
    (endpoint, workspace, data_dir)
}

async fn call_rpc(client: &DaemonClient, method: &str, params: Value) -> Result<Value, String> {
    let id = Uuid::new_v4().to_string();
    client.send_rpc(id.clone(), method, params)?;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for RPC `{method}`"
        );
        let msg = tokio::time::timeout(Duration::from_secs(2), client.recv())
            .await
            .map_err(|_| format!("timeout waiting for rpc `{method}`"))?
            .map_err(|error| error.to_string())?;
        if let DaemonMessage::RpcResult {
            id: resp_id,
            result,
            error,
        } = msg
            && resp_id == id
        {
            if let Some(err) = error {
                return Err(err);
            }
            return Ok(result.unwrap_or(Value::Null));
        }
    }
}

async fn wait_relay_connected(client: &DaemonClient) -> RelayConnectionStatus {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let value = call_rpc(client, "relay.status", json!({}))
            .await
            .expect("relay.status rpc");
        let status: RelayConnectionStatus =
            serde_json::from_value(value).expect("parse relay status");
        if status.connected {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "daemon did not connect to relay in time: {status:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_device_discovery_remote_session_cursor_resync_revocation_and_privacy() {
    let relay_dir = new_temp_dir("relay");
    let dev_a_dir = new_temp_dir("dev-a");
    let dev_b_dir = new_temp_dir("dev-b");

    // 1. Start standalone pig-relay server.
    let (relay_tx, relay_rx) = oneshot::channel();
    let relay_data_dir = relay_dir.0.clone();
    tokio::spawn(async move {
        let _ = pig_relay::serve_with_ready(
            RelayServerConfig {
                bind: "127.0.0.1:0".parse().unwrap(),
                data_dir: relay_data_dir,
                public_url: None,
                allow_registration: true,
            },
            Some(relay_tx),
        )
        .await;
    });
    let relay_addr = tokio::time::timeout(Duration::from_secs(10), relay_rx)
        .await
        .expect("relay ready timeout")
        .expect("relay bound address");
    let relay_url = format!("http://{relay_addr}");

    // Verify embedded Web remote client & health endpoint.
    let http = reqwest::Client::new();
    let health: Value = http
        .get(format!("{relay_url}/health"))
        .send()
        .await
        .expect("GET /health")
        .json()
        .await
        .expect("parse health JSON");
    assert_eq!(health["ok"], true);
    let html = http
        .get(format!("{relay_url}/"))
        .send()
        .await
        .expect("GET /")
        .text()
        .await
        .expect("read index HTML");
    assert!(html.contains("Pig Code Remote"));
    assert!(html.contains("/ws/client/"));

    // 2. Start two isolated pig-daemon instances (Device A and Device B).
    let mock_port = mock::start_mock_server();
    let (ep_a, _ws_a, data_a) = spawn_test_daemon(&dev_a_dir.0, mock_port).await;
    let (ep_b, ws_b, data_b) = spawn_test_daemon(&dev_b_dir.0, mock_port).await;

    let client_a = DaemonClient::connect(ep_a, None)
        .await
        .expect("connect local daemon A");
    let client_b = DaemonClient::connect(ep_b, None)
        .await
        .expect("connect local daemon B");

    // 3. Device A registers account + connects outbound tunnel.
    let reg_val = call_rpc(
        &client_a,
        "relay.register",
        json!({
            "relay_url": relay_url,
            "username": "alice",
            "password": "password-1234",
            "device_name": "Laptop-A",
        }),
    )
    .await
    .expect("device A relay.register");
    let status_a = wait_relay_connected(&client_a).await;
    let session_token_a = status_a
        .session_token
        .clone()
        .expect("session token from register");
    let device_a_id = status_a.device_id.clone().expect("device A id");
    assert_eq!(reg_val["username"], "alice");

    // 4. Device B pairs using the Device Code flow approved by Device A.
    let challenge = call_rpc(
        &client_b,
        "relay.device_start",
        json!({
            "relay_url": relay_url,
            "device_name": "Workstation-B",
        }),
    )
    .await
    .expect("device B device_start");
    let user_code = challenge["user_code"].as_str().expect("user_code");
    let device_code = challenge["device_code"].as_str().expect("device_code");

    let pending = call_rpc(
        &client_b,
        "relay.device_poll",
        json!({
            "relay_url": relay_url,
            "device_code": device_code,
            "device_name": "Workstation-B",
        }),
    )
    .await
    .expect("device B poll pending");
    assert_eq!(pending["status"], "pending");

    let approved_flag = call_rpc(
        &client_a,
        "relay.approve_code",
        json!({ "user_code": user_code }),
    )
    .await
    .expect("device A approve_code");
    assert_eq!(approved_flag, Value::Bool(true));

    let poll_ok = call_rpc(
        &client_b,
        "relay.device_poll",
        json!({
            "relay_url": relay_url,
            "device_code": device_code,
            "device_name": "Workstation-B",
        }),
    )
    .await
    .expect("device B poll approved");
    assert_eq!(poll_ok["status"], "approved");

    let status_b = wait_relay_connected(&client_b).await;
    let device_b_id = status_b.device_id.clone().expect("device B id");
    assert_ne!(device_a_id, device_b_id);

    // 5. Verify cross-device discovery reports both devices online.
    let devices: Vec<RelayDeviceInfo> = list_relay_devices(&relay_url, &session_token_a)
        .await
        .expect("list relay devices");
    assert_eq!(devices.len(), 2);
    assert!(devices.iter().all(|d| d.online));
    assert!(devices.iter().any(|d| d.device_id == device_a_id && d.name == "Laptop-A"));
    assert!(devices.iter().any(|d| d.device_id == device_b_id && d.name == "Workstation-B"));

    // 6. Control Device B remotely through pig-relay (identical DaemonRequest/DaemonMessage protocol).
    let remote = DaemonClient::connect_remote(
        RemoteConnectConfig {
            relay_url: relay_url.clone(),
            session_token: session_token_a.clone(),
            device_id: device_b_id.clone(),
        },
        None,
        None,
    )
    .await
    .expect("connect remote to device B");

    // Remote RPC executes on Device B and returns Device B's data_dir & workspace.
    let remote_info = call_rpc(&remote, "daemon.info", json!({}))
        .await
        .expect("remote daemon.info");
    assert_eq!(
        remote_info["data_dir"].as_str(),
        Some(data_b.to_string_lossy().as_ref())
    );
    let remote_dir = call_rpc(
        &remote,
        "fs.list_directory",
        json!({ "path": ws_b.display().to_string(), "include_files": true }),
    )
    .await
    .expect("remote fs.list_directory");
    assert!(
        remote_dir["entries"]
            .as_array()
            .expect("entries array")
            .iter()
            .any(|e| e["name"] == mock::MOCK_FILE_NAME)
    );

    // Create a session on Device B using placeholder "." cwd (normalized to Device B's workspace).
    remote
        .send_op(Op::NewSession {
            cwd: PathBuf::from("."),
            provider_id: None,
            model_id: None,
            reasoning_level: None,
            permissions: Some(ExecMode::Auto.permissions()),
            plan_enabled: Some(false),
            bot_id: None,
            global: false,
        })
        .expect("send remote NewSession");

    let session_id = {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for SessionConfigured"
            );
            let msg = tokio::time::timeout(Duration::from_secs(2), remote.recv())
                .await
                .expect("recv timeout")
                .expect("stream open");
            if let DaemonMessage::Batch(batch) = msg
                && let Some((sid, cwd)) = batch.events.into_iter().find_map(|event| match event {
                    Event::SessionConfigured {
                        session_id, cwd, ..
                    } => Some((session_id, cwd)),
                    _ => None,
                })
            {
                assert_eq!(cwd, ws_b);
                break sid;
            }
        }
    };

    // Send a message to Device B via relay and stream the full assistant reply.
    remote
        .send_op(Op::SendMessage {
            session_id: session_id.clone(),
            content: "Please check the mock fixture file.".to_string(),
            files: Vec::new(),
            images: Vec::new(),
            permissions: ExecMode::Auto.permissions(),
            delivery: MessageDelivery::Queue,
        })
        .expect("send remote SendMessage");

    let mut saw_reply_text = false;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for TurnComplete over relay"
        );
        let msg = tokio::time::timeout(Duration::from_secs(2), remote.recv())
            .await
            .expect("recv timeout")
            .expect("remote stream open");
        if let DaemonMessage::Batch(batch) = msg {
            let mut completed = false;
            for event in batch.events {
                match event {
                    Event::ConversationUpdate {
                        item: ConversationItem::Markdown { text, .. },
                        ..
                    } => {
                        if text.contains(mock::MOCK_REPLY_MARKER) {
                            saw_reply_text = true;
                        }
                    }
                    Event::TurnComplete { .. } => {
                        completed = true;
                    }
                    _ => {}
                }
            }
            if completed {
                break;
            }
        }
    }
    assert!(saw_reply_text, "expected mock reply text streamed via relay");

    // 7. Disconnect & reconnect with saved EventCursor -> verify cursor replay / snapshot attach.
    let saved_cursor = remote.cursor().expect("recorded event cursor");
    drop(remote);

    let resumed = DaemonClient::connect_remote(
        RemoteConnectConfig {
            relay_url: relay_url.clone(),
            session_token: session_token_a.clone(),
            device_id: device_b_id.clone(),
        },
        Some(saved_cursor.clone()),
        None,
    )
    .await
    .expect("reconnect remote with cursor");
    let first_msg = tokio::time::timeout(Duration::from_secs(5), resumed.recv())
        .await
        .expect("subscription timeout")
        .expect("subscription message");
    match first_msg {
        DaemonMessage::Subscription {
            cursor,
            resync_required,
        } => {
            assert_eq!(cursor, saved_cursor);
            assert!(!resync_required);
        }
        other => panic!("expected Subscription with matching cursor, got {other:?}"),
    }
    resumed
        .send_op(Op::OpenSession {
            session_id: session_id.clone(),
        })
        .expect("open session after reconnect");
    let snapshot = loop {
        let msg = tokio::time::timeout(Duration::from_secs(10), resumed.recv())
            .await
            .expect("snapshot timeout")
            .expect("stream open");
        if let DaemonMessage::Snapshot(snapshot) = msg {
            break snapshot;
        }
    };
    assert_eq!(snapshot.session_id, session_id);
    assert!(snapshot.items.iter().any(|item| matches!(
        item,
        ConversationItem::Markdown { text, .. } if text.contains(mock::MOCK_REPLY_MARKER)
    )));
    drop(resumed);

    // 7b. Multi-device Bot & personal memory sync across Device A and Device B.
    let custom_bot = pig_protocol::BotProfile {
        bot_id: "cross-device-bot".to_string(),
        name: "Cross Device Reviewer".to_string(),
        avatar: None,
        description: "Follows user across devices".to_string(),
        system_prompt: "Focus on reliability and clear error handling.".to_string(),
        provider_id: None,
        model_id: None,
        reasoning_level: None,
        capabilities: pig_protocol::BotCapabilities::coding(),
        builtin: false,
        created_at_ms: 1_700_000_000_000,
        updated_at_ms: 1_700_000_000_100,
    };
    let upserted = call_rpc(&client_a, "bot.upsert", json!({ "bot": custom_bot }))
        .await
        .expect("bot.upsert on Device A");
    assert_eq!(upserted, Value::Bool(true));
    // Wait briefly for Device A core to persist the bot & create its personal memory directory.
    let bot_mem_a = data_a.join("memories").join("bots").join("cross-device-bot");
    let mem_deadline = Instant::now() + Duration::from_secs(5);
    while !bot_mem_a.exists() {
        assert!(Instant::now() < mem_deadline, "timed out waiting for bot memory dir on Device A");
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    let personal_memory_marker = "BOT_PERSONAL_MEMORY_PREF_RUST_EXPLICIT_ERRORS";
    std::fs::write(
        bot_mem_a.join("MEMORY.md"),
        format!("# Bot Personal Memory\n- {personal_memory_marker}\n"),
    )
    .expect("write bot personal memory on Device A");

    let _ = call_rpc(&client_a, "relay.sync_bots", json!({}))
        .await
        .expect("sync_bots on Device A");
    let _ = call_rpc(&client_b, "relay.sync_bots", json!({}))
        .await
        .expect("sync_bots on Device B");

    let bots_b: Vec<pig_protocol::BotProfile> = serde_json::from_value(
        call_rpc(&client_b, "bot.list", json!({}))
            .await
            .expect("bot.list on Device B"),
    )
    .expect("parse bot list on Device B");
    assert!(
        bots_b.iter().any(|b| b.bot_id == "cross-device-bot" && !b.capabilities.control_plane),
        "Device B must receive synced custom bot: {bots_b:?}"
    );
    let synced_mem_b = std::fs::read_to_string(
        data_b
            .join("memories")
            .join("bots")
            .join("cross-device-bot")
            .join("MEMORY.md"),
    )
    .expect("read synced bot personal memory on Device B");
    assert!(
        synced_mem_b.contains(personal_memory_marker),
        "Device B must receive synced bot personal memory: {synced_mem_b}"
    );

    // 8. Privacy boundary audit: relay.db must NEVER contain source code, tool output, conversation text, or API keys.
    let db_bytes = std::fs::read(relay_dir.0.join("relay.db")).expect("read relay.db");
    let db_lossy = String::from_utf8_lossy(&db_bytes);
    for forbidden in [
        mock::MOCK_FILE_CONTENT,
        mock::MOCK_REPLY_MARKER,
        "Please check the mock fixture file.",
        "mock-key",
    ] {
        assert!(
            !db_lossy.contains(forbidden),
            "relay.db leaked forbidden content `{forbidden}`"
        );
    }

    // 9. Revoke Device B -> tunnel disconnects and new remote connections are rejected.
    let revoked = call_rpc(
        &client_a,
        "relay.revoke_device",
        json!({ "device_id": device_b_id }),
    )
    .await
    .expect("revoke device B");
    assert_eq!(revoked, Value::Bool(true));

    let rejected = DaemonClient::connect_remote(
        RemoteConnectConfig {
            relay_url: relay_url.clone(),
            session_token: session_token_a,
            device_id: device_b_id,
        },
        None,
        None,
    )
    .await;
    assert!(rejected.is_err(), "revoked device must reject remote clients");

    // 10. Headless CLI login + live daemon `relay.reload` + logout.
    let headless_creds = pig_daemon::relay::login_headless(
        &data_b,
        &relay_url,
        "alice",
        "password-1234",
        Some("Headless-B"),
        false,
    )
    .await
    .expect("headless login");
    assert_eq!(headless_creds.device_name, "Headless-B");
    let _ = call_rpc(&client_b, "relay.reload", json!({}))
        .await
        .expect("relay.reload rpc");
    let reconnected = wait_relay_connected(&client_b).await;
    assert_eq!(reconnected.device_name.as_deref(), Some("Headless-B"));
    pig_daemon::relay::logout_headless(&data_b)
        .await
        .expect("headless logout");
    let after_logout: RelayConnectionStatus = serde_json::from_value(
        call_rpc(&client_b, "relay.reload", json!({}))
            .await
            .expect("relay.reload after logout"),
    )
    .expect("parse status after logout");
    assert!(!after_logout.configured);
}
