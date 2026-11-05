use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pig_protocol::{
    CoreError, Event, PluginAction, PluginErrorCode, PluginPackageInfo, PluginProtocolError,
    PluginResource, PluginScope, PluginService, PluginServiceRequest, PluginSource, SecretString,
};

const MAX_PENDING_SERVICES: usize = 32;
const MAX_STORAGE_BYTES: usize = 1024 * 1024;
const MAX_RESOURCE_BYTES: usize = 4 * 1024 * 1024;
const SERVICE_TIMEOUT: Duration = Duration::from_secs(35);

#[derive(Clone)]
pub struct HostServiceGateway {
    data_dir: PathBuf,
    agent: async_channel::Sender<pig_protocol::Op>,
    pending: Arc<Mutex<BTreeMap<String, PendingService>>>,
    usage: Arc<Mutex<BTreeMap<(String, u64), ServiceUsage>>>,
    locale: Arc<Mutex<String>>,
}

#[derive(Default)]
struct ServiceUsage {
    network_bytes: u64,
    timed_out_requests: u64,
    cancelled_requests: u64,
}

struct PendingService {
    external_id: String,
    instance: String,
    epoch: u64,
    generation: u64,
    service: PluginService,
    network_bytes: u64,
    deadline: Instant,
}

pub struct ServiceReply {
    pub instance: String,
    pub epoch: u64,
    pub generation: u64,
    pub request_id: String,
    pub service: PluginService,
    pub result: Result<serde_json::Value, PluginProtocolError>,
}

pub enum ServiceExecution {
    Complete(ServiceReply),
    Forwarded,
    Action(ServiceReply, serde_json::Value),
}

impl HostServiceGateway {
    pub fn new(data_dir: PathBuf, agent: async_channel::Sender<pig_protocol::Op>) -> Self {
        Self {
            data_dir,
            agent,
            pending: Arc::new(Mutex::new(BTreeMap::new())),
            usage: Arc::new(Mutex::new(BTreeMap::new())),
            locale: Arc::new(Mutex::new(system_locale())),
        }
    }

    pub fn locale(&self) -> String {
        self.locale.lock().expect("plugin locale lock").clone()
    }

    pub fn set_locale(&self, locale: &str) -> bool {
        let normalized = normalize_locale(locale);
        let mut current = self.locale.lock().expect("plugin locale lock");
        if *current == normalized {
            return false;
        }
        *current = normalized;
        true
    }

    pub fn execute(
        &self,
        package: &PluginPackageInfo,
        instance: &str,
        epoch: u64,
        generation: u64,
        request_id: &str,
        request: PluginServiceRequest,
    ) -> ServiceExecution {
        if serde_json::to_vec(&request.payload).map_or(0, |bytes| bytes.len()) > 1024 * 1024 {
            return ServiceExecution::Complete(self.reply(
                instance,
                epoch,
                generation,
                request_id,
                request.service,
                Err(protocol_error(
                    PluginErrorCode::QuotaExceeded,
                    "plugin service payload exceeds 1 MiB",
                )),
            ));
        }
        if let Err(error) = authorize_request(package, &request) {
            return ServiceExecution::Complete(self.reply(
                instance,
                epoch,
                generation,
                request_id,
                request.service,
                Err(error),
            ));
        }
        match request.service {
            PluginService::StorageRead => ServiceExecution::Complete(self.reply(
                instance,
                epoch,
                generation,
                request_id,
                request.service,
                read_storage(&self.data_dir, &super::manager::plugin_data_key(package)),
            )),
            PluginService::StorageWrite => ServiceExecution::Complete(self.reply(
                instance,
                epoch,
                generation,
                request_id,
                request.service,
                write_storage(
                    &self.data_dir,
                    &super::manager::plugin_data_key(package),
                    &request.payload,
                ),
            )),
            PluginService::ResourceRead => ServiceExecution::Complete(self.reply(
                instance,
                epoch,
                generation,
                request_id,
                request.service,
                read_resource(package, &request.payload),
            )),
            PluginService::LocaleRead => ServiceExecution::Complete(self.reply(
                instance,
                epoch,
                generation,
                request_id,
                request.service,
                Ok(serde_json::json!({"locale": self.locale()})),
            )),
            PluginService::ComposerSubmit => {
                let reply = self.reply(
                    instance,
                    epoch,
                    generation,
                    request_id,
                    request.service,
                    Ok(serde_json::Value::Null),
                );
                ServiceExecution::Action(reply, serde_json::json!({"name": "composer.submit"}))
            }
            PluginService::NavigationOpen => {
                let page_id = request
                    .payload
                    .get("pageId")
                    .or_else(|| request.payload.get("page_id"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                if page_id.is_empty() || page_id.starts_with("plugins.") {
                    return ServiceExecution::Complete(self.reply(
                        instance,
                        epoch,
                        generation,
                        request_id,
                        request.service,
                        Err(protocol_error(
                            PluginErrorCode::InvalidOperation,
                            "invalid navigation target",
                        )),
                    ));
                }
                let contribution = request
                    .payload
                    .get("contribution")
                    .and_then(serde_json::Value::as_str);
                let turn_id = request
                    .payload
                    .get("turnId")
                    .and_then(serde_json::Value::as_str);
                let turn_index = request
                    .payload
                    .get("turnIndex")
                    .and_then(serde_json::Value::as_u64);
                let anchor = request
                    .payload
                    .get("anchor")
                    .and_then(serde_json::Value::as_str);
                let valid_overlay = page_id != "overlay"
                    || (contribution.is_some_and(|contribution| {
                        package.contributions.ui.iter().any(|item| {
                            item.id == contribution
                                && item.target == "plugin.overlay"
                                && item.kind == pig_protocol::PluginUiContributionKind::Surface
                        })
                    }) && anchor.is_some_and(|anchor| {
                        package.contributions.ui.iter().any(|item| {
                            item.target == anchor
                                && item.kind == pig_protocol::PluginUiContributionKind::Surface
                                && matches!(
                                    anchor,
                                    "status.activity"
                                        | "status.usage"
                                        | "status.context"
                                        | "message.turnStats"
                                )
                        })
                    }));
                if !valid_overlay || turn_id.is_some_and(|id| id.len() > 256) {
                    return ServiceExecution::Complete(self.reply(
                        instance,
                        epoch,
                        generation,
                        request_id,
                        request.service,
                        Err(protocol_error(
                            PluginErrorCode::InvalidOperation,
                            "plugin overlay contribution is not registered",
                        )),
                    ));
                }
                let reply = self.reply(
                    instance,
                    epoch,
                    generation,
                    request_id,
                    request.service,
                    Ok(serde_json::json!({"pageId": page_id})),
                );
                ServiceExecution::Action(
                    reply,
                    serde_json::json!({
                        "name": "navigation.open",
                        "pageId": page_id,
                        "contribution": contribution,
                        "turnId": turn_id,
                        "turnIndex": turn_index,
                        "anchor": anchor,
                    }),
                )
            }
            _ => self.forward(package, instance, epoch, generation, request_id, request),
        }
    }

    fn forward(
        &self,
        package: &PluginPackageInfo,
        instance: &str,
        epoch: u64,
        generation: u64,
        request_id: &str,
        request: PluginServiceRequest,
    ) -> ServiceExecution {
        let service = request.service;
        let failure = |error| {
            ServiceExecution::Complete(self.reply(
                instance,
                epoch,
                generation,
                request_id,
                service,
                Err(error),
            ))
        };
        {
            let pending = self.pending.lock().expect("plugin service pending lock");
            let concurrent = pending
                .values()
                .filter(|pending| pending.instance == instance && pending.epoch == epoch)
                .count();
            if concurrent >= MAX_PENDING_SERVICES {
                return failure(protocol_error(
                    PluginErrorCode::QuotaExceeded,
                    "too many concurrent plugin service requests",
                ));
            }
        }
        static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);
        let internal_id = format!(
            "plugin-service-{}-{}",
            std::process::id(),
            NEXT_REQUEST.fetch_add(1, Ordering::Relaxed)
        );
        let network_bytes = if service == PluginService::HttpRequest {
            serde_json::to_vec(&request.payload).map_or(0, |bytes| bytes.len() as u64)
        } else {
            0
        };
        let scope = match &package.source {
            PluginSource::Project { workspace } => PluginScope::Project {
                workspace: workspace.clone(),
            },
            _ => PluginScope::User,
        };
        let op = match service {
            PluginService::ConfigRead | PluginService::ConfigWrite => {
                let path = request
                    .payload
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let action = if service == PluginService::ConfigRead {
                    PluginAction::ReadConfig { path }
                } else {
                    PluginAction::WriteConfig {
                        path,
                        value: request.payload.get("value").cloned().unwrap_or_default(),
                    }
                };
                pig_protocol::Op::PluginAction {
                    request_id: internal_id.clone(),
                    package: package.id.clone(),
                    digest: package.digest.clone(),
                    action,
                    session_id: request.session_id,
                }
            }
            PluginService::HttpRequest => {
                let parsed = serde_json::from_value(request.payload).map_err(|error| {
                    protocol_error(PluginErrorCode::InvalidOperation, error.to_string())
                });
                match parsed {
                    Ok(request) => pig_protocol::Op::PluginHttpRequest {
                        request_id: internal_id.clone(),
                        package: package.id.clone(),
                        digest: package.digest.clone(),
                        request,
                    },
                    Err(parse_error) => return failure(parse_error),
                }
            }
            PluginService::CredentialSet | PluginService::CredentialClear => {
                let credential = request
                    .payload
                    .get("credential")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                if service == PluginService::CredentialSet {
                    let secret = request
                        .payload
                        .get("secret")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    if secret.is_empty() || secret.len() > 64 * 1024 {
                        return failure(protocol_error(
                            PluginErrorCode::InvalidOperation,
                            "a non-empty credential within 64 KiB is required",
                        ));
                    }
                    pig_protocol::Op::SetPluginSecret {
                        request_id: internal_id.clone(),
                        package: package.id.clone(),
                        digest: package.digest.clone(),
                        credential,
                        secret: SecretString::new(secret),
                    }
                } else {
                    pig_protocol::Op::ClearPluginSecret {
                        request_id: internal_id.clone(),
                        package: package.id.clone(),
                        digest: package.digest.clone(),
                        credential,
                    }
                }
            }
            PluginService::SessionRead | PluginService::WorkspaceRead => {
                pig_protocol::Op::QueryPlugin {
                    request_id: internal_id.clone(),
                    package: package.id.clone(),
                    digest: package.digest.clone(),
                    scope,
                    session_id: if service == PluginService::SessionRead {
                        request.session_id.clone()
                    } else {
                        None
                    },
                }
            }
            _ => {
                return failure(protocol_error(
                    PluginErrorCode::UnsupportedCapability,
                    "unsupported plugin service",
                ));
            }
        };
        let pending = PendingService {
            external_id: request_id.to_owned(),
            instance: instance.to_owned(),
            epoch,
            generation,
            service,
            network_bytes,
            deadline: Instant::now() + SERVICE_TIMEOUT,
        };
        self.pending
            .lock()
            .expect("plugin service pending lock")
            .insert(internal_id.clone(), pending);
        if let Err(send_error) = self.agent.try_send(op) {
            self.pending
                .lock()
                .expect("plugin service pending lock")
                .remove(&internal_id);
            return failure(
                protocol_error(PluginErrorCode::Transport, send_error.to_string()).retryable(),
            );
        }
        ServiceExecution::Forwarded
    }

    pub fn handle_core_event(&self, event: &Event) -> Vec<ServiceReply> {
        let (internal_id, result) = match event {
            Event::PluginServices {
                request_id,
                result: Ok(snapshot),
                ..
            } => {
                let pending = self.take(request_id);
                let Some(pending) = pending else {
                    return Vec::new();
                };
                let projected = match pending.service {
                    PluginService::SessionRead => serde_json::json!({
                        "generation": snapshot.generation,
                        "session": snapshot.session,
                    }),
                    PluginService::WorkspaceRead => serde_json::json!({
                        "generation": snapshot.generation,
                        "scope": snapshot.scope,
                        "workspaces": snapshot.workspaces,
                    }),
                    _ => serde_json::to_value(snapshot).unwrap_or_default(),
                };
                return vec![self.finish(pending, Ok(projected))];
            }
            Event::PluginServices {
                request_id,
                result: Err(core_error),
                ..
            } => (request_id, Err(PluginProtocolError::from_core(core_error))),
            Event::PluginActionResult { result } => (
                &result.request_id,
                result
                    .result
                    .clone()
                    .map_err(|error| PluginProtocolError::from_core(&error)),
            ),
            Event::PluginHttpResponse { request_id, result } => (
                request_id,
                result
                    .as_ref()
                    .map(|response| {
                        serde_json::to_value(response).unwrap_or(serde_json::Value::Null)
                    })
                    .map_err(PluginProtocolError::from_core),
            ),
            Event::PluginSecretResult { request_id, result } => (
                request_id,
                result
                    .as_ref()
                    .map(|_| serde_json::Value::Null)
                    .map_err(PluginProtocolError::from_core),
            ),
            _ => return Vec::new(),
        };
        let Some(pending) = self.take(internal_id) else {
            return Vec::new();
        };
        if pending.service == PluginService::HttpRequest {
            let response_bytes = result
                .as_ref()
                .ok()
                .and_then(|value| value.get("body"))
                .and_then(serde_json::Value::as_str)
                .map_or(0, |body| body.len() as u64);
            let bytes = pending.network_bytes.saturating_add(response_bytes);
            let mut usage = self.usage.lock().expect("plugin service usage lock");
            let usage = usage
                .entry((pending.instance.clone(), pending.epoch))
                .or_default();
            usage.network_bytes = usage.network_bytes.saturating_add(bytes);
        }
        vec![self.finish(pending, result)]
    }

    pub fn expire(&self) -> Vec<ServiceReply> {
        let now = Instant::now();
        let expired: Vec<String> = self
            .pending
            .lock()
            .expect("plugin service pending lock")
            .iter()
            .filter(|(_, pending)| pending.deadline <= now)
            .map(|(id, _)| id.clone())
            .collect();
        expired
            .into_iter()
            .filter_map(|id| {
                self.take(&id).map(|pending| {
                    self.usage
                        .lock()
                        .expect("plugin service usage lock")
                        .entry((pending.instance.clone(), pending.epoch))
                        .or_default()
                        .timed_out_requests += 1;
                    self.finish(
                        pending,
                        Err(protocol_error(
                            PluginErrorCode::Timeout,
                            "plugin service request timed out",
                        )
                        .retryable()),
                    )
                })
            })
            .collect()
    }

    pub fn cancel_instance(&self, instance: &str, epoch: Option<u64>) -> Vec<ServiceReply> {
        let ids: Vec<String> = self
            .pending
            .lock()
            .expect("plugin service pending lock")
            .iter()
            .filter(|(_, pending)| {
                pending.instance == instance && epoch.is_none_or(|epoch| pending.epoch == epoch)
            })
            .map(|(id, _)| id.clone())
            .collect();
        let replies: Vec<_> = ids
            .into_iter()
            .filter_map(|id| {
                self.take(&id).map(|pending| {
                    self.usage
                        .lock()
                        .expect("plugin service usage lock")
                        .entry((pending.instance.clone(), pending.epoch))
                        .or_default()
                        .cancelled_requests += 1;
                    self.finish(
                        pending,
                        Err(protocol_error(
                            PluginErrorCode::Cancelled,
                            "plugin service request was cancelled",
                        )),
                    )
                })
            })
            .collect();
        if let Some(epoch) = epoch {
            self.usage
                .lock()
                .expect("plugin service usage lock")
                .retain(|(candidate, current), _| candidate != instance || *current >= epoch);
        }
        replies
    }

    pub fn cancel_requests(&self, request_ids: &[String]) -> Vec<ServiceReply> {
        let ids: Vec<String> = self
            .pending
            .lock()
            .expect("plugin service pending lock")
            .iter()
            .filter(|(_, pending)| request_ids.contains(&pending.external_id))
            .map(|(id, _)| id.clone())
            .collect();
        ids.into_iter()
            .filter_map(|id| {
                self.take(&id).map(|pending| {
                    self.usage
                        .lock()
                        .expect("plugin service usage lock")
                        .entry((pending.instance.clone(), pending.epoch))
                        .or_default()
                        .cancelled_requests += 1;
                    self.finish(
                        pending,
                        Err(protocol_error(
                            PluginErrorCode::Cancelled,
                            "plugin service request was cancelled",
                        )),
                    )
                })
            })
            .collect()
    }

    pub fn pending_count(&self, instance: &str, epoch: u64) -> usize {
        self.pending
            .lock()
            .expect("plugin service pending lock")
            .values()
            .filter(|pending| pending.instance == instance && pending.epoch == epoch)
            .count()
    }

    pub fn record_tool_completion(
        &self,
        instance: &str,
        epoch: u64,
        timed_out: bool,
        cancelled: bool,
    ) {
        if !timed_out && !cancelled {
            return;
        }
        let mut usage = self.usage.lock().expect("plugin service usage lock");
        let usage = usage.entry((instance.to_owned(), epoch)).or_default();
        usage.timed_out_requests += u64::from(timed_out);
        usage.cancelled_requests += u64::from(cancelled);
    }

    pub fn usage(&self, instance: &str, epoch: u64) -> (u64, u64, u64) {
        self.usage
            .lock()
            .expect("plugin service usage lock")
            .get(&(instance.to_owned(), epoch))
            .map(|usage| {
                (
                    usage.network_bytes,
                    usage.timed_out_requests,
                    usage.cancelled_requests,
                )
            })
            .unwrap_or_default()
    }

    pub fn storage_bytes(&self, package: &PluginPackageInfo) -> u64 {
        std::fs::metadata(storage_path(
            &self.data_dir,
            &super::manager::plugin_data_key(package),
        ))
        .map_or(0, |metadata| metadata.len())
    }

    fn take(&self, internal_id: &str) -> Option<PendingService> {
        self.pending
            .lock()
            .expect("plugin service pending lock")
            .remove(internal_id)
    }

    fn finish(
        &self,
        pending: PendingService,
        result: Result<serde_json::Value, PluginProtocolError>,
    ) -> ServiceReply {
        self.reply(
            &pending.instance,
            pending.epoch,
            pending.generation,
            &pending.external_id,
            pending.service,
            result,
        )
    }

    fn reply(
        &self,
        instance: &str,
        epoch: u64,
        generation: u64,
        request_id: &str,
        service: PluginService,
        result: Result<serde_json::Value, PluginProtocolError>,
    ) -> ServiceReply {
        ServiceReply {
            instance: instance.to_owned(),
            epoch,
            generation,
            request_id: request_id.to_owned(),
            service,
            result,
        }
    }
}

fn authorize_request(
    package: &PluginPackageInfo,
    request: &PluginServiceRequest,
) -> Result<(), PluginProtocolError> {
    let target = match request.service {
        PluginService::ConfigRead
        | PluginService::ConfigWrite
        | PluginService::CredentialSet
        | PluginService::CredentialClear => request
            .payload
            .get("path")
            .or_else(|| request.payload.get("credential"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default(),
        PluginService::HttpRequest => request
            .payload
            .get("origin")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default(),
        PluginService::SessionRead => request.session_id.as_deref().unwrap_or_default(),
        PluginService::WorkspaceRead => request.workspace_id.as_deref().unwrap_or("*"),
        PluginService::NavigationOpen => request
            .payload
            .get("pageId")
            .or_else(|| request.payload.get("page_id"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default(),
        _ => "*",
    };
    let capability = match request.service {
        PluginService::ConfigRead => Some("config.read"),
        PluginService::ConfigWrite => Some("config.write"),
        PluginService::HttpRequest => Some("http"),
        PluginService::CredentialSet | PluginService::CredentialClear => Some("credentials.own"),
        PluginService::SessionRead => Some("session.read"),
        PluginService::WorkspaceRead => Some("workspace.read"),
        PluginService::NavigationOpen => Some("navigation.open"),
        PluginService::ComposerSubmit => Some("composer.submit"),
        _ => None,
    };
    if let Some(capability) = capability
        && !capability_granted(package, capability, target)
    {
        return Err(protocol_error(
            PluginErrorCode::PermissionDenied,
            format!("{capability} is not granted for `{target}`"),
        ));
    }
    if request.service == PluginService::WorkspaceRead
        && let PluginSource::Project { workspace } = &package.source
        && request
            .workspace_id
            .as_deref()
            .is_some_and(|id| !id.is_empty() && Path::new(id) != workspace)
    {
        return Err(protocol_error(
            PluginErrorCode::PermissionDenied,
            "a project plugin cannot read another workspace",
        ));
    }
    Ok(())
}

pub fn capability_granted(package: &PluginPackageInfo, capability: &str, target: &str) -> bool {
    package.capabilities.iter().any(|item| {
        item.name == capability
            && (item.targets.iter().any(|allowed| allowed == "*")
                || item.targets.iter().any(|allowed| allowed == target))
    })
}

fn read_storage(data_dir: &Path, data_key: &str) -> Result<serde_json::Value, PluginProtocolError> {
    match std::fs::read(storage_path(data_dir, data_key)) {
        Ok(bytes) if bytes.len() > MAX_STORAGE_BYTES => Err(protocol_error(
            PluginErrorCode::QuotaExceeded,
            "plugin storage exceeds 1 MiB",
        )),
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|error| protocol_error(PluginErrorCode::Internal, error.to_string())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(serde_json::Value::Null),
        Err(error) => Err(protocol_error(
            PluginErrorCode::Transport,
            error.to_string(),
        )),
    }
}

fn write_storage(
    data_dir: &Path,
    data_key: &str,
    value: &serde_json::Value,
) -> Result<serde_json::Value, PluginProtocolError> {
    let bytes = serde_json::to_vec(value)
        .map_err(|error| protocol_error(PluginErrorCode::InvalidOperation, error.to_string()))?;
    if bytes.len() > MAX_STORAGE_BYTES {
        return Err(protocol_error(
            PluginErrorCode::QuotaExceeded,
            "plugin storage exceeds 1 MiB",
        ));
    }
    let path = storage_path(data_dir, data_key);
    let parent = path
        .parent()
        .ok_or_else(|| protocol_error(PluginErrorCode::Internal, "plugin storage has no parent"))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| protocol_error(PluginErrorCode::Transport, error.to_string()))?;
    let temporary = path.with_extension("json.tmp");
    write_private(&temporary, &bytes)
        .map_err(|error| protocol_error(PluginErrorCode::Transport, error.to_string()))?;
    std::fs::rename(&temporary, &path)
        .map_err(|error| protocol_error(PluginErrorCode::Transport, error.to_string()))?;
    Ok(serde_json::Value::Null)
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    use std::io::Write as _;
    options.open(path)?.write_all(bytes)
}

pub fn storage_path(data_dir: &Path, data_key: &str) -> PathBuf {
    use sha2::{Digest as _, Sha256};
    let digest = Sha256::digest(data_key.as_bytes());
    data_dir
        .join("instances")
        .join(format!("{digest:x}"))
        .join("storage.json")
}

fn read_resource(
    package: &PluginPackageInfo,
    payload: &serde_json::Value,
) -> Result<serde_json::Value, PluginProtocolError> {
    let uri = payload
        .get("uri")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let prefix = format!("plugin://{}/{}/", package.id, package.digest);
    let relative = uri.strip_prefix(&prefix).ok_or_else(|| {
        protocol_error(
            PluginErrorCode::PermissionDenied,
            "the resource URI does not belong to this exact package",
        )
    })?;
    let relative = PathBuf::from(relative);
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(protocol_error(
            PluginErrorCode::InvalidOperation,
            "invalid package resource path",
        ));
    }
    if pig_core::plugins::package_digest(&package.root)
        .map_err(|error| PluginProtocolError::from_core(&error))?
        != package.content_digest
    {
        return Err(protocol_error(
            PluginErrorCode::PermissionDenied,
            "plugin resources changed after verification",
        ));
    }
    let root = package
        .root
        .canonicalize()
        .map_err(|error| protocol_error(PluginErrorCode::NotFound, error.to_string()))?;
    let path = root.join(relative);
    let metadata = std::fs::symlink_metadata(&path)
        .map_err(|error| protocol_error(PluginErrorCode::NotFound, error.to_string()))?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() as usize > MAX_RESOURCE_BYTES
    {
        return Err(protocol_error(
            PluginErrorCode::QuotaExceeded,
            "resource must be a regular file no larger than 4 MiB",
        ));
    }
    let path = path
        .canonicalize()
        .map_err(|error| protocol_error(PluginErrorCode::NotFound, error.to_string()))?;
    if !path.starts_with(&root) {
        return Err(protocol_error(
            PluginErrorCode::PermissionDenied,
            "resource escapes the package",
        ));
    }
    let bytes = std::fs::read(&path)
        .map_err(|error| protocol_error(PluginErrorCode::Transport, error.to_string()))?;
    let mime = match path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
    {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "json" => "application/json",
        "md" => "text/markdown",
        _ => "application/octet-stream",
    };
    serde_json::to_value(PluginResource {
        mime: mime.to_owned(),
        bytes,
    })
    .map_err(|error| protocol_error(PluginErrorCode::Internal, error.to_string()))
}

fn system_locale() -> String {
    std::env::var("LC_ALL")
        .or_else(|_| std::env::var("LC_MESSAGES"))
        .or_else(|_| std::env::var("LANG"))
        .map(|locale| normalize_locale(&locale))
        .unwrap_or_else(|_| "en".to_owned())
}

fn normalize_locale(locale: &str) -> String {
    let locale = locale
        .split(['.', '@'])
        .next()
        .unwrap_or("en")
        .replace('_', "-");
    if locale.eq_ignore_ascii_case("zh") {
        "zh-CN".to_owned()
    } else if locale.is_empty()
        || locale.eq_ignore_ascii_case("c")
        || locale.eq_ignore_ascii_case("posix")
    {
        "en".to_owned()
    } else {
        locale
    }
}

fn protocol_error(code: PluginErrorCode, message: impl Into<String>) -> PluginProtocolError {
    PluginProtocolError::new(code, message)
}

#[allow(dead_code)]
fn _core_error_is_not_exposed(_: CoreError) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_is_atomic_and_scoped_by_the_stable_data_key() {
        let root = std::env::temp_dir().join(format!(
            "pig-service-storage-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        write_storage(
            &root,
            "moon:user:default",
            &serde_json::json!({"answer": 42}),
        )
        .unwrap();
        assert_eq!(
            read_storage(&root, "moon:user:default").unwrap()["answer"],
            42
        );
        assert_eq!(
            read_storage(&root, "moon:project:other").unwrap(),
            serde_json::Value::Null
        );
        assert!(
            !storage_path(&root, "moon:user:default")
                .to_string_lossy()
                .contains("moon:user")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn locale_fallback_is_stable() {
        assert_eq!(normalize_locale("zh_CN.UTF-8"), "zh-CN");
        assert_eq!(normalize_locale("C"), "en");
        assert_eq!(normalize_locale("en_US"), "en-US");
    }
}
