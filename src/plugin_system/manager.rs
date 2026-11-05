use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use nsh_core::{
    ApproxTextEngine, EventKind, EventPayload, MountBatch, PayloadValue, Point, Points, PressEvent,
};
use nsh_engine::{App, Engine, EngineConfig, HostChannel, MountSink};
use pig_protocol::{
    CoreError, Event, PluginContributions, PluginEnvelope, PluginHello, PluginLifecycleState,
    PluginMessageKind, PluginNegotiation, PluginPackageInfo, PluginProtocolError,
    PluginRuntimeMetrics, PluginRuntimeSnapshot, PluginSequenceTracker, PluginServiceRequest,
    PluginServiceResponse, PluginSnapshot, PluginSource, PluginStream, PluginToolResult,
    PluginToolsSelection, PluginUiContribution, PluginUiEvent, RemoteDocument,
};

use super::loader::{installed_package_path, load_verified};
use super::mount_projector::project_batch;
use super::services::{HostServiceGateway, ServiceExecution, ServiceReply, storage_path};

const START_TIMEOUT: Duration = Duration::from_secs(10);
const HEALTH_INTERVAL: Duration = Duration::from_secs(3);
const HEALTH_TIMEOUT: Duration = Duration::from_secs(9);
const MAX_RETRY_ATTEMPTS: u32 = 3;
const MAX_EVENT_IDS: usize = 4096;
const MAX_PENDING_TOOLS: usize = 32;

#[derive(Clone)]
pub struct PluginSystemManager {
    pub(super) inner: Arc<Mutex<Inner>>,
    pub(super) events: async_channel::Sender<String>,
    pub(super) receiver: async_channel::Receiver<String>,
    pub(super) gateway: HostServiceGateway,
    pub(super) surface_work: async_channel::Sender<super::host_api::SurfaceWork>,
}

pub(super) struct Inner {
    pub(super) data_dir: PathBuf,
    trust_root: PathBuf,
    agent: async_channel::Sender<pig_protocol::Op>,
    workspace: PathBuf,
    pub(super) instances: BTreeMap<String, PluginInstance>,
    desired: BTreeMap<String, (PluginPackageInfo, u64)>,
    latest_generation: u64,
    retries: BTreeMap<String, RetryInstance>,
    failed: BTreeSet<String>,
    pending: BTreeMap<String, PendingCall>,
    retired_outgoing: BTreeMap<String, (u64, PluginSequenceTracker)>,
    epoch: u64,
    pub(super) surfaces: BTreeMap<String, super::host_api::SurfaceState>,
    pub(super) surface_requests: BTreeMap<String, super::host_api::PendingSurfaceRequest>,
    pub(super) surface_revision: u64,
}

struct RetryInstance {
    package: PluginPackageInfo,
    generation: u64,
    attempt: u32,
    at: Instant,
}

pub(super) struct PluginInstance {
    pub(super) package: PluginPackageInfo,
    pub(super) host: HostChannel,
    pub(super) engine: Option<Engine>,
    pub(super) epoch: u64,
    pub(super) generation: u64,
    pub(super) engine_generation: u64,
    pub(super) state: PluginLifecycleState,
    pub(super) attempt: u32,
    pub(super) negotiation: Option<PluginNegotiation>,
    pub(super) ready: bool,
    pub(super) started_at: Instant,
    pub(super) last_health: Instant,
    pub(super) health_token: u64,
    pub(super) incoming: PluginSequenceTracker,
    pub(super) outgoing: PluginSequenceTracker,
    pub(super) staged_contributions: PluginContributions,
    pub(super) active_contributions: PluginContributions,
    pub(super) staged_document: Option<RemoteDocument>,
    pub(super) document: Option<RemoteDocument>,
    pub(super) pending_frame: Option<RemoteDocument>,
    pub(super) contribution_sync: Option<String>,
    pub(super) contributions_dirty: bool,
    pub(super) metrics: PluginRuntimeMetrics,
    pub(super) event_ids: BTreeSet<String>,
    pub(super) event_order: VecDeque<String>,
    pub(super) logs: VecDeque<serde_json::Value>,
    pub(super) log_window: Instant,
    pub(super) log_count: u32,
}

impl PluginInstance {
    pub(super) fn new(
        package: PluginPackageInfo,
        host: HostChannel,
        epoch: u64,
        generation: u64,
        attempt: u32,
    ) -> Self {
        let now = Instant::now();
        Self {
            package,
            host,
            engine: None,
            epoch,
            generation,
            engine_generation: generation,
            state: PluginLifecycleState::Starting,
            attempt,
            negotiation: None,
            ready: false,
            started_at: now,
            last_health: now,
            health_token: 0,
            incoming: PluginSequenceTracker::default(),
            outgoing: PluginSequenceTracker::default(),
            staged_contributions: PluginContributions::default(),
            active_contributions: PluginContributions::default(),
            staged_document: None,
            document: None,
            pending_frame: None,
            contribution_sync: None,
            contributions_dirty: false,
            metrics: PluginRuntimeMetrics::default(),
            event_ids: BTreeSet::new(),
            event_order: VecDeque::new(),
            logs: VecDeque::new(),
            log_window: now,
            log_count: 0,
        }
    }

    fn runtime(&self, state: PluginLifecycleState) -> PluginRuntimeSnapshot {
        let mut metrics = self.metrics.clone();
        metrics.pending_requests = 0;
        PluginRuntimeSnapshot {
            instance: String::new(),
            state,
            epoch: self.epoch,
            generation: self.generation,
            attempt: self.attempt,
            retry_after_ms: None,
            message: None,
            features: self.negotiation.as_ref().map_or(0, |value| value.features),
            metrics,
        }
    }
}

struct PendingCall {
    session_id: String,
    package: String,
    digest: String,
    instance: String,
    epoch: u64,
    selection: bool,
    deadline: Instant,
}

impl PluginSystemManager {
    pub fn new(
        data_dir: PathBuf,
        agent: async_channel::Sender<pig_protocol::Op>,
        workspace: PathBuf,
    ) -> Self {
        let (events, receiver) = async_channel::bounded(2048);
        let (surface_work, surface_receiver) = async_channel::bounded(128);
        let manager = Self {
            inner: Arc::new(Mutex::new(Inner {
                trust_root: data_dir.join("plugin-root.pub"),
                data_dir: data_dir.clone(),
                agent: agent.clone(),
                workspace,
                instances: BTreeMap::new(),
                desired: BTreeMap::new(),
                latest_generation: 0,
                retries: BTreeMap::new(),
                failed: BTreeSet::new(),
                pending: BTreeMap::new(),
                retired_outgoing: BTreeMap::new(),
                epoch: 0,
                surfaces: BTreeMap::new(),
                surface_requests: BTreeMap::new(),
                surface_revision: 0,
            })),
            events,
            receiver,
            gateway: HostServiceGateway::new(data_dir, agent),
            surface_work,
        };
        manager.start_surface_worker(surface_receiver);
        manager.start_maintenance();
        manager
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
        let same_root = std::fs::canonicalize(root).ok() == std::fs::canonicalize(&trust_root).ok();
        if !same_root {
            std::fs::copy(root, &trust_root)
                .map_err(|error| format!("store plugin root key: {error}"))?;
        }
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

    pub fn uninstall(&self, id: &str, digest: &str) -> Result<(), String> {
        let keys: Vec<String> = self
            .inner
            .lock()
            .expect("plugin manager lock")
            .instances
            .iter()
            .filter(|(_, instance)| instance.package.id == id && instance.package.digest == digest)
            .map(|(key, _)| key.clone())
            .collect();
        for key in keys {
            self.stop(&key);
        }
        let (data_dir, agent) = {
            let inner = self.inner.lock().expect("plugin manager lock");
            (inner.data_dir.clone(), inner.agent.clone())
        };
        let root = data_dir.join("packages").join(id).join(digest);
        if root.exists() {
            std::fs::remove_dir_all(root).map_err(|error| format!("remove plugin: {error}"))?;
        }
        agent
            .try_send(pig_protocol::Op::SyncPlugins {
                packages: super::loader::installed_sources(&data_dir),
            })
            .map_err(|error| error.to_string())
    }

    pub fn retry(&self, id: &str, digest: &str) -> Result<(), String> {
        let (package, generation) = {
            let mut inner = self.inner.lock().expect("plugin manager lock");
            let (package, generation) = inner
                .desired
                .iter()
                .find(|(_, (package, _))| {
                    package.id == id && package.digest == digest && package.enabled
                })
                .map(|(_, (package, generation))| (package.clone(), *generation))
                .ok_or_else(|| "the enabled plugin instance was not found".to_owned())?;
            let blocked: Vec<String> = inner
                .failed
                .iter()
                .chain(inner.retries.keys())
                .filter(|key| {
                    inner.desired.get(*key).is_some_and(|(candidate, _)| {
                        candidate.id == id && candidate.digest == digest
                    })
                })
                .cloned()
                .collect();
            for key in blocked {
                inner.failed.remove(&key);
                inner.retries.remove(&key);
            }
            (package, generation)
        };
        if package.contributions.is_engine_backed() {
            self.start_with_attempt(package, generation, 0);
        } else {
            self.activate_declarative(package, generation);
        }
        Ok(())
    }

    pub fn dispatch_command(&self, key: &str, command_id: &str) -> Result<Option<String>, String> {
        let (page_id, found) = {
            let inner = self.inner.lock().expect("plugin manager lock");
            let instance = inner
                .instances
                .get(key)
                .filter(|instance| instance.state == PluginLifecycleState::Active)
                .ok_or_else(|| "plugin engine is not active".to_owned())?;
            let command = instance
                .package
                .contributions
                .commands
                .iter()
                .chain(instance.active_contributions.commands.iter())
                .find(|command| command.id == command_id && command.enabled);
            let ui = instance
                .active_contributions
                .ui
                .iter()
                .chain(instance.package.contributions.ui.iter())
                .find(|contribution| {
                    contribution.id == command_id
                        && matches!(
                            contribution.kind,
                            pig_protocol::PluginUiContributionKind::Command
                        )
                        && contribution
                            .props
                            .get("disabled")
                            .and_then(serde_json::Value::as_bool)
                            != Some(true)
                });
            let page_id = command
                .and_then(|command| command.page_id.clone())
                .or_else(|| {
                    ui.and_then(|contribution| {
                        contribution
                            .props
                            .get("pageId")
                            .or_else(|| contribution.props.get("page_id"))
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned)
                    })
                });
            (page_id, command.is_some() || ui.is_some())
        };
        if !found {
            return Err("plugin command is not registered".into());
        }
        self.send_to_instance(
            key,
            PluginMessageKind::HostAction,
            PluginStream::Services,
            None,
            serde_json::json!({"name": "command.invoke", "commandId": command_id}),
        );
        Ok(page_id)
    }

    pub fn set_locale(&self, locale: &str) {
        if !self.gateway.set_locale(locale) {
            return;
        }
        let instances: Vec<String> = self
            .inner
            .lock()
            .expect("plugin manager lock")
            .instances
            .keys()
            .cloned()
            .collect();
        for key in instances {
            self.send_to_instance(
                &key,
                PluginMessageKind::ServiceEvent,
                PluginStream::Services,
                None,
                serde_json::json!({"name": "locale.changed", "locale": self.gateway.locale()}),
            );
        }
    }

    pub fn handle_core_event(&self, event: &Event) {
        for reply in self.gateway.handle_core_event(event) {
            self.send_service_reply(reply);
        }
        let accept_snapshot = |snapshot: &PluginSnapshot| {
            let current = {
                let mut inner = self.inner.lock().expect("plugin manager lock");
                if snapshot.generation < inner.latest_generation {
                    false
                } else {
                    inner.latest_generation = snapshot.generation;
                    true
                }
            };
            if current {
                self.reconcile(snapshot);
            }
            current
        };
        match event {
            Event::PluginSnapshot { snapshot } => {
                accept_snapshot(snapshot);
            }
            Event::PluginRuntimeContributionCommitted {
                request_id,
                epoch,
                snapshot,
            } => {
                accept_snapshot(snapshot);
                self.complete_contribution_sync(request_id, *epoch, snapshot);
            }
            Event::PluginToolCall { call } => self.start_tool_call(call.clone(), false),
            Event::PluginToolsQuery { query } => {
                let call = pig_protocol::PluginToolCall {
                    request_id: query.request_id.clone(),
                    session_id: query.session_id.clone(),
                    package: query.package.clone(),
                    digest: query.digest.clone(),
                    tool: String::new(),
                    args: serde_json::to_value(query).unwrap_or_default(),
                };
                self.start_tool_call(call, true);
            }
            Event::PluginActionResult { result } => {
                if let Err(error) = &result.result {
                    let key = if result.request_id.is_empty() {
                        self.instance_key_for_package(&result.package, &result.digest)
                    } else {
                        self.inner
                            .lock()
                            .expect("plugin manager lock")
                            .instances
                            .iter()
                            .find(|(_, instance)| {
                                instance.contribution_sync.as_deref()
                                    == Some(result.request_id.as_str())
                            })
                            .map(|(key, _)| key.clone())
                    };
                    if let Some(key) = key {
                        self.engine_failed(
                            &key,
                            None,
                            PluginProtocolError::from_core(error).message,
                        );
                    }
                }
            }
            _ => {}
        }
        self.note_session_event(event);
    }

    fn start_tool_call(&self, call: pig_protocol::PluginToolCall, selection: bool) {
        let key = self.instance_key_for_package(&call.package, &call.digest);
        let active = key.as_ref().is_some_and(|key| {
            self.inner
                .lock()
                .expect("plugin manager lock")
                .instances
                .get(key)
                .is_some_and(|instance| instance.state == PluginLifecycleState::Active)
        });
        if !active {
            self.finish_tool_without_engine(call, selection, "the plugin engine is not active");
            return;
        }
        if serde_json::to_vec(&call.args).map_or(0, |bytes| bytes.len()) > 1024 * 1024 {
            self.finish_tool_without_engine(call, selection, "plugin tool arguments exceed 1 MiB");
            return;
        }
        let key = key.expect("active key");
        let (epoch, overloaded) = {
            let inner = self.inner.lock().expect("plugin manager lock");
            let overloaded = inner.pending.contains_key(&call.request_id)
                || inner
                    .pending
                    .values()
                    .filter(|pending| pending.instance == key)
                    .count()
                    >= MAX_PENDING_TOOLS;
            (
                inner
                    .instances
                    .get(&key)
                    .map(|instance| instance.epoch)
                    .unwrap_or_default(),
                overloaded,
            )
        };
        if overloaded {
            self.finish_tool_without_engine(
                call,
                selection,
                "too many concurrent or duplicate plugin tool calls",
            );
            return;
        }
        let pending = PendingCall {
            session_id: call.session_id.clone(),
            package: call.package.clone(),
            digest: call.digest.clone(),
            instance: key.clone(),
            epoch,
            selection,
            deadline: Instant::now()
                + if selection {
                    Duration::from_secs(2)
                } else {
                    Duration::from_secs(60)
                },
        };
        self.inner
            .lock()
            .expect("plugin manager lock")
            .pending
            .insert(call.request_id.clone(), pending);
        self.send_to_instance(
            &key,
            if selection {
                PluginMessageKind::ToolsSelect
            } else {
                PluginMessageKind::ToolsCall
            },
            PluginStream::Tools,
            Some(call.request_id.clone()),
            serde_json::to_value(call).unwrap_or_default(),
        );
    }

    fn finish_tool_without_engine(
        &self,
        call: pig_protocol::PluginToolCall,
        selection: bool,
        message: &str,
    ) {
        let agent = self
            .inner
            .lock()
            .expect("plugin manager lock")
            .agent
            .clone();
        let error = Err(CoreError::PluginTransport {
            detail: message.to_owned(),
        });
        if selection {
            let _ = agent.try_send(pig_protocol::Op::PluginToolsSelection {
                selection: PluginToolsSelection {
                    request_id: call.request_id,
                    session_id: call.session_id,
                    package: call.package,
                    digest: call.digest,
                    result: error.map(|_| Vec::new()),
                },
            });
        } else {
            let _ = agent.try_send(pig_protocol::Op::PluginToolResult {
                result: PluginToolResult {
                    request_id: call.request_id,
                    session_id: call.session_id,
                    package: call.package,
                    digest: call.digest,
                    result: error,
                },
            });
        }
    }

    fn reconcile(&self, snapshot: &PluginSnapshot) {
        let workspace = self
            .inner
            .lock()
            .expect("plugin manager lock")
            .workspace
            .clone();
        let desired: BTreeMap<String, (PluginPackageInfo, u64)> = snapshot
            .packages
            .iter()
            .map(|package| {
                (
                    instance_key(package, &workspace),
                    (package.clone(), snapshot.generation),
                )
            })
            .collect();
        self.inner.lock().expect("plugin manager lock").desired = desired.clone();
        let existing: Vec<String> = self
            .inner
            .lock()
            .expect("plugin manager lock")
            .instances
            .keys()
            .cloned()
            .collect();
        for key in existing {
            let keep = desired
                .get(&key)
                .is_some_and(|(package, _)| package.enabled);
            if !keep {
                self.stop(&key);
            }
        }
        for (key, (package, generation)) in desired {
            if !package.enabled {
                continue;
            }
            let blocked = {
                let inner = self.inner.lock().expect("plugin manager lock");
                inner.retries.contains_key(&key) || inner.failed.contains(&key)
            };
            if blocked {
                continue;
            }
            let unchanged = self
                .inner
                .lock()
                .expect("plugin manager lock")
                .instances
                .get_mut(&key)
                .map(|instance| {
                    instance.generation = generation;
                    instance.state == PluginLifecycleState::Active
                })
                .unwrap_or(false);
            if unchanged {
                continue;
            }
            if package.contributions.is_engine_backed() {
                self.start_with_attempt(package, generation, 0);
            } else {
                self.activate_declarative(package, generation);
            }
        }
    }

    fn activate_declarative(&self, package: PluginPackageInfo, generation: u64) {
        let key = {
            let workspace = self
                .inner
                .lock()
                .expect("plugin manager lock")
                .workspace
                .clone();
            instance_key(&package, &workspace)
        };
        let host = HostChannel::new();
        let mut instance = PluginInstance::new(package, host, 0, generation, 0);
        instance.state = PluginLifecycleState::Active;
        instance.negotiation = Some(PluginNegotiation {
            ui_protocol: 1,
            services_protocol: 1,
            features: pig_protocol::PLUGIN_HOST_FEATURES,
            rejected_optional_features: 0,
            locale: self.gateway.locale(),
        });
        let runtime = instance.runtime(PluginLifecycleState::Active);
        self.inner
            .lock()
            .expect("plugin manager lock")
            .instances
            .insert(key.clone(), instance);
        self.emit_instance_envelope(
            &key,
            (0, generation),
            PluginMessageKind::UiActivate,
            PluginStream::Ui,
            serde_json::json!({"package": self.package_for_key(&key), "document": null, "contributions": {}}),
            false,
        );
        self.emit_runtime(&key, runtime, false);
    }

    fn start_with_attempt(&self, package: PluginPackageInfo, generation: u64, attempt: u32) {
        let (key, epoch, data_dir, trust_root) = {
            let mut inner = self.inner.lock().expect("plugin manager lock");
            let key = instance_key(&package, &inner.workspace);
            if inner.instances.contains_key(&key) {
                return;
            }
            inner.epoch += 1;
            let epoch = inner.epoch;
            let package_root = package.root.join(".pig-root.pub");
            (
                key,
                epoch,
                inner.data_dir.clone(),
                if package_root.is_file() {
                    package_root
                } else {
                    inner.trust_root.clone()
                },
            )
        };
        let host = HostChannel::new();
        host.set_params(
            serde_json::json!({
                "plugin": {
                    "instance": key,
                    "epoch": epoch,
                    "generation": generation,
                    "protocol": pig_protocol::PLUGIN_UI_PROTOCOL_VERSION,
                    "servicesProtocol": pig_protocol::PLUGIN_SERVICES_PROTOCOL_VERSION,
                    "features": pig_protocol::PLUGIN_HOST_FEATURES,
                    "locale": self.gateway.locale(),
                    "resourcePrefix": format!("plugin://{}/{}/", package.id, package.digest),
                }
            })
            .as_object()
            .expect("plugin params object")
            .clone(),
        );
        self.configure_host(&host, &key, epoch);
        let instance =
            PluginInstance::new(package.clone(), host.clone(), epoch, generation, attempt);
        let starting = instance.runtime(PluginLifecycleState::Starting);
        self.inner
            .lock()
            .expect("plugin manager lock")
            .instances
            .insert(key.clone(), instance);
        self.emit_runtime(&key, starting, false);
        let manager = self.clone();
        std::thread::spawn(move || {
            let result = load_verified(
                &installed_package_path(&package.root),
                &trust_root,
                &data_dir,
            )
            .map_err(|error| error.to_string())
            .and_then(|verified| {
                let sink = UiSink {
                    manager: manager.clone(),
                    instance: key.clone(),
                    epoch,
                    generation,
                    sequence: std::sync::atomic::AtomicU64::new(0),
                    previous: Mutex::new(None),
                    resource_prefix: format!("plugin://{}/{}/", package.id, package.digest),
                };
                let mut config = EngineConfig::new(App::Js(verified.bundle));
                config.host = host.clone();
                config.ui_sets = vec![nsh_base::registry()];
                config.data_dir = storage_path(&data_dir, &plugin_data_key(&package))
                    .parent()
                    .map(Path::to_path_buf);
                config.limits.memory_bytes = 64 * 1024 * 1024;
                config.limits.max_nodes = super::mount_projector::MAX_REMOTE_NODES;
                config.limits.max_op_values = 100_000;
                config.limits.max_canvas_ops = 0;
                config.limits.load_budget = Duration::from_secs(3);
                config.limits.call_budget = Duration::from_millis(750);
                Engine::start(config, || ApproxTextEngine, sink).map_err(|error| error.to_string())
            });
            match result {
                Ok(engine) => {
                    let installed = {
                        let mut inner = manager.inner.lock().expect("plugin manager lock");
                        if let Some(instance) = inner.instances.get_mut(&key)
                            && instance.epoch == epoch
                            && instance.state == PluginLifecycleState::Starting
                        {
                            instance.engine = Some(engine);
                            true
                        } else {
                            false
                        }
                    };
                    if installed {
                        manager.maybe_begin_activation(&key, epoch);
                    }
                }
                Err(error) => manager.engine_failed(&key, Some(epoch), error),
            }
        });
    }

    fn configure_host(&self, host: &HostChannel, key: &str, epoch: u64) {
        let manager = self.clone();
        let instance = key.to_owned();
        host.handle("plugin.protocol", move |call| {
            let result = serde_json::from_value::<PluginEnvelope>(call.data().clone())
                .map_err(|error| error.to_string())
                .and_then(|envelope| manager.handle_envelope(&instance, epoch, envelope));
            match result {
                Ok(value) => call.resolve(value),
                Err(error) => call.reject(error),
            }
        });
    }

    fn handle_envelope(
        &self,
        key: &str,
        epoch: u64,
        envelope: PluginEnvelope,
    ) -> Result<serde_json::Value, String> {
        envelope.validate().map_err(|error| error.message)?;
        {
            let mut inner = self.inner.lock().expect("plugin manager lock");
            let instance = inner
                .instances
                .get_mut(key)
                .ok_or_else(|| "plugin instance is not running".to_owned())?;
            if envelope.instance != key || envelope.epoch != epoch || instance.epoch != epoch {
                return Err("plugin instance or epoch is stale".into());
            }
            if envelope.generation != instance.engine_generation {
                return Err("plugin generation is stale".into());
            }
            instance
                .incoming
                .accept(envelope.stream, envelope.sequence)
                .map_err(|error| error.message)?;
        }
        match envelope.kind {
            PluginMessageKind::LifecycleHello => {
                let hello: PluginHello =
                    serde_json::from_value(envelope.body).map_err(|error| error.to_string())?;
                let negotiation = hello
                    .negotiate(&self.gateway.locale())
                    .map_err(|error| error.message)?;
                self.inner
                    .lock()
                    .expect("plugin manager lock")
                    .instances
                    .get_mut(key)
                    .expect("running instance")
                    .negotiation = Some(negotiation.clone());
                self.send_to_instance(
                    key,
                    PluginMessageKind::LifecycleStart,
                    PluginStream::Lifecycle,
                    None,
                    serde_json::to_value(negotiation).unwrap_or_default(),
                );
            }
            PluginMessageKind::LifecycleReady => {
                {
                    let mut inner = self.inner.lock().expect("plugin manager lock");
                    let instance = inner.instances.get_mut(key).expect("running instance");
                    if instance.negotiation.is_none() {
                        return Err("plugin sent ready before the capability handshake".into());
                    }
                    instance.ready = true;
                    instance.last_health = Instant::now();
                }
                self.maybe_begin_activation(key, epoch);
            }
            PluginMessageKind::LifecycleHealth => {
                let token = envelope
                    .body
                    .get("token")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
                let mut inner = self.inner.lock().expect("plugin manager lock");
                let instance = inner.instances.get_mut(key).expect("running instance");
                if token != instance.health_token {
                    return Err("stale health check token".into());
                }
                instance.last_health = Instant::now();
            }
            PluginMessageKind::LifecycleFailed => {
                let message = envelope
                    .body
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("plugin reported a lifecycle failure")
                    .to_owned();
                self.engine_failed(key, Some(epoch), message);
            }
            PluginMessageKind::UiCommit => {
                let document: RemoteDocument =
                    serde_json::from_value(envelope.body).map_err(|error| error.to_string())?;
                self.accept_document(key, epoch, document);
            }
            PluginMessageKind::ContributionRegister => {
                let delta: PluginContributions =
                    serde_json::from_value(envelope.body).map_err(|error| error.to_string())?;
                self.merge_contributions(key, epoch, delta)?;
            }
            PluginMessageKind::ContributionRemove => {
                let id = envelope
                    .body
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| "contribution id is required".to_owned())?;
                self.remove_contribution(key, epoch, id)?;
            }
            PluginMessageKind::ServiceRequest => {
                let request: PluginServiceRequest =
                    serde_json::from_value(envelope.body).map_err(|error| error.to_string())?;
                let request_id = envelope
                    .request_id
                    .ok_or_else(|| "service request requires requestId".to_owned())?;
                let (package, generation) = {
                    let inner = self.inner.lock().expect("plugin manager lock");
                    let instance = inner.instances.get(key).expect("running instance");
                    if instance.state != PluginLifecycleState::Active {
                        return Err("plugin instance is not active".into());
                    }
                    (instance.package.clone(), instance.engine_generation)
                };
                match self
                    .gateway
                    .execute(&package, key, epoch, generation, &request_id, request)
                {
                    ServiceExecution::Complete(reply) => self.send_service_reply(reply),
                    ServiceExecution::Forwarded => {}
                    ServiceExecution::Action(reply, action) => {
                        self.send_service_reply(reply);
                        self.emit_instance_envelope(
                            key,
                            (epoch, generation),
                            PluginMessageKind::HostAction,
                            PluginStream::Services,
                            action,
                            false,
                        );
                    }
                }
            }
            PluginMessageKind::ToolsResult => {
                self.complete_tool(key, epoch, envelope.body);
            }
            PluginMessageKind::LogEvent => {
                self.record_log(key, epoch, envelope.body);
            }
            other => return Err(format!("unsupported plugin message: {other:?}")),
        }
        Ok(serde_json::Value::Null)
    }

    fn maybe_begin_activation(&self, key: &str, epoch: u64) {
        let ready = self
            .inner
            .lock()
            .expect("plugin manager lock")
            .instances
            .get(key)
            .is_some_and(|instance| {
                instance.epoch == epoch
                    && instance.ready
                    && instance.engine.is_some()
                    && instance.negotiation.is_some()
                    && instance.state == PluginLifecycleState::Starting
                    && instance.contribution_sync.is_none()
            });
        if ready {
            self.sync_contributions(key, epoch);
        }
    }

    fn sync_contributions(&self, key: &str, epoch: u64) {
        let request = {
            let mut inner = self.inner.lock().expect("plugin manager lock");
            let Some(instance) = inner.instances.get_mut(key) else {
                return;
            };
            if instance.epoch != epoch {
                return;
            }
            if instance.state == PluginLifecycleState::Starting
                && (!instance.ready || instance.engine.is_none())
            {
                return;
            }
            if !matches!(
                instance.state,
                PluginLifecycleState::Starting | PluginLifecycleState::Active
            ) {
                return;
            }
            if instance.contribution_sync.is_some() {
                instance.contributions_dirty = true;
                return;
            }
            let request_id = format!("contributions:{}:{}", epoch, next_event_id());
            instance.contribution_sync = Some(request_id.clone());
            instance.contributions_dirty = false;
            (
                request_id,
                instance.package.id.clone(),
                instance.package.digest.clone(),
                instance.epoch,
                instance.engine_generation,
                instance.staged_contributions.clone(),
            )
        };
        let agent = self
            .inner
            .lock()
            .expect("plugin manager lock")
            .agent
            .clone();
        if let Err(error) = agent.try_send(pig_protocol::Op::SyncPluginRuntimeContributions {
            request_id: request.0,
            package: request.1,
            digest: request.2,
            epoch: request.3,
            generation: request.4,
            contributions: request.5,
        }) {
            self.engine_failed(key, Some(epoch), error.to_string());
        }
    }

    fn complete_contribution_sync(&self, request_id: &str, epoch: u64, snapshot: &PluginSnapshot) {
        let (key, activating, package, document, contributions, runtime, dirty) =
            {
                let mut inner = self.inner.lock().expect("plugin manager lock");
                let generation = inner.latest_generation;
                let Some((key, instance)) = inner.instances.iter_mut().find(|(_, instance)| {
                    instance.contribution_sync.as_deref() == Some(request_id)
                }) else {
                    return;
                };
                if instance.epoch != epoch {
                    return;
                }
                let expected = snapshot
                    .runtime_contributions
                    .get(&format!(
                        "{}:{}",
                        instance.package.id, instance.package.digest
                    ))
                    .cloned()
                    .unwrap_or_default();
                if expected != instance.staged_contributions {
                    let key = key.clone();
                    drop(inner);
                    self.engine_failed(
                        &key,
                        Some(epoch),
                        "core did not commit the requested runtime contributions".to_owned(),
                    );
                    return;
                }
                let activating = instance.state == PluginLifecycleState::Starting;
                instance.contribution_sync = None;
                instance.generation = generation;
                instance.active_contributions = instance.staged_contributions.clone();
                if activating {
                    instance.state = PluginLifecycleState::Active;
                    instance.document = instance
                        .staged_document
                        .take()
                        .or_else(|| instance.pending_frame.take());
                }
                (
                    key.clone(),
                    activating,
                    instance.package.clone(),
                    instance.document.clone(),
                    instance.active_contributions.clone(),
                    instance.runtime(PluginLifecycleState::Active),
                    instance.contributions_dirty,
                )
            };
        self.emit_instance_envelope(
            &key,
            (epoch, runtime.generation),
            if activating {
                PluginMessageKind::UiActivate
            } else {
                PluginMessageKind::UiCommit
            },
            PluginStream::Ui,
            serde_json::json!({
                "package": package,
                "document": document,
                "contributions": contributions,
            }),
            false,
        );
        self.emit_runtime(&key, runtime, false);
        if activating {
            self.retire_previous_versions(&key);
        }
        if dirty {
            self.sync_contributions(&key, epoch);
        }
    }

    fn merge_contributions(
        &self,
        key: &str,
        epoch: u64,
        delta: PluginContributions,
    ) -> Result<(), String> {
        validate_delta(&delta)?;
        let mut inner = self.inner.lock().expect("plugin manager lock");
        let instance = inner
            .instances
            .get_mut(key)
            .ok_or("plugin is not running")?;
        if instance.epoch != epoch {
            return Err("plugin epoch is stale".into());
        }
        let mut staged = instance.staged_contributions.clone();
        merge_unique(&mut staged.pages, delta.pages, |page| page.id.clone());
        merge_unique(&mut staged.ui, delta.ui, |contribution| {
            contribution.id.clone()
        });
        merge_unique(&mut staged.themes, delta.themes, |theme| theme.id.clone());
        merge_unique(&mut staged.skills, delta.skills, |skill| skill.path.clone());
        merge_unique(&mut staged.mcp, delta.mcp, |server| server.name.clone());
        merge_unique(&mut staged.tools, delta.tools, |tool| tool.name.clone());
        merge_unique(&mut staged.commands, delta.commands, |command| {
            command.id.clone()
        });
        if serde_json::to_vec(&staged)
            .map_err(|error| error.to_string())?
            .len()
            > 1024 * 1024
        {
            return Err("runtime contributions exceed 1 MiB".into());
        }
        instance.staged_contributions = staged;
        drop(inner);
        self.sync_contributions(key, epoch);
        Ok(())
    }

    fn remove_contribution(&self, key: &str, epoch: u64, id: &str) -> Result<(), String> {
        let mut inner = self.inner.lock().expect("plugin manager lock");
        let instance = inner
            .instances
            .get_mut(key)
            .ok_or("plugin is not running")?;
        if instance.epoch != epoch {
            return Err("plugin epoch is stale".into());
        }
        instance
            .staged_contributions
            .pages
            .retain(|item| item.id != id);
        instance
            .staged_contributions
            .ui
            .retain(|item| item.id != id);
        instance
            .staged_contributions
            .themes
            .retain(|item| item.id != id);
        instance
            .staged_contributions
            .skills
            .retain(|item| item.path != id);
        instance
            .staged_contributions
            .mcp
            .retain(|item| item.name != id);
        instance
            .staged_contributions
            .tools
            .retain(|item| item.name != id);
        instance
            .staged_contributions
            .commands
            .retain(|item| item.id != id);
        drop(inner);
        self.sync_contributions(key, epoch);
        Ok(())
    }

    fn accept_document(&self, key: &str, epoch: u64, mut document: RemoteDocument) {
        let mut inner = self.inner.lock().expect("plugin manager lock");
        let Some(instance) = inner.instances.get_mut(key) else {
            return;
        };
        if instance.epoch != epoch || document.instance != key {
            return;
        }
        if document.epoch != epoch || document.generation != instance.engine_generation {
            drop(inner);
            self.engine_failed(
                key,
                Some(epoch),
                "remote UI document identity is stale".into(),
            );
            return;
        }
        let resource_prefix = format!(
            "plugin://{}/{}/",
            instance.package.id, instance.package.digest
        );
        if let Err(error) = super::mount_projector::validate_document(&document, &resource_prefix) {
            drop(inner);
            self.engine_failed(key, Some(epoch), error);
            return;
        }
        let previous = [
            instance.document.as_ref(),
            instance.pending_frame.as_ref(),
            instance.staged_document.as_ref(),
        ]
        .into_iter()
        .flatten()
        .map(|document| document.sequence)
        .max()
        .unwrap_or(0);
        let Some(revision) = previous.checked_add(1) else {
            drop(inner);
            self.engine_failed(key, Some(epoch), "remote UI revision quota exceeded".into());
            return;
        };
        document.base_revision = previous;
        document.sequence = revision;
        instance.metrics.nodes = document.nodes.len();
        instance.metrics.operations += 1;
        if instance.state == PluginLifecycleState::Active {
            if instance.pending_frame.is_some() {
                instance.metrics.coalesced_frames += 1;
            }
            instance.pending_frame = Some(document);
        } else {
            instance.staged_document = Some(document);
        }
    }

    pub fn acknowledge_ui(
        &self,
        key: &str,
        epoch: u64,
        generation: u64,
        ack: pig_protocol::PluginUiAck,
    ) -> Result<(), String> {
        let current = self
            .inner
            .lock()
            .expect("plugin manager lock")
            .instances
            .get(key)
            .is_some_and(|instance| {
                instance.epoch == epoch
                    && instance.generation == generation
                    && instance.state == PluginLifecycleState::Active
                    && instance
                        .document
                        .as_ref()
                        .is_some_and(|document| document.sequence == ack.revision)
            });
        if current {
            self.send_to_instance(
                key,
                PluginMessageKind::UiAck,
                PluginStream::Ui,
                None,
                serde_json::to_value(ack).map_err(|error| error.to_string())?,
            );
        }
        Ok(())
    }

    fn flush_frames(&self) {
        let frames: Vec<(String, u64, u64, RemoteDocument)> = self
            .inner
            .lock()
            .expect("plugin manager lock")
            .instances
            .iter_mut()
            .filter_map(|(key, instance)| {
                instance.pending_frame.take().map(|document| {
                    instance.document = Some(document.clone());
                    (key.clone(), instance.epoch, instance.generation, document)
                })
            })
            .collect();
        for (key, epoch, generation, document) in frames {
            self.emit_instance_envelope(
                &key,
                (epoch, generation),
                PluginMessageKind::UiCommit,
                PluginStream::Ui,
                serde_json::to_value(document).unwrap_or_default(),
                false,
            );
        }
    }

    fn send_service_reply(&self, reply: ServiceReply) {
        if reply.request_id.starts_with("surface-request-") {
            self.complete_surface_request(reply);
            return;
        }
        let current = self
            .inner
            .lock()
            .expect("plugin manager lock")
            .instances
            .get(&reply.instance)
            .is_some_and(|instance| {
                instance.epoch == reply.epoch && instance.engine_generation == reply.generation
            });
        if !current {
            return;
        }
        let response = PluginServiceResponse {
            service: reply.service,
            result: reply.result,
        };
        self.send_to_instance(
            &reply.instance,
            PluginMessageKind::ServiceResponse,
            PluginStream::Services,
            Some(reply.request_id),
            serde_json::to_value(response).unwrap_or_default(),
        );
    }

    fn send_to_instance(
        &self,
        key: &str,
        kind: PluginMessageKind,
        stream: PluginStream,
        request_id: Option<String>,
        body: serde_json::Value,
    ) {
        let message = {
            let mut inner = self.inner.lock().expect("plugin manager lock");
            let Some(instance) = inner.instances.get_mut(key) else {
                return;
            };
            let sequence = instance.outgoing.next(stream);
            let mut envelope = PluginEnvelope::new(
                kind,
                key,
                instance.epoch,
                stream,
                sequence,
                instance.engine_generation,
                body,
            );
            envelope.request_id = request_id;
            (instance.host.clone(), envelope)
        };
        if let Err(error) = message.0.send(
            "plugin.protocol",
            serde_json::to_value(message.1).unwrap_or_default(),
        ) {
            self.engine_failed(key, None, error);
        }
    }

    fn complete_tool(&self, key: &str, epoch: u64, data: serde_json::Value) {
        let request_id = data
            .get("request_id")
            .or_else(|| data.get("requestId"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let pending = {
            let mut inner = self.inner.lock().expect("plugin manager lock");
            if inner
                .pending
                .get(request_id)
                .is_some_and(|pending| pending.instance == key && pending.epoch == epoch)
            {
                inner.pending.remove_entry(request_id)
            } else {
                None
            }
        };
        let Some((request_id, pending)) = pending else {
            return;
        };
        let result = data
            .get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        if serde_json::to_vec(&result).map_or(0, |bytes| bytes.len()) > 1024 * 1024 {
            self.finish_pending_tool_with_id(
                request_id,
                pending,
                Err("plugin tool result exceeds 1 MiB".into()),
            );
            return;
        }
        let result = match data.get("error").and_then(|value| value.as_str()) {
            Some(error) => Err(error.to_owned()),
            None => Ok(result),
        };
        self.finish_pending_tool_with_id(request_id, pending, result);
    }

    pub fn dispatch_host_event(&self, key: &str, event: PluginUiEvent) -> Result<(), String> {
        {
            let mut inner = self.inner.lock().expect("plugin manager lock");
            let instance = inner
                .instances
                .get_mut(key)
                .ok_or_else(|| "plugin engine is not active".to_owned())?;
            if instance.state != PluginLifecycleState::Active {
                return Err("plugin engine is not active".into());
            }
            let property = match event.kind.as_str() {
                "context" | "openChange" => None,
                "press" => Some("onPress"),
                "changeText" => Some("onChange"),
                "focus" => Some("onFocus"),
                "blur" => Some("onBlur"),
                "keyPress" => Some("onKeyPress"),
                _ => return Err("unsupported host UI event".into()),
            };
            let contribution = instance
                .active_contributions
                .ui
                .iter()
                .chain(instance.package.contributions.ui.iter())
                .find(|contribution| {
                    contribution.id == event.contribution
                        && (event.kind == "openChange" || contribution.target == event.node)
                })
                .ok_or_else(|| "host UI contribution is not registered".to_owned())?;
            if let Some(property) = property {
                if contribution
                    .props
                    .get(property)
                    .and_then(serde_json::Value::as_bool)
                    != Some(true)
                    || !super::services::capability_granted(
                        &instance.package,
                        "ui.interact",
                        &event.node,
                    )
                {
                    return Err("host UI interaction is not authorized".into());
                }
            }
            if !event.payload.is_object()
                || serde_json::to_vec(&event.payload)
                    .map_err(|error| error.to_string())?
                    .len()
                    > 64 * 1024
            {
                return Err("host UI event payload exceeds 64 KiB".into());
            }
            if !instance.event_ids.insert(event.event_id.clone()) {
                return Ok(());
            }
            instance.event_order.push_back(event.event_id.clone());
            if instance.event_order.len() > MAX_EVENT_IDS
                && let Some(expired) = instance.event_order.pop_front()
            {
                instance.event_ids.remove(&expired);
            }
        }
        self.send_to_instance(
            key,
            PluginMessageKind::UiEvent,
            PluginStream::Events,
            None,
            serde_json::to_value(event).map_err(|error| error.to_string())?,
        );
        Ok(())
    }

    pub fn dispatch_ui_event_full(&self, key: &str, event: PluginUiEvent) -> Result<(), String> {
        if event.kind == "openChange" {
            return self.dispatch_host_event(key, event);
        }
        let (control, id, event_kind, payload, scroll) = {
            let mut inner = self.inner.lock().expect("plugin manager lock");
            let instance = inner
                .instances
                .get_mut(key)
                .ok_or_else(|| "plugin engine is not active".to_owned())?;
            if instance.state != PluginLifecycleState::Active {
                return Err("plugin engine is not active".into());
            }
            let document = instance
                .pending_frame
                .as_ref()
                .or(instance.document.as_ref())
                .ok_or_else(|| "plugin surface is not ready".to_owned())?;
            let surface_revision = if event.surface_revision == 0 {
                document.sequence
            } else {
                event.surface_revision
            };
            if surface_revision != document.sequence
                || !document.nodes.iter().any(|node| node.id == event.node)
            {
                return Err("plugin UI event belongs to a stale surface".into());
            }
            if instance.event_ids.contains(&event.event_id) {
                return Ok(());
            }
            instance.event_ids.insert(event.event_id.clone());
            instance.event_order.push_back(event.event_id.clone());
            if instance.event_order.len() > MAX_EVENT_IDS
                && let Some(expired) = instance.event_order.pop_front()
            {
                instance.event_ids.remove(&expired);
            }
            let id = event
                .node
                .parse::<u32>()
                .map_err(|_| "invalid remote node id")?;
            let control = instance
                .engine
                .as_ref()
                .map(|engine| engine.control())
                .ok_or_else(|| "plugin engine is not active".to_owned())?;
            let (kind, payload, scroll) = native_event(&event)?;
            (control, id, kind, payload, scroll)
        };
        if let Some((point, end)) = scroll {
            if end {
                control.dispatch_event(id, event_kind, EventPayload::Scroll(point));
            } else {
                control.scroll(id, point, 0);
            }
        } else {
            control.dispatch_event(id, event_kind, payload);
        }
        Ok(())
    }

    fn record_log(&self, key: &str, epoch: u64, body: serde_json::Value) {
        let body = sanitize_log(body);
        let accepted = {
            let mut inner = self.inner.lock().expect("plugin manager lock");
            let Some(instance) = inner.instances.get_mut(key) else {
                return;
            };
            if instance.epoch != epoch {
                return;
            }
            if instance.log_window.elapsed() >= Duration::from_secs(1) {
                instance.log_window = Instant::now();
                instance.log_count = 0;
            }
            if instance.log_count >= 32
                || serde_json::to_vec(&body).map_or(0, |bytes| bytes.len()) > 16 * 1024
            {
                instance.metrics.dropped_events += 1;
                return;
            }
            instance.log_count += 1;
            if instance.logs.len() >= 100 {
                instance.logs.pop_front();
                instance.metrics.dropped_events += 1;
            }
            instance.logs.push_back(body.clone());
            true
        };
        if accepted {
            self.emit_instance_envelope(
                key,
                (epoch, 0),
                PluginMessageKind::LogEvent,
                PluginStream::Logs,
                body,
                false,
            );
        }
    }

    pub(super) fn stop(&self, key: &str) {
        let instance = {
            let mut inner = self.inner.lock().expect("plugin manager lock");
            inner.retries.remove(key);
            inner.failed.remove(key);
            inner.instances.remove(key)
        };
        let Some(mut instance) = instance else {
            return;
        };
        self.retire_instance_surfaces(key);
        self.inner
            .lock()
            .expect("plugin manager lock")
            .retired_outgoing
            .insert(key.to_owned(), (instance.epoch, instance.outgoing.clone()));
        let epoch = instance.epoch;
        let generation = instance.generation;
        let runtime = instance.runtime(PluginLifecycleState::Stopped);
        let registered =
            instance.state == PluginLifecycleState::Active || instance.contribution_sync.is_some();
        discard_engine(instance.engine.take());
        for reply in self.gateway.cancel_instance(key, None) {
            self.send_service_reply(reply);
        }
        self.cancel_pending_tools(key, "the plugin instance stopped");
        if registered {
            self.clear_core_contributions(&instance.package, generation, epoch);
        }
        self.emit_instance_envelope(
            key,
            (epoch, generation),
            PluginMessageKind::UiReset,
            PluginStream::Ui,
            serde_json::json!({}),
            true,
        );
        self.emit_runtime(key, runtime, true);
    }

    fn clear_core_contributions(&self, package: &PluginPackageInfo, generation: u64, epoch: u64) {
        let agent = self
            .inner
            .lock()
            .expect("plugin manager lock")
            .agent
            .clone();
        let _ = agent.try_send(pig_protocol::Op::SyncPluginRuntimeContributions {
            request_id: format!("clear:{}:{}", epoch, next_event_id()),
            package: package.id.clone(),
            digest: package.digest.clone(),
            epoch,
            generation,
            contributions: PluginContributions::default(),
        });
    }

    fn cancel_pending_tools(&self, key: &str, message: &str) {
        let ids: Vec<String> = self
            .inner
            .lock()
            .expect("plugin manager lock")
            .pending
            .iter()
            .filter(|(_, pending)| pending.instance == key)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            let pending = self
                .inner
                .lock()
                .expect("plugin manager lock")
                .pending
                .remove(&id);
            if let Some(pending) = pending {
                self.gateway
                    .record_tool_completion(key, pending.epoch, false, true);
                self.finish_pending_tool_with_id(id, pending, Err(message.to_owned()));
            }
        }
    }

    fn finish_pending_tool_with_id(
        &self,
        request_id: String,
        pending: PendingCall,
        result: Result<serde_json::Value, String>,
    ) {
        let agent = self
            .inner
            .lock()
            .expect("plugin manager lock")
            .agent
            .clone();
        let error = result
            .as_ref()
            .err()
            .map(|detail| CoreError::PluginTransport {
                detail: detail.clone(),
            });
        let value = result.unwrap_or(serde_json::Value::Null);
        if pending.selection {
            let _ = agent.try_send(pig_protocol::Op::PluginToolsSelection {
                selection: PluginToolsSelection {
                    request_id,
                    session_id: pending.session_id,
                    package: pending.package,
                    digest: pending.digest,
                    result: match error {
                        Some(error) => Err(error),
                        None => serde_json::from_value(value).map_err(|error| {
                            CoreError::PluginTransport {
                                detail: error.to_string(),
                            }
                        }),
                    },
                },
            });
        } else {
            let _ = agent.try_send(pig_protocol::Op::PluginToolResult {
                result: PluginToolResult {
                    request_id,
                    session_id: pending.session_id,
                    package: pending.package,
                    digest: pending.digest,
                    result: error.map_or(Ok(value), Err),
                },
            });
        }
    }

    fn engine_failed(&self, key: &str, epoch: Option<u64>, message: String) {
        let removed = {
            let mut inner = self.inner.lock().expect("plugin manager lock");
            if epoch.is_some_and(|epoch| {
                inner
                    .instances
                    .get(key)
                    .is_some_and(|instance| instance.epoch != epoch)
            }) {
                return;
            }
            inner.instances.remove(key)
        };
        let Some(mut instance) = removed else {
            return;
        };
        self.retire_instance_surfaces(key);
        let actual_epoch = instance.epoch;
        self.inner
            .lock()
            .expect("plugin manager lock")
            .retired_outgoing
            .insert(key.to_owned(), (actual_epoch, instance.outgoing.clone()));
        let generation = instance.generation;
        let attempt = instance.attempt;
        let package = instance.package.clone();
        let registered =
            instance.state == PluginLifecycleState::Active || instance.contribution_sync.is_some();
        discard_engine(instance.engine.take());
        for reply in self.gateway.cancel_instance(key, Some(actual_epoch)) {
            self.send_service_reply(reply);
        }
        self.cancel_pending_tools(key, "the plugin engine failed");
        if registered {
            self.clear_core_contributions(&package, generation, actual_epoch);
        }
        self.emit_instance_envelope(
            key,
            (actual_epoch, generation),
            PluginMessageKind::UiReset,
            PluginStream::Ui,
            serde_json::json!({}),
            true,
        );
        let fallback = self
            .inner
            .lock()
            .expect("plugin manager lock")
            .instances
            .values()
            .any(|candidate| {
                candidate.package.id == package.id
                    && candidate.state == PluginLifecycleState::Active
            });
        if fallback {
            let agent = self
                .inner
                .lock()
                .expect("plugin manager lock")
                .agent
                .clone();
            let _ = agent.try_send(pig_protocol::Op::CancelPluginUpdate {
                package: package.id.clone(),
                digest: package.digest.clone(),
            });
        }
        if !fallback && attempt < MAX_RETRY_ATTEMPTS {
            let delay = Duration::from_millis(500 * (1 << attempt));
            let retry_at = Instant::now() + delay;
            self.inner
                .lock()
                .expect("plugin manager lock")
                .retries
                .insert(
                    key.to_owned(),
                    RetryInstance {
                        package,
                        generation,
                        attempt: attempt + 1,
                        at: retry_at,
                    },
                );
            let mut runtime = instance.runtime(PluginLifecycleState::Retrying);
            runtime.instance = key.to_owned();
            runtime.retry_after_ms = Some(delay.as_millis() as u64);
            runtime.message = Some(message);
            self.emit_runtime(key, runtime, true);
        } else {
            if !fallback {
                self.inner
                    .lock()
                    .expect("plugin manager lock")
                    .failed
                    .insert(key.to_owned());
            }
            let mut runtime = instance.runtime(PluginLifecycleState::Failed);
            runtime.instance = key.to_owned();
            runtime.message = Some(message);
            self.emit_runtime(key, runtime, true);
        }
    }

    fn retire_previous_versions(&self, key: &str) {
        let Some(package) = self.package_for_key(key) else {
            return;
        };
        let (previous, data_dir, agent) = {
            let inner = self.inner.lock().expect("plugin manager lock");
            let previous: Vec<(String, PluginPackageInfo)> = inner
                .instances
                .iter()
                .filter(|(candidate, instance)| {
                    *candidate != key && instance.package.id == package.id
                })
                .map(|(candidate, instance)| (candidate.clone(), instance.package.clone()))
                .collect();
            (previous, inner.data_dir.clone(), inner.agent.clone())
        };
        if previous.is_empty() {
            return;
        }
        for (candidate, _) in &previous {
            self.stop(candidate);
        }
        let _ = agent.try_send(pig_protocol::Op::CommitPluginUpdate {
            package: package.id.clone(),
            digest: package.digest.clone(),
        });
        for (_, old) in previous {
            let root = data_dir.join("packages").join(&old.id).join(&old.digest);
            if root.exists() {
                let _ = std::fs::remove_dir_all(root);
            }
        }
        let _ = agent.try_send(pig_protocol::Op::SyncPlugins {
            packages: super::loader::installed_sources(&data_dir),
        });
    }

    fn start_maintenance(&self) {
        let weak: Weak<Mutex<Inner>> = Arc::downgrade(&self.inner);
        let gateway = self.gateway.clone();
        let events = self.events.clone();
        let surface_work = self.surface_work.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(250));
                let Some(inner) = weak.upgrade() else {
                    break;
                };
                let manager = PluginSystemManager {
                    inner,
                    events: events.clone(),
                    receiver: async_channel::bounded(1).1,
                    gateway: gateway.clone(),
                    surface_work: surface_work.clone(),
                };
                manager.maintenance();
            }
        });
    }

    fn maintenance(&self) {
        self.flush_frames();
        for reply in self.gateway.expire() {
            self.send_service_reply(reply);
        }
        self.refresh_subscriptions();
        let now = Instant::now();
        let retries: Vec<RetryInstance> = {
            let mut inner = self.inner.lock().expect("plugin manager lock");
            let ready: Vec<String> = inner
                .retries
                .iter()
                .filter(|(_, retry)| retry.at <= now)
                .map(|(key, _)| key.clone())
                .collect();
            ready
                .into_iter()
                .filter_map(|key| inner.retries.remove(&key))
                .collect()
        };
        for retry in retries {
            self.start_with_attempt(retry.package, retry.generation, retry.attempt);
        }
        let checks: Vec<(String, u64, bool, bool)> = self
            .inner
            .lock()
            .expect("plugin manager lock")
            .instances
            .iter()
            .filter(|(_, instance)| instance.engine.is_some())
            .map(|(key, instance)| {
                (
                    key.clone(),
                    instance.epoch,
                    instance.state == PluginLifecycleState::Starting
                        && instance.started_at.elapsed() > START_TIMEOUT,
                    instance.state == PluginLifecycleState::Active
                        && instance.last_health.elapsed() > HEALTH_TIMEOUT,
                )
            })
            .collect();
        for (key, epoch, start_expired, health_expired) in checks {
            if start_expired || health_expired {
                self.engine_failed(
                    &key,
                    Some(epoch),
                    "plugin engine health check timed out".into(),
                );
                continue;
            }
            let send = self
                .inner
                .lock()
                .expect("plugin manager lock")
                .instances
                .get_mut(&key)
                .is_some_and(|instance| {
                    if instance.state == PluginLifecycleState::Active
                        && instance.last_health.elapsed() >= HEALTH_INTERVAL
                    {
                        instance.health_token += 1;
                        true
                    } else {
                        false
                    }
                });
            if send {
                let token = self
                    .inner
                    .lock()
                    .expect("plugin manager lock")
                    .instances
                    .get(&key)
                    .map(|instance| instance.health_token)
                    .unwrap_or_default();
                self.send_to_instance(
                    &key,
                    PluginMessageKind::LifecycleHealth,
                    PluginStream::Lifecycle,
                    None,
                    serde_json::json!({"token": token}),
                );
                let runtime = self
                    .inner
                    .lock()
                    .expect("plugin manager lock")
                    .instances
                    .get(&key)
                    .map(|instance| instance.runtime(PluginLifecycleState::Active));
                if let Some(runtime) = runtime {
                    self.emit_runtime(&key, runtime, false);
                }
            }
        }
        let timeouts: Vec<String> = self
            .inner
            .lock()
            .expect("plugin manager lock")
            .pending
            .iter()
            .filter(|(_, pending)| pending.deadline <= now)
            .map(|(id, _)| id.clone())
            .collect();
        for id in timeouts {
            if let Some(pending) = self
                .inner
                .lock()
                .expect("plugin manager lock")
                .pending
                .remove(&id)
            {
                self.gateway
                    .record_tool_completion(&pending.instance, pending.epoch, true, false);
                self.finish_pending_tool_with_id(
                    id,
                    pending,
                    Err("plugin tool call timed out".into()),
                );
            }
        }
    }

    fn emit_runtime(&self, key: &str, mut runtime: PluginRuntimeSnapshot, critical: bool) {
        runtime.instance = key.to_owned();
        let (network_bytes, timed_out_requests, cancelled_requests) =
            self.gateway.usage(key, runtime.epoch);
        let surface_pending = self.surface_pending_count(key, runtime.epoch);
        let pending = {
            let inner = self.inner.lock().expect("plugin manager lock");
            inner
                .pending
                .values()
                .filter(|pending| pending.instance == key && pending.epoch == runtime.epoch)
                .count()
                + self.gateway.pending_count(key, runtime.epoch)
                + surface_pending
        };
        let queue_depth = pending
            + usize::from(
                self.inner
                    .lock()
                    .expect("plugin manager lock")
                    .instances
                    .get(key)
                    .is_some_and(|instance| instance.pending_frame.is_some()),
            );
        runtime.metrics.pending_requests = pending;
        runtime.metrics.queue_depth = queue_depth;
        runtime.metrics.network_bytes = network_bytes;
        runtime.metrics.timed_out_requests = timed_out_requests;
        runtime.metrics.cancelled_requests = cancelled_requests;
        runtime.metrics.storage_bytes = self
            .package_for_key(key)
            .map(|package| self.gateway.storage_bytes(&package))
            .unwrap_or(0);
        self.emit_instance_envelope(
            key,
            (runtime.epoch, runtime.generation),
            PluginMessageKind::Runtime,
            PluginStream::Lifecycle,
            serde_json::to_value(runtime).unwrap_or_default(),
            critical,
        );
    }

    pub(super) fn emit_instance_envelope(
        &self,
        key: &str,
        identity: (u64, u64),
        kind: PluginMessageKind,
        stream: PluginStream,
        body: serde_json::Value,
        critical: bool,
    ) {
        let (epoch, generation) = identity;
        let sequence = {
            let mut inner = self.inner.lock().expect("plugin manager lock");
            if let Some(instance) = inner
                .instances
                .get_mut(key)
                .filter(|instance| instance.epoch == epoch)
            {
                instance.outgoing.next(stream)
            } else {
                let retired = inner
                    .retired_outgoing
                    .entry(key.to_owned())
                    .or_insert_with(|| (epoch, PluginSequenceTracker::default()));
                if retired.0 != epoch {
                    *retired = (epoch, PluginSequenceTracker::default());
                }
                retired.1.next(stream)
            }
        };
        let envelope = PluginEnvelope::new(kind, key, epoch, stream, sequence, generation, body);
        let encoded = serde_json::to_string(&envelope)
            .unwrap_or_else(|error| serde_json::json!({"error": error.to_string()}).to_string());
        if critical {
            let sender = self.events.clone();
            std::thread::spawn(move || {
                let _ = sender.send_blocking(encoded);
            });
        } else {
            let _ = self.events.try_send(encoded);
        }
    }

    fn package_for_key(&self, key: &str) -> Option<PluginPackageInfo> {
        self.inner
            .lock()
            .expect("plugin manager lock")
            .instances
            .get(key)
            .map(|instance| instance.package.clone())
    }

    fn instance_key_for_package(&self, id: &str, digest: &str) -> Option<String> {
        self.inner
            .lock()
            .expect("plugin manager lock")
            .instances
            .iter()
            .find(|(_, instance)| instance.package.id == id && instance.package.digest == digest)
            .map(|(key, _)| key.clone())
    }
}

fn discard_engine(engine: Option<Engine>) {
    if let Some(engine) = engine {
        std::thread::spawn(move || drop(engine));
    }
}

struct UiSink {
    manager: PluginSystemManager,
    instance: String,
    epoch: u64,
    generation: u64,
    sequence: std::sync::atomic::AtomicU64,
    previous: Mutex<Option<RemoteDocument>>,
    resource_prefix: String,
}

impl MountSink for UiSink {
    fn submit(&self, batch: MountBatch) {
        let sequence = self
            .sequence
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        let result = {
            let previous = self.previous.lock().expect("remote document lock");
            project_batch(
                batch,
                &self.instance,
                self.epoch,
                self.generation,
                sequence,
                previous.as_ref(),
                &self.resource_prefix,
            )
        };
        match result {
            Ok(document) => {
                *self.previous.lock().expect("remote document lock") = Some(document.clone());
                self.manager
                    .accept_document(&self.instance, self.epoch, document);
            }
            Err(error) => self
                .manager
                .engine_failed(&self.instance, Some(self.epoch), error),
        }
    }

    fn report_error(&self, message: &str) {
        self.manager
            .engine_failed(&self.instance, Some(self.epoch), message.to_owned());
    }
}

fn instance_key(package: &PluginPackageInfo, _workspace: &Path) -> String {
    match &package.source {
        PluginSource::BuiltIn => format!("{}:{}:built-in:global", package.id, package.digest),
        PluginSource::User => format!("{}:{}:user:default", package.id, package.digest),
        PluginSource::Project { workspace } => {
            format!(
                "{}:{}:project:{}",
                package.id,
                package.digest,
                workspace.display()
            )
        }
    }
}

pub(super) fn plugin_data_key(package: &PluginPackageInfo) -> String {
    match &package.source {
        PluginSource::BuiltIn => format!("{}:built-in:global", package.id),
        PluginSource::User => format!("{}:user:default", package.id),
        PluginSource::Project { workspace } => {
            format!("{}:project:{}", package.id, workspace.display())
        }
    }
}

fn merge_unique<T, F>(items: &mut Vec<T>, delta: Vec<T>, id: F)
where
    F: Fn(&T) -> String,
{
    for item in delta {
        let item_id = id(&item);
        items.retain(|existing| id(existing) != item_id);
        items.push(item);
    }
    items.sort_by_key(id);
}

fn validate_delta(delta: &PluginContributions) -> Result<(), String> {
    if !delta.credentials.is_empty()
        || !delta.http.is_empty()
        || !delta.actions.is_empty()
        || !delta.config.items.is_empty()
        || !delta.config.read.is_empty()
        || !delta.config.write.is_empty()
    {
        return Err("runtime contributions cannot alter service permissions".into());
    }
    let count = delta.pages.len()
        + delta.ui.len()
        + delta.themes.len()
        + delta.skills.len()
        + delta.mcp.len()
        + delta.tools.len()
        + delta.commands.len();
    if count > 256 {
        return Err("a contribution batch may contain at most 256 entries".into());
    }
    for contribution in &delta.ui {
        validate_ui_contribution(contribution)?;
    }
    Ok(())
}

fn validate_ui_contribution(contribution: &PluginUiContribution) -> Result<(), String> {
    if contribution.id.trim().is_empty()
        || [
            "approval.",
            "security.",
            "credentials.",
            "plugin.manager",
            "plugins.",
        ]
        .iter()
        .any(|prefix| contribution.target.starts_with(prefix))
    {
        return Err("the UI contribution target is not allowed".into());
    }
    Ok(())
}

type NativeEvent = (EventKind, EventPayload, Option<(Point<Points>, bool)>);

fn native_event(event: &PluginUiEvent) -> Result<NativeEvent, String> {
    let payload = &event.payload;
    let text = payload
        .get("text")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let number = |name: &str| {
        payload
            .get(name)
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0)
    };
    let field = |value: &serde_json::Value, name: &str| {
        value
            .get(name)
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0)
    };
    let point = || {
        let offset = payload.get("offset").unwrap_or(payload);
        Point::new(
            Points(field(offset, "x") as f32),
            Points(field(offset, "y") as f32),
        )
    };
    let bounds = |value: &serde_json::Value| {
        let origin = value.get("origin").cloned().unwrap_or_default();
        let size = value.get("size").cloned().unwrap_or_default();
        nsh_core::Bounds::new(
            Point::new(
                Points(field(&origin, "x") as f32),
                Points(field(&origin, "y") as f32),
            ),
            nsh_core::Size::new(
                Points(field(&size, "width") as f32),
                Points(field(&size, "height") as f32),
            ),
        )
    };
    let press = || {
        let position = payload.get("position").cloned().unwrap_or_default();
        let page_position = payload.get("pagePosition").cloned().unwrap_or_default();
        let modifiers = payload.get("modifiers").cloned().unwrap_or_default();
        let modifier = |name: &str| {
            modifiers
                .get(name)
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        };
        PressEvent {
            position: Point::new(
                Points(field(&position, "x") as f32),
                Points(field(&position, "y") as f32),
            ),
            page_position: Point::new(
                Points(field(&page_position, "x") as f32),
                Points(field(&page_position, "y") as f32),
            ),
            modifiers: nsh_core::keymap::Modifiers {
                ctrl: modifier("ctrl"),
                alt: modifier("alt"),
                shift: modifier("shift"),
                cmd: modifier("cmd"),
            },
            clicks: payload
                .get("clickCount")
                .or_else(|| payload.get("clicks"))
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0)
                .min(u32::MAX as u64) as u32,
        }
    };
    let event = match event.kind.as_str() {
        "press" => (EventKind::Press, EventPayload::Press(press()), None),
        "longPress" => (EventKind::LongPress, EventPayload::Press(press()), None),
        "pressDown" => (EventKind::PressDown, EventPayload::Press(press()), None),
        "pressUp" => (EventKind::PressUp, EventPayload::Press(press()), None),
        "changeText" => (
            EventKind::ChangeText,
            EventPayload::Text {
                text,
                count: number("count") as u32,
            },
            None,
        ),
        "submit" => (
            EventKind::SubmitEditing,
            EventPayload::Text {
                text,
                count: number("count") as u32,
            },
            None,
        ),
        "focus" => (EventKind::Focus, EventPayload::None, None),
        "blur" => (EventKind::Blur, EventPayload::None, None),
        "selectionChange" => (
            EventKind::SelectionChange,
            EventPayload::Map(vec![
                ("start", PayloadValue::Number(number("start"))),
                ("end", PayloadValue::Number(number("end"))),
            ]),
            None,
        ),
        "keyPress" => (
            EventKind::KeyPress,
            EventPayload::Map(vec![("key", PayloadValue::Str(text_or_key(payload)))]),
            None,
        ),
        "compositionChange" => (
            EventKind::CompositionChange,
            EventPayload::Map(vec![
                (
                    "composing",
                    PayloadValue::Bool(
                        payload
                            .get("composing")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false),
                    ),
                ),
                ("text", PayloadValue::Str(text)),
            ]),
            None,
        ),
        "contentSizeChange" => (
            EventKind::ContentSizeChange,
            EventPayload::Map(vec![
                ("width", PayloadValue::Number(number("width"))),
                ("height", PayloadValue::Number(number("height"))),
            ]),
            None,
        ),
        "scroll" => (
            EventKind::Scroll,
            EventPayload::None,
            Some((point(), false)),
        ),
        "scrollEnd" => (
            EventKind::ScrollEnd,
            EventPayload::Scroll(point()),
            Some((point(), true)),
        ),
        "scrollDragStart" => (
            EventKind::ScrollDragStart,
            EventPayload::Scroll(point()),
            None,
        ),
        "scrollDragEnd" => (
            EventKind::ScrollDragEnd,
            EventPayload::Scroll(point()),
            None,
        ),
        "momentumStart" => (
            EventKind::MomentumStart,
            EventPayload::Scroll(point()),
            None,
        ),
        "momentumEnd" => (EventKind::MomentumEnd, EventPayload::Scroll(point()), None),
        "caretMove" => (
            EventKind::CaretMove,
            EventPayload::caret(bounds(payload.get("bounds").unwrap_or(payload))),
            None,
        ),
        "layout" => (
            EventKind::Layout,
            EventPayload::Layout(bounds(payload)),
            None,
        ),
        "mouseMove" | "hoverMove" => (
            EventKind::MouseMove,
            EventPayload::Mouse(nsh_core::MouseEvent::new(
                point(),
                point(),
                Default::default(),
                None,
            )),
            None,
        ),
        "hoverIn" => (
            EventKind::MouseEnter,
            EventPayload::Mouse(nsh_core::MouseEvent::new(
                point(),
                point(),
                Default::default(),
                None,
            )),
            None,
        ),
        "hoverOut" => (
            EventKind::MouseLeave,
            EventPayload::Mouse(nsh_core::MouseEvent::new(
                point(),
                point(),
                Default::default(),
                None,
            )),
            None,
        ),
        "accessibilityAction" => (
            EventKind::AccessibilityAction,
            EventPayload::Map(vec![(
                "actionName",
                PayloadValue::Str(
                    payload
                        .get("actionName")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                ),
            )]),
            None,
        ),
        other => return Err(format!("unsupported plugin UI event: {other}")),
    };
    Ok(event)
}

fn text_or_key(payload: &serde_json::Value) -> String {
    payload
        .get("key")
        .or_else(|| payload.get("text"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn sanitize_log(mut value: serde_json::Value) -> serde_json::Value {
    match &mut value {
        serde_json::Value::Object(fields) => {
            if fields.len() > 64 {
                fields.clear();
                fields.insert("truncated".into(), true.into());
            }
            for (key, value) in fields {
                if [
                    "authorization",
                    "token",
                    "secret",
                    "apikey",
                    "api_key",
                    "password",
                    "content",
                    "body",
                    "session",
                    "history",
                    "prompt",
                    "request",
                    "response",
                ]
                .contains(&key.to_ascii_lowercase().as_str())
                {
                    *value = serde_json::Value::String("${PIG_REDACTED}".into());
                } else {
                    *value = sanitize_log(std::mem::take(value));
                }
            }
        }
        serde_json::Value::Array(items) => {
            items.truncate(64);
            for item in items {
                *item = sanitize_log(std::mem::take(item));
            }
        }
        serde_json::Value::String(text) if text.len() > 4096 => {
            *text = text.chars().take(4096).collect();
        }
        _ => {}
    }
    value
}

pub(super) fn next_event_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
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
    fn instance_and_data_identities_have_distinct_digest_and_scope_rules() {
        let package = PluginPackageInfo {
            id: "moon".into(),
            name: "Moon".into(),
            version: "1".into(),
            digest: "sha256:test".into(),
            content_digest: "sha256:content".into(),
            root: PathBuf::new(),
            source: PluginSource::User,
            enabled: true,
            contributions: PluginContributions::default(),
            capabilities: Vec::new(),
        };
        let user = instance_key(&package, Path::new("/workspace"));
        assert!(user.ends_with(":user:default"));
        let updated = PluginPackageInfo {
            digest: "sha256:updated".into(),
            ..package.clone()
        };
        assert_ne!(
            instance_key(&package, Path::new("/workspace")),
            instance_key(&updated, Path::new("/workspace"))
        );
        assert_eq!(plugin_data_key(&package), plugin_data_key(&updated));
        let project = PluginPackageInfo {
            source: PluginSource::Project {
                workspace: PathBuf::from("/workspace"),
            },
            ..package.clone()
        };
        assert_ne!(plugin_data_key(&package), plugin_data_key(&project));
        let first = instance_key(&project, Path::new("/workspace"));
        let other_project = PluginPackageInfo {
            source: PluginSource::Project {
                workspace: PathBuf::from("/other"),
            },
            ..project
        };
        let second = instance_key(&other_project, Path::new("/other"));
        assert_ne!(user, first);
        assert_ne!(first, second);
    }

    #[test]
    fn logs_redact_secrets_recursively_and_bound_size() {
        let value = sanitize_log(serde_json::json!({
            "authorization": "Bearer private",
            "nested": {"apiKey": "private"},
            "message": "visible",
        }));
        assert!(!value.to_string().contains("private"));
        assert_eq!(value["message"], "visible");
    }
}
