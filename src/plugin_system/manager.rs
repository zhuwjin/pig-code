use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use nsh_core::{ApproxTextEngine, EventKind, EventPayload, MountBatch, PressEvent};
use nsh_engine::{App, Engine, EngineConfig, HostChannel, MountSink};
use pig_protocol::{
    CoreError, Event, PluginPackageInfo, PluginSnapshot, PluginToolResult, PluginToolsSelection,
};

use super::loader::{installed_package_path, load_verified};
use super::mount_projector::project_batch;

#[derive(Clone)]
pub struct PluginSystemManager {
    inner: Arc<Mutex<Inner>>,
    events: async_channel::Sender<String>,
    receiver: async_channel::Receiver<String>,
}

struct Inner {
    data_dir: PathBuf,
    trust_root: PathBuf,
    agent: async_channel::Sender<pig_protocol::Op>,
    instances: BTreeMap<String, PluginInstance>,
    pending: BTreeMap<String, PendingCall>,
    input_counts: BTreeMap<(String, String), u32>,
    epoch: u64,
}

struct PluginInstance {
    package: PluginPackageInfo,
    host: HostChannel,
    engine: Option<Engine>,
}

struct PendingCall {
    session_id: String,
    package: String,
    digest: String,
    selection: bool,
}

impl PluginSystemManager {
    pub fn new(data_dir: PathBuf, agent: async_channel::Sender<pig_protocol::Op>) -> Self {
        let (events, receiver) = async_channel::bounded(256);
        Self {
            inner: Arc::new(Mutex::new(Inner {
                trust_root: data_dir.join("plugin-root.pub"),
                data_dir,
                agent,
                instances: BTreeMap::new(),
                pending: BTreeMap::new(),
                input_counts: BTreeMap::new(),
                epoch: 0,
            })),
            events,
            receiver,
        }
    }

    pub fn events(&self) -> async_channel::Receiver<String> {
        self.receiver.clone()
    }

    pub fn install(&self, package: &Path, root: &Path) -> Result<serde_json::Value, String> {
        let (data_dir, trust_root, agent) = {
            let inner = self.inner.lock().expect("plugin manager lock");
            (
                inner.data_dir.clone(),
                inner.trust_root.clone(),
                inner.agent.clone(),
            )
        };
        let verified = load_verified(package, root, &data_dir)?;
        std::fs::copy(root, &trust_root)
            .map_err(|error| format!("store plugin root key: {error}"))?;
        agent
            .try_send(pig_protocol::Op::SyncPlugins {
                packages: super::loader::installed_sources(&data_dir),
            })
            .map_err(|error| error.to_string())?;
        Ok(serde_json::json!({
            "id": verified.manifest.id,
            "digest": verified.digest,
            "root": verified.assets,
        }))
    }

    pub fn handle_core_event(&self, event: &Event) {
        match event {
            Event::PluginSnapshot { snapshot } => self.reconcile(snapshot),
            Event::PluginToolCall { call } => {
                let package = self.find_package(&call.package, &call.digest);
                if package.is_some() {
                    let call = call.clone();
                    let pending = PendingCall {
                        session_id: call.session_id.clone(),
                        package: call.package.clone(),
                        digest: call.digest.clone(),
                        selection: false,
                    };
                    let mut inner = self.inner.lock().expect("plugin manager lock");
                    inner.pending.insert(call.request_id.clone(), pending);
                    if let Some(instance) = inner
                        .instances
                        .get(&instance_key(&call.package, &call.digest))
                    {
                        let _ = instance.host.send(
                            "tools.call",
                            serde_json::to_value(&call).unwrap_or_default(),
                        );
                    }
                }
            }
            Event::PluginToolsQuery { query } => {
                let pending = PendingCall {
                    session_id: query.session_id.clone(),
                    package: query.package.clone(),
                    digest: query.digest.clone(),
                    selection: true,
                };
                let mut inner = self.inner.lock().expect("plugin manager lock");
                inner.pending.insert(query.request_id.clone(), pending);
                if let Some(instance) = inner
                    .instances
                    .get(&instance_key(&query.package, &query.digest))
                {
                    let _ = instance.host.send(
                        "tools.select",
                        serde_json::to_value(query).unwrap_or_default(),
                    );
                }
            }
            _ => {}
        }
    }

    fn find_package(&self, id: &str, digest: &str) -> Option<PluginPackageInfo> {
        self.inner
            .lock()
            .expect("plugin manager lock")
            .instances
            .get(&instance_key(id, digest))
            .map(|instance| instance.package.clone())
    }

    fn reconcile(&self, snapshot: &PluginSnapshot) {
        let desired: BTreeMap<String, PluginPackageInfo> = snapshot
            .packages
            .iter()
            .filter(|package| package.enabled)
            .map(|package| (instance_key(&package.id, &package.digest), package.clone()))
            .collect();
        let existing: Vec<String> = self
            .inner
            .lock()
            .expect("plugin manager lock")
            .instances
            .keys()
            .cloned()
            .collect();
        for key in existing {
            if !desired.contains_key(&key) {
                self.stop(&key);
            }
        }
        for package in snapshot.packages.clone() {
            let key = instance_key(&package.id, &package.digest);
            self.emit(serde_json::json!({
                "kind": if package.enabled { "ui.patch" } else { "ui.reset" },
                "instance": key,
                "package": package,
            }));
            if package.enabled {
                self.start(package);
            }
        }
    }

    fn start(&self, package: PluginPackageInfo) {
        let key = instance_key(&package.id, &package.digest);
        if self
            .inner
            .lock()
            .expect("plugin manager lock")
            .instances
            .contains_key(&key)
        {
            return;
        }
        let needs_engine = package
            .contributions
            .ui
            .iter()
            .any(|item| item.export.is_some())
            || !package.contributions.tools.is_empty();
        if !needs_engine {
            return;
        }
        let (root, data_dir, epoch) = {
            let mut inner = self.inner.lock().expect("plugin manager lock");
            inner.epoch += 1;
            let package_root = package.root.join(".pig-root.pub");
            (
                if package_root.is_file() {
                    package_root
                } else {
                    inner.trust_root.clone()
                },
                inner.data_dir.clone(),
                inner.epoch,
            )
        };
        match load_verified(&installed_package_path(&package.root), &root, &data_dir) {
            Ok(verified) => {
                let host = HostChannel::new();
                self.configure_host(&host, package.clone());
                let sink = UiSink {
                    instance: key.clone(),
                    epoch,
                    sequence: AtomicU64::new(0),
                    previous: Mutex::new("{}".into()),
                    events: self.events.clone(),
                };
                let mut config = EngineConfig::new(App::Js(verified.bundle));
                config.host = host.clone();
                config.ui_sets = vec![nsh_base::registry()];
                config.data_dir = Some(data_dir.join("instances").join(&key));
                match Engine::start(config, || ApproxTextEngine, sink) {
                    Ok(engine) => {
                        self.inner
                            .lock()
                            .expect("plugin manager lock")
                            .instances
                            .insert(
                                key.clone(),
                                PluginInstance {
                                    package,
                                    host,
                                    engine: Some(engine),
                                },
                            );
                        self.emit(serde_json::json!({"kind":"runtime","instance":key,"state":"active","epoch":epoch}));
                    }
                    Err(error) => self.emit(serde_json::json!({
                        "kind":"runtime","instance":key,"state":"failed","message":error.to_string()
                    })),
                }
            }
            Err(error) => self.emit(serde_json::json!({
                "kind":"runtime","instance":key,"state":"failed","message":error
            })),
        }
    }

    fn stop(&self, key: &str) {
        let instance = self
            .inner
            .lock()
            .expect("plugin manager lock")
            .instances
            .remove(key);
        if let Some(instance) = instance {
            drop(instance.engine);
        }
        self.emit(serde_json::json!({"kind":"ui.reset","instance":key}));
        self.emit(serde_json::json!({"kind":"runtime","instance":key,"state":"stopped"}));
    }

    fn configure_host(&self, host: &HostChannel, package: PluginPackageInfo) {
        let manager = self.clone();
        let instance = instance_key(&package.id, &package.digest);
        host.handle("tools.result", move |call| {
            manager.complete_tool(call.data().clone());
            call.resolve(serde_json::Value::Null);
        });
        let events = self.events.clone();
        let permitted = package.capabilities.iter().any(|capability| {
            capability.name == "composer.submit"
                && (capability.targets.is_empty()
                    || capability.targets.contains(&"*".into())
                    || capability.targets.contains(&instance.clone()))
        });
        host.handle("composer.submit", move |call| {
            if permitted {
                let _ = events.send_blocking(
                    serde_json::json!({
                        "kind":"host.action","name":"composer.submit","data":call.data().clone()
                    })
                    .to_string(),
                );
            } else {
                call.reject("composer.submit is not granted");
            }
        });
        let storage = self
            .inner
            .lock()
            .expect("plugin manager lock")
            .data_dir
            .clone()
            .join("instances")
            .join(instance_key(&package.id, &package.digest))
            .join("storage.json");
        let read_storage = storage.clone();
        host.handle("storage.read", move |call| {
            match std::fs::read(&read_storage) {
                Ok(bytes) => match serde_json::from_slice(&bytes) {
                    Ok(value) => call.resolve(value),
                    Err(error) => call.reject(error.to_string()),
                },
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    call.resolve(serde_json::Value::Null);
                }
                Err(error) => call.reject(error.to_string()),
            }
        });
        host.handle("storage.write", move |call| {
            let result = (|| {
                let bytes = serde_json::to_vec(call.data()).map_err(|error| error.to_string())?;
                if bytes.len() > 1024 * 1024 {
                    return Err("plugin storage exceeds 1 MiB".into());
                }
                if let Some(parent) = storage.parent() {
                    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
                }
                std::fs::write(&storage, bytes).map_err(|error| error.to_string())
            })();
            match result {
                Ok(()) => call.resolve(serde_json::Value::Null),
                Err(error) => call.reject(error),
            }
        });
    }

    fn complete_tool(&self, data: serde_json::Value) {
        let request_id = data
            .get("request_id")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        let pending = self
            .inner
            .lock()
            .expect("plugin manager lock")
            .pending
            .remove(request_id);
        let Some(pending) = pending else {
            return;
        };
        let result = data
            .get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let error = data
            .get("error")
            .and_then(|value| value.as_str())
            .map(|detail| CoreError::PluginTransport {
                detail: detail.into(),
            });
        let agent = self
            .inner
            .lock()
            .expect("plugin manager lock")
            .agent
            .clone();
        if pending.selection {
            let selection = PluginToolsSelection {
                request_id: request_id.into(),
                session_id: pending.session_id,
                package: pending.package,
                digest: pending.digest,
                result: match error {
                    Some(error) => Err(error),
                    None => Ok(serde_json::from_value(result).unwrap_or_else(|_| Vec::new())),
                },
            };
            let _ = agent.try_send(pig_protocol::Op::PluginToolsSelection { selection });
        } else {
            let result = PluginToolResult {
                request_id: request_id.into(),
                session_id: pending.session_id,
                package: pending.package,
                digest: pending.digest,
                result: error.map_or(Ok(result), Err),
            };
            let _ = agent.try_send(pig_protocol::Op::PluginToolResult { result });
        }
    }

    pub fn dispatch_ui_event(
        &self,
        instance: &str,
        node: &str,
        kind: &str,
        text: Option<String>,
    ) -> Result<(), String> {
        let mut inner = self.inner.lock().expect("plugin manager lock");
        let instance_id = instance.to_owned();
        let control = inner
            .instances
            .get(instance)
            .and_then(|instance| instance.engine.as_ref())
            .map(|engine| engine.control())
            .ok_or_else(|| "plugin engine is not active".to_owned())?;
        let id = node.parse::<u32>().map_err(|_| "invalid remote node id")?;
        match kind {
            "press" => control.dispatch_event(
                id,
                EventKind::Press,
                EventPayload::Press(PressEvent::default()),
            ),
            "changeText" => {
                let count = inner
                    .input_counts
                    .entry((instance_id, node.into()))
                    .or_default();
                *count += 1;
                control.dispatch_event(
                    id,
                    EventKind::ChangeText,
                    EventPayload::Text {
                        text: text.unwrap_or_default(),
                        count: *count,
                    },
                );
            }
            _ => return Err(format!("unsupported plugin UI event: {kind}")),
        }
        Ok(())
    }

    fn emit(&self, value: serde_json::Value) {
        let _ = self.events.send_blocking(value.to_string());
    }
}

struct UiSink {
    instance: String,
    epoch: u64,
    sequence: AtomicU64,
    previous: Mutex<String>,
    events: async_channel::Sender<String>,
}

impl MountSink for UiSink {
    fn submit(&self, batch: MountBatch) {
        let sequence = self.sequence.fetch_add(1, Ordering::SeqCst) + 1;
        let document = {
            let previous = self.previous.lock().expect("remote document lock");
            project_batch(batch, &self.instance, self.epoch, sequence, &previous)
        };
        let encoded = serde_json::json!({"kind":"ui.document","document":document}).to_string();
        *self.previous.lock().expect("remote document lock") =
            serde_json::json!({self.instance.clone(): document}).to_string();
        let _ = self.events.send_blocking(encoded);
    }

    fn report_error(&self, message: &str) {
        let _ = self.events.send_blocking(
            serde_json::json!({
                "kind":"runtime","instance":self.instance,"state":"failed","message":message
            })
            .to_string(),
        );
    }
}

fn instance_key(id: &str, digest: &str) -> String {
    format!("{id}:{digest}")
}

impl Drop for Inner {
    fn drop(&mut self) {
        for (_, instance) in std::mem::take(&mut self.instances) {
            drop(instance.engine);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_results_match_the_pending_request() {
        let (ops, receiver) = async_channel::unbounded();
        let manager = PluginSystemManager::new(PathBuf::from("/tmp/pig-plugin-manager-test"), ops);
        manager
            .inner
            .lock()
            .expect("plugin manager lock")
            .pending
            .insert(
                "request-1".into(),
                PendingCall {
                    session_id: "session-1".into(),
                    package: "moon".into(),
                    digest: "digest-1".into(),
                    selection: false,
                },
            );
        manager.complete_tool(serde_json::json!({
            "request_id": "request-1",
            "result": {"echo": "hello"}
        }));
        let op = receiver.try_recv().expect("tool result op");
        let pig_protocol::Op::PluginToolResult { result } = op else {
            panic!("unexpected op: {op:?}");
        };
        assert_eq!(result.request_id, "request-1");
        assert_eq!(result.session_id, "session-1");
        assert_eq!(result.package, "moon");
        assert_eq!(result.digest, "digest-1");
        assert_eq!(
            result.result.expect("tool result"),
            serde_json::json!({"echo": "hello"})
        );
        manager.complete_tool(serde_json::json!({
            "request_id": "request-1",
            "result": {"echo": "stale"}
        }));
        assert!(receiver.try_recv().is_err());
    }
}
