use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use nsh_core::{ApproxTextEngine, MountBatch};
use nsh_engine::{App, Bundle, Engine, EngineConfig, MountSink};
use pig_protocol::{
    Event, Op, PluginCapabilityRequest, PluginContextUsage, PluginContributions, PluginHostContext,
    PluginPackageInfo, PluginScope, PluginServiceSnapshot, PluginSessionProjection,
    PluginSessionStats, PluginSource, PluginUiContribution,
};
use serde_json::{Value as JsonValue, json};

use super::*;
use crate::plugin_system::manager::PluginInstance;

const INSTANCE: &str = "moon:digest:user:default";
const CONTRIBUTION: &str = "panel";

struct Fixture {
    manager: PluginSystemManager,
    ops: async_channel::Receiver<Op>,
    directory: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        detach_manager(&self.manager);
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

struct Client {
    descriptor: SurfaceDescriptor,
    events: Receiver<JsonValue>,
    _engine: Engine,
}

impl Client {
    fn next(&self) -> JsonValue {
        self.events
            .recv_timeout(Duration::from_secs(5))
            .expect("plugin HostChannel reply")
    }
}

struct Sink;
impl MountSink for Sink {
    fn submit(&self, _: MountBatch) {}
}

fn receive<T>(receiver: &async_channel::Receiver<T>) -> T {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(value) = receiver.try_recv() {
            return value;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "core operation timed out"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn fixture() -> Fixture {
    let directory = std::env::temp_dir().join(format!(
        "pig-surface-host-{}-{}",
        std::process::id(),
        next_event_id()
    ));
    let (ops_tx, ops) = async_channel::bounded(128);
    let manager =
        PluginSystemManager::new(directory.clone(), ops_tx, PathBuf::from("C:/workspace"));
    let ui: PluginUiContribution = serde_json::from_value(json!({
        "id": CONTRIBUTION,
        "slot": "status",
        "kind": "popover",
        "label": {"en": "Panel"},
        "surface": "Panel",
        "choices": [],
    }))
    .unwrap();
    let package = PluginPackageInfo {
        id: "moon".into(),
        name: "Moon".into(),
        version: "1".into(),
        digest: "digest".into(),
        content_digest: "content".into(),
        root: directory.join("assets"),
        source: PluginSource::User,
        enabled: true,
        ui_entry: Some("App.tsx".into()),
        component_entry: None,
        backend: None,
        contributions: PluginContributions {
            ui: vec![ui],
            ..PluginContributions::default()
        },
        capabilities: vec![
            PluginCapabilityRequest {
                name: "ui.add".into(),
                targets: vec!["status".into()],
            },
            PluginCapabilityRequest {
                name: "session.read".into(),
                targets: vec!["*".into()],
            },
            PluginCapabilityRequest {
                name: "navigation.open".into(),
                targets: vec!["main".into()],
            },
        ],
    };
    let mut instance = PluginInstance::new(package, HostChannel::new(), 1, 1, 0);
    instance.state = PluginLifecycleState::Active;
    manager
        .inner
        .lock()
        .expect("plugin manager lock")
        .instances
        .insert(INSTANCE.into(), instance);
    Fixture {
        manager,
        ops,
        directory,
    }
}

fn mount_client(fixture: &Fixture, session: &str, calls: Vec<(&str, JsonValue, &str)>) -> Client {
    mount_client_with_revision(fixture, session, calls, 0)
}

fn mount_client_with_revision(
    fixture: &Fixture,
    session: &str,
    calls: Vec<(&str, JsonValue, &str)>,
    revision_offset: u64,
) -> Client {
    let registration: JsonValue = serde_json::from_str(
        &fixture
            .manager
            .mount_surface(
                INSTANCE,
                CONTRIBUTION,
                PluginHostContext {
                    session_id: Some(session.into()),
                    turn_id: None,
                },
            )
            .expect("mount Surface"),
    )
    .unwrap();
    let host = HostChannel::new();
    let mount = attach(registration["surface"].as_str().unwrap(), host.clone()).unwrap();
    let (events_tx, events) = channel();
    let replies = events_tx.clone();
    host.handle("test.reply", move |mut call| {
        let _ = replies.send(call.take_data());
    });
    host.handle("test.event", move |mut call| {
        let _ = events_tx.send(call.take_data());
    });
    let source = format!(
        r#"
        import {{ host }} from 'nsh';
        const params = host.params.plugin;
        host.on('pig.host', message => host.post('test.event', message));
        async function run() {{
          for (const [method, payload, requestId] of {}) {{
            const response = await host.call('pig.host', {{
              apiVersion: 1,
              instance: params.instance,
              surface: params.surface,
              contribution: params.contribution,
              revision: params.revision + {revision_offset},
              requestId,
              method,
              payload,
            }});
            host.post('test.reply', response);
          }}
        }}
        run().catch(error => host.post('test.event', {{ kind: 'call.error', message: String(error) }}));
        "#,
        serde_json::to_string(&calls).unwrap()
    );
    let mut bundle = Bundle::source(source);
    bundle.policy.declared =
        Some(nsh_core::access::Capabilities::from_json(&json!({"host": true})).unwrap());
    let mut config = EngineConfig::new(App::Js(bundle));
    config.host = host;
    config.data_dir = Some(fixture.directory.join("engine"));
    let engine = Engine::start(config, || ApproxTextEngine, Sink).expect("start Host API client");
    Client {
        descriptor: mount.descriptor,
        events,
        _engine: engine,
    }
}

fn session_request(ops: &async_channel::Receiver<Op>, session: &str) -> String {
    match receive(ops) {
        Op::QueryPlugin {
            request_id,
            session_id,
            ..
        } => {
            assert_eq!(session_id.as_deref(), Some(session));
            request_id
        }
        other => panic!("unexpected core operation: {other:?}"),
    }
}

fn snapshot(session: &str, used: u64) -> PluginServiceSnapshot {
    PluginServiceSnapshot {
        generation: 1,
        locale: "zh-CN".into(),
        scope: PluginScope::User,
        workspaces: Vec::new(),
        session: Some(PluginSessionProjection {
            id: session.into(),
            cwd: PathBuf::from("C:/workspace"),
            stats: PluginSessionStats {
                turns: 1,
                usage: Some(PluginContextUsage {
                    used,
                    total: 128_000,
                    cache_read_total: 0,
                    input_total: 0,
                }),
                ..PluginSessionStats::default()
            },
            turns: Vec::new(),
        }),
        skills: Default::default(),
        mcp: Default::default(),
    }
}

fn answer_session(fixture: &Fixture, request_id: String, session: &str, used: u64) {
    fixture.manager.handle_core_event(&Event::PluginServices {
        request_id,
        package: "moon".into(),
        digest: "digest".into(),
        result: Ok(snapshot(session, used)),
    });
}

#[test]
fn surface_channels_bind_sessions_and_cancel_their_own_subscriptions() {
    let fixture = fixture();
    let first = mount_client(
        &fixture,
        "session-a",
        vec![
            ("session.snapshot", json!({}), "snapshot"),
            ("session.subscribe", json!({}), "subscribe"),
        ],
    );
    let query = session_request(&fixture.ops, "session-a");
    answer_session(&fixture, query, "session-a", 10);
    let snapshot_reply = first.next();
    assert_eq!(snapshot_reply["result"]["Ok"]["session"]["id"], "session-a");
    let query = session_request(&fixture.ops, "session-a");
    answer_session(&fixture, query, "session-a", 11);
    let first_subscription = first.next();
    let second = mount_client(
        &fixture,
        "session-b",
        vec![("session.subscribe", json!({}), "subscribe")],
    );
    let query = session_request(&fixture.ops, "session-b");
    answer_session(&fixture, query, "session-b", 20);
    let second_subscription = second.next();
    assert_eq!(first_subscription["surface"], first.descriptor.surface);
    assert_eq!(second_subscription["surface"], second.descriptor.surface);
    assert_eq!(
        first_subscription["result"]["Ok"]["session"]["id"],
        "session-a"
    );
    assert_eq!(
        second_subscription["result"]["Ok"]["session"]["id"],
        "session-b"
    );
    assert_ne!(first.descriptor.surface, second.descriptor.surface);
    fixture.manager.execute_surface(SurfaceWork {
        request: PluginHostRequest {
            api_version: PLUGIN_HOST_API_VERSION,
            instance: first.descriptor.instance.clone(),
            surface: first.descriptor.surface.clone(),
            contribution: first.descriptor.contribution.clone(),
            revision: first.descriptor.revision,
            request_id: "wrong-unsubscribe".into(),
            method: PluginHostMethod::SessionUnsubscribe,
            payload: json!({"subscriptionId": "not-the-subscription"}),
        },
        reply: None,
    });
    assert_eq!(
        fixture.manager.inner.lock().unwrap().surfaces[&first.descriptor.surface]
            .subscription
            .as_ref()
            .map(|subscription| subscription.id.as_str()),
        Some("subscribe")
    );

    fixture.manager.handle_core_event(&Event::ContextUsage {
        session_id: "session-a".into(),
        used: 42,
        total: 128_000,
        cache_read_total: 0,
        input_total: 0,
    });
    fixture.manager.refresh_subscriptions();
    let query = session_request(&fixture.ops, "session-a");
    fixture.manager.handle_core_event(&Event::ContextUsage {
        session_id: "session-a".into(),
        used: 43,
        total: 128_000,
        cache_read_total: 0,
        input_total: 0,
    });
    fixture.manager.refresh_subscriptions();
    assert!(fixture.ops.try_recv().is_err());
    answer_session(&fixture, query, "session-a", 42);
    let update = first.next();
    assert_eq!(update["kind"], "session.snapshot");
    assert_eq!(update["surface"], first.descriptor.surface);
    assert_eq!(update["session"]["id"], "session-a");
    assert_eq!(update["session"]["stats"]["usage"]["used"], 42);
    assert!(
        update["revision"].as_u64().unwrap()
            > first_subscription["result"]["Ok"]["revision"]
                .as_u64()
                .unwrap()
    );
    assert!(second.events.try_recv().is_err());

    fixture.manager.refresh_subscriptions();
    let query = session_request(&fixture.ops, "session-a");
    answer_session(&fixture, query, "session-a", 43);
    assert_eq!(first.next()["session"]["stats"]["usage"]["used"], 43);
    assert!(fixture.manager.inner.lock().unwrap().surfaces.len() == 2);
    fixture.manager.stop(INSTANCE);
    assert!(fixture.manager.inner.lock().unwrap().surfaces.is_empty());
    assert!(
        fixture
            .manager
            .inner
            .lock()
            .unwrap()
            .surface_requests
            .is_empty()
    );
    assert_eq!(first.next()["kind"], "surface.disposed");
    assert_eq!(second.next()["kind"], "surface.disposed");
    fixture.manager.handle_core_event(&Event::ContextUsage {
        session_id: "session-a".into(),
        used: 99,
        total: 128_000,
        cache_read_total: 0,
        input_total: 0,
    });
    fixture.manager.refresh_subscriptions();
    while let Ok(operation) = fixture.ops.try_recv() {
        assert!(
            matches!(operation, Op::SyncPluginRuntimeContributions { .. }),
            "unexpected session refresh after Surface disposal"
        );
    }
    assert!(first.events.try_recv().is_err());
    assert!(second.events.try_recv().is_err());
}

#[test]
fn storage_navigation_and_stale_identity_use_the_same_surface_contract() {
    let fixture = fixture();
    let client = mount_client(
        &fixture,
        "session-a",
        vec![
            ("storage.read", json!(null), "read-before"),
            ("storage.write", json!(null), "write-null"),
            ("storage.read", json!(null), "read-null"),
            ("storage.write", json!({"owner": "surface"}), "write-value"),
            ("storage.read", json!(null), "read-after"),
            ("navigation.open", json!({"pageId": "main"}), "navigate"),
            ("overlay.close", json!(null), "close-overlay"),
        ],
    );
    assert_eq!(client.next()["result"]["Ok"], JsonValue::Null);
    assert_eq!(client.next()["result"]["Ok"], JsonValue::Null);
    assert_eq!(client.next()["result"]["Ok"], JsonValue::Null);
    assert_eq!(client.next()["result"]["Ok"], JsonValue::Null);
    assert_eq!(client.next()["result"]["Ok"]["owner"], "surface");
    assert_eq!(client.next()["result"]["Ok"]["pageId"], "main");
    let action: JsonValue = serde_json::from_str(&receive(&fixture.manager.events())).unwrap();
    assert_eq!(action["kind"], "host.action");
    assert_eq!(action["body"]["pageId"], "main");
    assert_eq!(client.next()["result"]["Ok"], JsonValue::Null);
    let overlay: JsonValue = serde_json::from_str(&receive(&fixture.manager.events())).unwrap();
    assert_eq!(overlay["body"]["name"], "overlay.close");
    assert_eq!(overlay["body"]["surface"], client.descriptor.surface);

    let stale = mount_client_with_revision(
        &fixture,
        "session-a",
        vec![("session.snapshot", json!({}), "stale")],
        1,
    );
    assert_eq!(stale.next()["result"]["Err"]["code"], "stale_revision");
    assert!(fixture.ops.try_recv().is_err());
    fixture.manager.stop(INSTANCE);
}

#[test]
fn disable_rejects_a_late_initial_snapshot() {
    let fixture = fixture();
    let client = mount_client(
        &fixture,
        "session-a",
        vec![("session.subscribe", json!({}), "subscribe")],
    );
    let query = session_request(&fixture.ops, "session-a");
    fixture.manager.stop(INSTANCE);
    let mut cancelled = false;
    let mut disposed = false;
    for _ in 0..2 {
        let event = client.next();
        cancelled |=
            event["requestId"] == "subscribe" && event["result"]["Err"]["code"] == "cancelled";
        disposed |= event["kind"] == "surface.disposed";
    }
    assert!(cancelled && disposed);
    answer_session(&fixture, query, "session-a", 100);
    assert!(client.events.try_recv().is_err());
    assert!(fixture.manager.inner.lock().unwrap().surfaces.is_empty());
    assert!(
        fixture
            .manager
            .inner
            .lock()
            .unwrap()
            .surface_requests
            .is_empty()
    );
}

#[test]
fn a_pending_subscription_is_unique_and_can_be_cancelled() {
    let fixture = fixture();
    let client = mount_client(&fixture, "session-a", Vec::new());
    let request = |method, payload: JsonValue, request_id: &str| PluginHostRequest {
        api_version: PLUGIN_HOST_API_VERSION,
        instance: client.descriptor.instance.clone(),
        surface: client.descriptor.surface.clone(),
        contribution: client.descriptor.contribution.clone(),
        revision: client.descriptor.revision,
        request_id: request_id.into(),
        method,
        payload,
    };
    fixture.manager.execute_surface(SurfaceWork {
        request: request(PluginHostMethod::SessionSubscribe, json!({}), "subscribe"),
        reply: None,
    });
    let query = session_request(&fixture.ops, "session-a");
    fixture.manager.execute_surface(SurfaceWork {
        request: request(PluginHostMethod::SessionSubscribe, json!({}), "duplicate"),
        reply: None,
    });
    assert!(fixture.ops.try_recv().is_err());
    fixture.manager.execute_surface(SurfaceWork {
        request: request(
            PluginHostMethod::SessionUnsubscribe,
            json!({"subscriptionId": "subscribe"}),
            "cancel",
        ),
        reply: None,
    });
    assert!(
        fixture
            .manager
            .inner
            .lock()
            .unwrap()
            .surface_requests
            .is_empty()
    );
    answer_session(&fixture, query, "session-a", 100);
    assert!(client.events.try_recv().is_err());
    assert!(
        fixture
            .manager
            .inner
            .lock()
            .unwrap()
            .surfaces
            .values()
            .all(|surface| surface.subscription.is_none())
    );
    fixture.manager.stop(INSTANCE);
}
