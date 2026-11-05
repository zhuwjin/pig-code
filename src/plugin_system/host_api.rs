use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use nsh_engine::{HostChannel, HostReply};
use pig_protocol::{
    Event, PLUGIN_HOST_API_VERSION, PluginErrorCode, PluginHostContext, PluginHostMethod,
    PluginHostRequest, PluginHostResponse, PluginLifecycleState, PluginPackageInfo,
    PluginProtocolError, PluginService, PluginServiceRequest,
};
use serde_json::{Value as JsonValue, json};

use super::manager::{Inner, PluginSystemManager, next_event_id, plugin_data_key};
use super::services::{ServiceExecution, ServiceReply, capability_granted, storage_path};

const SURFACE_CHANNEL: &str = "pig.host";
const MAX_SURFACE_REQUESTS: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SurfaceDescriptor {
    pub instance: String,
    pub surface: String,
    pub contribution: String,
    pub revision: u64,
    pub package_root: PathBuf,
    pub storage_dir: PathBuf,
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
}

impl SurfaceDescriptor {
    fn params(&self) -> serde_json::Map<String, JsonValue> {
        json!({
            "plugin": {
                "apiVersion": PLUGIN_HOST_API_VERSION,
                "instance": self.instance,
                "surface": self.surface,
                "contribution": self.contribution,
                "revision": self.revision,
                "context": {
                    "sessionId": self.session_id,
                    "turnId": self.turn_id,
                },
            }
        })
        .as_object()
        .expect("surface host params")
        .clone()
    }
}

pub struct SurfaceMount {
    pub manager: PluginSystemManager,
    pub descriptor: SurfaceDescriptor,
}

pub struct SurfaceWork {
    pub request: PluginHostRequest,
    pub reply: Option<HostReply>,
}

pub struct SurfaceState {
    pub descriptor: SurfaceDescriptor,
    pub epoch: u64,
    pub generation: u64,
    pub host: Option<HostChannel>,
    pub subscription: Option<SurfaceSubscription>,
}

pub struct SurfaceSubscription {
    pub id: String,
    pub session_id: String,
    pub revision: u64,
    pub pending: bool,
    pub dirty: bool,
}

pub struct PendingSurfaceRequest {
    pub descriptor: SurfaceDescriptor,
    pub request_id: String,
    pub reply: Option<HostReply>,
    pub operation: SurfaceOperation,
}

pub enum SurfaceOperation {
    SessionSnapshot,
    SessionSubscribe,
    SessionRefresh(String),
    Storage(PluginService),
    Navigation,
}

fn pending_mounts() -> &'static Mutex<BTreeMap<String, SurfaceMount>> {
    static MOUNTS: OnceLock<Mutex<BTreeMap<String, SurfaceMount>>> = OnceLock::new();
    MOUNTS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

pub fn attach(surface: &str, host: HostChannel) -> Result<SurfaceMount, String> {
    let mount = pending_mounts()
        .lock()
        .expect("pending surface lock")
        .remove(surface)
        .ok_or_else(|| "plugin surface mount is stale or unknown".to_owned())?;
    if let Err(error) = mount.manager.attach_host(&mount.descriptor, host.clone()) {
        mount.manager.unmount_surface(surface);
        return Err(error);
    }
    let weak = Arc::downgrade(&mount.manager.inner);
    let events = mount.manager.events.clone();
    let receiver = mount.manager.receiver.clone();
    let gateway = mount.manager.gateway.clone();
    let work = mount.manager.surface_work.clone();
    host.handle(SURFACE_CHANNEL, move |mut call| {
        let data = call.take_data();
        let reply = call.into_reply();
        let Some(inner) = weak.upgrade() else {
            respond_error(
                reply,
                "",
                "",
                0,
                error(PluginErrorCode::NotActive, "the plugin host stopped"),
            );
            return;
        };
        let manager = PluginSystemManager {
            inner,
            events: events.clone(),
            receiver: receiver.clone(),
            gateway: gateway.clone(),
            surface_work: work.clone(),
        };
        let request = serde_json::from_value::<PluginHostRequest>(data)
            .map_err(|parse| error(PluginErrorCode::InvalidEnvelope, parse.to_string()));
        match request {
            Ok(request) => manager.enqueue(request, reply),
            Err(parse) => respond_error(reply, "", "", 0, parse),
        }
    });
    Ok(mount)
}

pub fn detach(surface: &str) {
    let mount = pending_mounts()
        .lock()
        .expect("pending surface lock")
        .remove(surface);
    if let Some(mount) = mount {
        mount.manager.unmount_surface(surface);
    }
}

pub fn detach_manager(manager: &PluginSystemManager) {
    let surfaces: Vec<String> = pending_mounts()
        .lock()
        .expect("pending surface lock")
        .iter()
        .filter(|(_, mount)| Arc::ptr_eq(&mount.manager.inner, &manager.inner))
        .map(|(surface, _)| surface.clone())
        .collect();
    for surface in surfaces {
        detach(&surface);
    }
}

fn discard_pending(surface: &str) {
    pending_mounts()
        .lock()
        .expect("pending surface lock")
        .remove(surface);
}

impl PluginSystemManager {
    pub(super) fn start_surface_worker(&self, receiver: async_channel::Receiver<SurfaceWork>) {
        let weak = Arc::downgrade(&self.inner);
        let events = self.events.clone();
        let gateway = self.gateway.clone();
        let work = self.surface_work.downgrade();
        std::thread::spawn(move || {
            while let Ok(work_item) = receiver.recv_blocking() {
                let Some(inner) = weak.upgrade() else {
                    break;
                };
                let Some(work) = work.upgrade() else {
                    break;
                };
                let manager = PluginSystemManager {
                    inner,
                    events: events.clone(),
                    receiver: async_channel::bounded(1).1,
                    gateway: gateway.clone(),
                    surface_work: work,
                };
                manager.execute_surface(work_item);
            }
        });
    }

    pub fn mount_surface(
        &self,
        instance: &str,
        contribution: &str,
        context: PluginHostContext,
    ) -> Result<String, String> {
        if context.session_id.as_ref().is_some_and(|id| id.len() > 256)
            || context.turn_id.as_ref().is_some_and(|id| id.len() > 256)
        {
            return Err("plugin surface business id is too long".into());
        }
        let descriptor = {
            let mut inner = self.inner.lock().expect("plugin manager lock");
            let (package_root, data_key, epoch, generation) = {
                let plugin = inner
                    .instances
                    .get(instance)
                    .filter(|plugin| plugin.state == PluginLifecycleState::Active)
                    .ok_or_else(|| "plugin surface requires an active instance".to_owned())?;
                if plugin.package.ui_entry.is_none() {
                    return Err("plugin has no uiEntry".into());
                }
                let target = if let Some(page) = plugin
                    .package
                    .contributions
                    .pages
                    .iter()
                    .find(|page| page.id == contribution && page.enabled)
                {
                    match page.kind {
                        pig_protocol::PluginPageKind::Settings => "settings",
                        pig_protocol::PluginPageKind::Workspace => "workspace",
                    }
                } else if let Some(declared) = plugin
                    .package
                    .contributions
                    .ui
                    .iter()
                    .find(|item| item.id == contribution)
                {
                    if !matches!(
                        declared.kind,
                        pig_protocol::PluginUiKind::Popover | pig_protocol::PluginUiKind::Dialog
                    ) {
                        return Err("only pages, popovers and dialogs can mount a Surface".into());
                    }
                    match declared.slot {
                        pig_protocol::PluginUiSlot::Status => "status",
                        pig_protocol::PluginUiSlot::Composer => "composer",
                        pig_protocol::PluginUiSlot::Message => "message",
                    }
                } else {
                    return Err("plugin Surface contribution is not registered".into());
                };
                if !capability_granted(&plugin.package, "ui.add", target) {
                    return Err("plugin Surface target is not authorized".into());
                }
                (
                    plugin.package.root.clone(),
                    plugin_data_key(&plugin.package),
                    plugin.epoch,
                    plugin.engine_generation,
                )
            };
            let revision = next_revision(&mut inner).map_err(|error| error.to_string())?;
            let descriptor = SurfaceDescriptor {
                instance: instance.to_owned(),
                surface: format!("surface-{}-{}", std::process::id(), next_event_id()),
                contribution: contribution.to_owned(),
                revision,
                package_root,
                storage_dir: storage_path(&inner.data_dir, &data_key)
                    .parent()
                    .expect("plugin storage parent")
                    .join("nsh"),
                session_id: context.session_id.filter(|id| !id.is_empty()),
                turn_id: context.turn_id.filter(|id| !id.is_empty()),
            };
            inner.surfaces.insert(
                descriptor.surface.clone(),
                SurfaceState {
                    descriptor: descriptor.clone(),
                    epoch,
                    generation,
                    host: None,
                    subscription: None,
                },
            );
            descriptor
        };
        let registration = json!({
            "instance": descriptor.instance,
            "surface": descriptor.surface,
            "contribution": descriptor.contribution,
            "revision": descriptor.revision,
            "packageRoot": descriptor.package_root,
        });
        pending_mounts()
            .lock()
            .expect("pending surface lock")
            .insert(
                descriptor.surface.clone(),
                SurfaceMount {
                    manager: self.clone(),
                    descriptor,
                },
            );
        serde_json::to_string(&registration).map_err(|error| error.to_string())
    }

    fn attach_host(&self, descriptor: &SurfaceDescriptor, host: HostChannel) -> Result<(), String> {
        self.current_surface(&descriptor.surface)
            .map_err(|error| error.to_string())?;
        let mut inner = self.inner.lock().expect("plugin manager lock");
        let surface = inner
            .surfaces
            .get_mut(&descriptor.surface)
            .ok_or_else(|| "plugin surface is not mounted".to_owned())?;
        if surface.host.is_some() {
            return Err("plugin surface already has a host channel".into());
        }
        host.set_params(descriptor.params());
        surface.host = Some(host);
        Ok(())
    }

    pub fn unmount_surface(&self, surface: &str) {
        discard_pending(surface);
        let state = self
            .inner
            .lock()
            .expect("plugin manager lock")
            .surfaces
            .remove(surface);
        if let Some(state) = state {
            retire_channel(&state);
        }
        self.cancel_surface_requests(surface);
    }

    pub(super) fn retire_instance_surfaces(&self, instance: &str) {
        let surfaces: Vec<String> = self
            .inner
            .lock()
            .expect("plugin manager lock")
            .surfaces
            .iter()
            .filter(|(_, surface)| surface.descriptor.instance == instance)
            .map(|(id, _)| id.clone())
            .collect();
        for surface in surfaces {
            self.unmount_surface(&surface);
        }
    }

    fn current_surface(
        &self,
        id: &str,
    ) -> Result<(PluginPackageInfo, SurfaceDescriptor), PluginProtocolError> {
        let inner = self.inner.lock().expect("plugin manager lock");
        let surface = inner
            .surfaces
            .get(id)
            .ok_or_else(|| error(PluginErrorCode::NotFound, "plugin surface is not mounted"))?;
        let plugin = inner
            .instances
            .get(&surface.descriptor.instance)
            .filter(|plugin| {
                plugin.state == PluginLifecycleState::Active
                    && plugin.epoch == surface.epoch
                    && plugin.engine_generation == surface.generation
            })
            .ok_or_else(|| {
                error(
                    PluginErrorCode::NotActive,
                    "plugin surface instance is stale",
                )
            })?;
        Ok((plugin.package.clone(), surface.descriptor.clone()))
    }

    fn enqueue(&self, request: PluginHostRequest, reply: Option<HostReply>) {
        if request.request_id.is_empty()
            || request.request_id.len() > 256
            || serde_json::to_vec(&request.payload).map_or(0, |bytes| bytes.len()) > 1024 * 1024
        {
            respond_error(
                reply,
                &request.surface,
                &request.request_id,
                request.revision,
                error(
                    PluginErrorCode::QuotaExceeded,
                    "invalid request id or a Host API payload larger than 1 MiB",
                ),
            );
            return;
        }
        let surface = request.surface.clone();
        let request_id = request.request_id.clone();
        let revision = request.revision;
        if let Err(full) = self.surface_work.try_send(SurfaceWork { request, reply }) {
            let work = full.into_inner();
            respond_error(
                work.reply,
                &surface,
                &request_id,
                revision,
                error(PluginErrorCode::QuotaExceeded, "the Host API queue is full"),
            );
        }
    }

    pub(super) fn execute_surface(&self, work: SurfaceWork) {
        let SurfaceWork { request, reply } = work;
        let descriptor = match self.validate_request(&request) {
            Ok(descriptor) => descriptor,
            Err(problem) => {
                respond_error(
                    reply,
                    &request.surface,
                    &request.request_id,
                    request.revision,
                    problem,
                );
                return;
            }
        };
        if request.method == PluginHostMethod::OverlayClose {
            let identity = self
                .inner
                .lock()
                .expect("plugin manager lock")
                .surfaces
                .get(&descriptor.surface)
                .map(|surface| (surface.epoch, surface.generation));
            let Some(identity) = identity else {
                respond_error(
                    reply,
                    &descriptor.surface,
                    &request.request_id,
                    descriptor.revision,
                    error(PluginErrorCode::NotActive, "plugin Surface is not mounted"),
                );
                return;
            };
            self.emit_instance_envelope(
                &descriptor.instance,
                identity,
                pig_protocol::PluginMessageKind::HostAction,
                pig_protocol::PluginStream::Services,
                json!({"name": "overlay.close", "surface": descriptor.surface}),
                false,
            );
            respond(reply, &descriptor, &request.request_id, Ok(JsonValue::Null));
            return;
        }
        if request.method == PluginHostMethod::SessionUnsubscribe {
            let subscription_id = request
                .payload
                .get("subscriptionId")
                .and_then(JsonValue::as_str)
                .unwrap_or_default()
                .to_owned();
            let removed = self
                .inner
                .lock()
                .expect("plugin manager lock")
                .surfaces
                .get_mut(&descriptor.surface)
                .and_then(|surface| {
                    if surface
                        .subscription
                        .as_ref()
                        .is_some_and(|subscription| subscription.id == subscription_id)
                    {
                        surface.subscription.take()
                    } else {
                        None
                    }
                });
            let starting = self
                .inner
                .lock()
                .expect("plugin manager lock")
                .surface_requests
                .iter()
                .find(|(_, pending)| {
                    pending.descriptor.surface == descriptor.surface
                        && pending.request_id == subscription_id
                        && matches!(pending.operation, SurfaceOperation::SessionSubscribe)
                })
                .map(|(id, _)| id.clone());
            if let Some(subscription) = removed {
                self.cancel_refresh(&descriptor.surface, &subscription.id);
                respond(
                    reply,
                    &descriptor,
                    &request.request_id,
                    Ok(json!({"unsubscribed": true})),
                );
            } else if let Some(starting) = starting {
                self.cancel_requests(&[starting]);
                respond(
                    reply,
                    &descriptor,
                    &request.request_id,
                    Ok(json!({"unsubscribed": true})),
                );
            } else {
                respond_error(
                    reply,
                    &descriptor.surface,
                    &request.request_id,
                    descriptor.revision,
                    error(
                        PluginErrorCode::NotFound,
                        "session subscription is not active",
                    ),
                );
            }
            return;
        }
        let operation = match request.method {
            PluginHostMethod::SessionSnapshot => SurfaceOperation::SessionSnapshot,
            PluginHostMethod::SessionSubscribe => SurfaceOperation::SessionSubscribe,
            PluginHostMethod::StorageRead => SurfaceOperation::Storage(PluginService::StorageRead),
            PluginHostMethod::StorageWrite => {
                SurfaceOperation::Storage(PluginService::StorageWrite)
            }
            PluginHostMethod::NavigationOpen => SurfaceOperation::Navigation,
            PluginHostMethod::SessionUnsubscribe | PluginHostMethod::OverlayClose => unreachable!(),
        };
        self.start_surface_operation(
            descriptor,
            request.request_id,
            reply,
            operation,
            request.payload,
        );
    }

    fn validate_request(
        &self,
        request: &PluginHostRequest,
    ) -> Result<SurfaceDescriptor, PluginProtocolError> {
        if request.api_version != PLUGIN_HOST_API_VERSION {
            return Err(error(
                PluginErrorCode::UnsupportedProtocol,
                "unsupported Host API version",
            ));
        }
        let (_, descriptor) = self.current_surface(&request.surface)?;
        if descriptor.instance != request.instance
            || descriptor.contribution != request.contribution
            || descriptor.revision != request.revision
        {
            return Err(error(
                PluginErrorCode::StaleRevision,
                "plugin surface identity or revision is stale",
            ));
        }
        Ok(descriptor)
    }

    fn start_surface_operation(
        &self,
        descriptor: SurfaceDescriptor,
        external_id: String,
        reply: Option<HostReply>,
        operation: SurfaceOperation,
        payload: JsonValue,
    ) {
        let (package, _) = match self.current_surface(&descriptor.surface) {
            Ok(current) => current,
            Err(problem) => {
                respond_error(
                    reply,
                    &descriptor.surface,
                    &external_id,
                    descriptor.revision,
                    problem,
                );
                return;
            }
        };
        if matches!(operation, SurfaceOperation::SessionSubscribe) {
            let inner = self.inner.lock().expect("plugin manager lock");
            let occupied = inner
                .surfaces
                .get(&descriptor.surface)
                .is_some_and(|surface| surface.subscription.is_some())
                || inner.surface_requests.values().any(|pending| {
                    pending.descriptor.surface == descriptor.surface
                        && matches!(pending.operation, SurfaceOperation::SessionSubscribe)
                });
            drop(inner);
            if occupied {
                respond_error(
                    reply,
                    &descriptor.surface,
                    &external_id,
                    descriptor.revision,
                    error(
                        PluginErrorCode::InvalidOperation,
                        "a Surface can have only one session subscription",
                    ),
                );
                return;
            }
        }
        let (service, session_id) = match &operation {
            SurfaceOperation::SessionSnapshot
            | SurfaceOperation::SessionSubscribe
            | SurfaceOperation::SessionRefresh(_) => {
                let Some(session) = descriptor.session_id.clone() else {
                    respond_error(
                        reply,
                        &descriptor.surface,
                        &external_id,
                        descriptor.revision,
                        error(
                            PluginErrorCode::InvalidOperation,
                            "this Surface has no session context",
                        ),
                    );
                    return;
                };
                (PluginService::SessionRead, Some(session))
            }
            SurfaceOperation::Storage(service) => (*service, None),
            SurfaceOperation::Navigation => (PluginService::NavigationOpen, None),
        };
        let request_id = format!("surface-request-{}-{}", std::process::id(), next_event_id());
        let mut inner = self.inner.lock().expect("plugin manager lock");
        let duplicate = !matches!(operation, SurfaceOperation::SessionRefresh(_))
            && inner.surface_requests.values().any(|pending| {
                pending.descriptor.surface == descriptor.surface
                    && pending.request_id == external_id
            });
        if duplicate {
            drop(inner);
            respond_error(
                reply,
                &descriptor.surface,
                &external_id,
                descriptor.revision,
                error(
                    PluginErrorCode::InvalidOperation,
                    "requestId is already pending for this Surface",
                ),
            );
            return;
        }
        let pending = inner
            .surface_requests
            .values()
            .filter(|pending| pending.descriptor.surface == descriptor.surface)
            .count();
        if pending >= MAX_SURFACE_REQUESTS {
            drop(inner);
            respond_error(
                reply,
                &descriptor.surface,
                &external_id,
                descriptor.revision,
                error(
                    PluginErrorCode::QuotaExceeded,
                    "too many pending requests for this Surface",
                ),
            );
            return;
        }
        inner.surface_requests.insert(
            request_id.clone(),
            PendingSurfaceRequest {
                descriptor: descriptor.clone(),
                request_id: external_id,
                reply,
                operation,
            },
        );
        drop(inner);
        let request = PluginServiceRequest {
            service,
            payload,
            session_id,
            workspace_id: None,
        };
        let epoch = self
            .inner
            .lock()
            .expect("plugin manager lock")
            .surfaces
            .get(&descriptor.surface)
            .map(|surface| (surface.epoch, surface.generation));
        let Some((epoch, generation)) = epoch else {
            self.cancel_surface_requests(&descriptor.surface);
            return;
        };
        match self.gateway.execute(
            &package,
            &descriptor.instance,
            epoch,
            generation,
            &request_id,
            request,
        ) {
            ServiceExecution::Complete(reply) => self.complete_surface_request(reply),
            ServiceExecution::Forwarded => {}
            ServiceExecution::Action(reply, action) => {
                self.complete_surface_request(reply);
                self.emit_instance_envelope(
                    &descriptor.instance,
                    (epoch, generation),
                    pig_protocol::PluginMessageKind::HostAction,
                    pig_protocol::PluginStream::Services,
                    action,
                    false,
                );
            }
        }
    }

    pub(super) fn complete_surface_request(&self, reply: ServiceReply) {
        let pending = self
            .inner
            .lock()
            .expect("plugin manager lock")
            .surface_requests
            .remove(&reply.request_id);
        let Some(pending) = pending else {
            return;
        };
        if self.current_surface(&pending.descriptor.surface).is_err() {
            respond_error(
                pending.reply,
                &pending.descriptor.surface,
                &pending.request_id,
                pending.descriptor.revision,
                error(
                    PluginErrorCode::Cancelled,
                    "the Surface request was cancelled",
                ),
            );
            return;
        }
        match pending.operation {
            SurfaceOperation::SessionSnapshot => {
                let result = reply
                    .result
                    .map(|value| json!({"session": value["session"]}));
                respond(
                    pending.reply,
                    &pending.descriptor,
                    &pending.request_id,
                    result,
                );
            }
            SurfaceOperation::SessionSubscribe => match reply.result {
                Ok(value) => {
                    let Some(session_id) = pending.descriptor.session_id.clone() else {
                        return;
                    };
                    let mut inner = self.inner.lock().expect("plugin manager lock");
                    let revision = match next_revision(&mut inner) {
                        Ok(revision) => revision,
                        Err(problem) => {
                            drop(inner);
                            respond_error(
                                pending.reply,
                                &pending.descriptor.surface,
                                &pending.request_id,
                                pending.descriptor.revision,
                                problem,
                            );
                            return;
                        }
                    };
                    let subscription = SurfaceSubscription {
                        id: pending.request_id.clone(),
                        session_id,
                        revision,
                        pending: false,
                        dirty: false,
                    };
                    if let Some(surface) = inner.surfaces.get_mut(&pending.descriptor.surface) {
                        surface.subscription = Some(subscription);
                    }
                    drop(inner);
                    respond(
                        pending.reply,
                        &pending.descriptor,
                        &pending.request_id,
                        Ok(json!({
                            "subscriptionId": pending.request_id,
                            "revision": revision,
                            "session": value["session"],
                        })),
                    );
                }
                Err(problem) => respond(
                    pending.reply,
                    &pending.descriptor,
                    &pending.request_id,
                    Err(problem),
                ),
            },
            SurfaceOperation::SessionRefresh(subscription_id) => {
                let next = {
                    let mut inner = self.inner.lock().expect("plugin manager lock");
                    if reply.result.is_ok() {
                        Some(next_revision(&mut inner))
                    } else {
                        None
                    }
                };
                let (host, revision, result) = {
                    let mut inner = self.inner.lock().expect("plugin manager lock");
                    let Some(surface) = inner.surfaces.get_mut(&pending.descriptor.surface) else {
                        return;
                    };
                    let Some(subscription) = surface
                        .subscription
                        .as_mut()
                        .filter(|subscription| subscription.id == subscription_id)
                    else {
                        return;
                    };
                    subscription.pending = false;
                    let revision = match &next {
                        Some(Ok(revision)) => *revision,
                        Some(Err(_)) => 0,
                        None => subscription.revision,
                    };
                    if next.as_ref().is_some_and(Result::is_ok) {
                        subscription.revision = revision;
                    }
                    let result = match next {
                        Some(Err(problem)) => Err(problem),
                        _ => reply.result,
                    };
                    (surface.host.clone(), revision, result)
                };
                if let Some(host) = host {
                    let kind = if result.is_ok() {
                        "session.snapshot"
                    } else {
                        "session.error"
                    };
                    let _ = host.send(
                        SURFACE_CHANNEL,
                        json!({
                            "kind": kind,
                            "apiVersion": PLUGIN_HOST_API_VERSION,
                            "surface": pending.descriptor.surface,
                            "subscriptionId": subscription_id,
                            "revision": revision,
                            "session": result.as_ref().ok().map(|value| value["session"].clone()),
                            "error": result.err(),
                        }),
                    );
                }
            }
            SurfaceOperation::Storage(_) | SurfaceOperation::Navigation => respond(
                pending.reply,
                &pending.descriptor,
                &pending.request_id,
                reply.result,
            ),
        }
    }

    pub(super) fn note_session_event(&self, event: &Event) {
        let Some(session_id) = session_event_id(event) else {
            return;
        };
        let mut inner = self.inner.lock().expect("plugin manager lock");
        for surface in inner.surfaces.values_mut() {
            if let Some(subscription) = surface.subscription.as_mut()
                && subscription.session_id == session_id
            {
                subscription.dirty = true;
            }
        }
    }

    pub(super) fn refresh_subscriptions(&self) {
        let requests: Vec<(SurfaceDescriptor, String)> = self
            .inner
            .lock()
            .expect("plugin manager lock")
            .surfaces
            .values_mut()
            .filter_map(|surface| {
                let subscription = surface.subscription.as_mut()?;
                if !subscription.dirty || subscription.pending {
                    return None;
                }
                subscription.dirty = false;
                subscription.pending = true;
                Some((surface.descriptor.clone(), subscription.id.clone()))
            })
            .collect();
        for (descriptor, subscription) in requests {
            self.start_surface_operation(
                descriptor,
                subscription.clone(),
                None,
                SurfaceOperation::SessionRefresh(subscription),
                JsonValue::Null,
            );
        }
    }

    fn cancel_refresh(&self, surface: &str, subscription: &str) {
        let ids: Vec<String> = self
            .inner
            .lock()
            .expect("plugin manager lock")
            .surface_requests
            .iter()
            .filter(|(_, pending)| {
                pending.descriptor.surface == surface
                    && matches!(&pending.operation, SurfaceOperation::SessionRefresh(id) if id == subscription)
            })
            .map(|(id, _)| id.clone())
            .collect();
        self.cancel_requests(&ids);
    }

    fn cancel_surface_requests(&self, surface: &str) {
        let ids: Vec<String> = self
            .inner
            .lock()
            .expect("plugin manager lock")
            .surface_requests
            .iter()
            .filter(|(_, pending)| pending.descriptor.surface == surface)
            .map(|(id, _)| id.clone())
            .collect();
        self.cancel_requests(&ids);
    }

    fn cancel_requests(&self, ids: &[String]) {
        let pending: Vec<PendingSurfaceRequest> = {
            let mut inner = self.inner.lock().expect("plugin manager lock");
            ids.iter()
                .filter_map(|id| inner.surface_requests.remove(id))
                .collect()
        };
        for pending in pending {
            respond_error(
                pending.reply,
                &pending.descriptor.surface,
                &pending.request_id,
                pending.descriptor.revision,
                error(
                    PluginErrorCode::Cancelled,
                    "the Surface request was cancelled",
                ),
            );
        }
        for reply in self.gateway.cancel_requests(ids) {
            self.complete_surface_request(reply);
        }
    }

    pub(super) fn surface_pending_count(&self, instance: &str, epoch: u64) -> usize {
        let inner = self.inner.lock().expect("plugin manager lock");
        inner
            .surface_requests
            .values()
            .filter(|pending| {
                pending.descriptor.instance == instance
                    && inner
                        .surfaces
                        .get(&pending.descriptor.surface)
                        .is_some_and(|surface| surface.epoch == epoch)
            })
            .count()
    }
}

fn retire_channel(state: &SurfaceState) {
    if let Some(host) = &state.host {
        host.remove_handler(SURFACE_CHANNEL);
        let _ = host.send(
            SURFACE_CHANNEL,
            json!({
                "kind": "surface.disposed",
                "apiVersion": PLUGIN_HOST_API_VERSION,
                "surface": state.descriptor.surface,
                "revision": state.descriptor.revision,
            }),
        );
    }
}

fn next_revision(inner: &mut Inner) -> Result<u64, PluginProtocolError> {
    inner.surface_revision = inner.surface_revision.checked_add(1).ok_or_else(|| {
        error(
            PluginErrorCode::QuotaExceeded,
            "Surface revision quota exceeded",
        )
    })?;
    Ok(inner.surface_revision)
}

fn respond(
    reply: Option<HostReply>,
    descriptor: &SurfaceDescriptor,
    request_id: &str,
    result: Result<JsonValue, PluginProtocolError>,
) {
    if let Some(reply) = reply {
        reply.resolve(
            serde_json::to_value(PluginHostResponse {
                api_version: PLUGIN_HOST_API_VERSION,
                surface: descriptor.surface.clone(),
                revision: descriptor.revision,
                request_id: request_id.to_owned(),
                result,
            })
            .unwrap_or_default(),
        );
    }
}

fn respond_error(
    reply: Option<HostReply>,
    surface: &str,
    request_id: &str,
    revision: u64,
    problem: PluginProtocolError,
) {
    if let Some(reply) = reply {
        reply.resolve(
            serde_json::to_value(PluginHostResponse {
                api_version: PLUGIN_HOST_API_VERSION,
                surface: surface.to_owned(),
                revision,
                request_id: request_id.to_owned(),
                result: Err(problem),
            })
            .unwrap_or_default(),
        );
    }
}

fn error(code: PluginErrorCode, message: impl Into<String>) -> PluginProtocolError {
    PluginProtocolError::new(code, message)
}

fn session_event_id(event: &Event) -> Option<&str> {
    match event {
        Event::SessionConfigured { session_id, .. }
        | Event::TurnStarted { session_id, .. }
        | Event::TurnComplete { session_id, .. }
        | Event::TurnAborted { session_id }
        | Event::ContextUsage { session_id, .. }
        | Event::ContextBreakdownChanged { session_id, .. }
        | Event::ToolCallBegin { session_id, .. }
        | Event::ToolCallEnd { session_id, .. } => Some(session_id),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
