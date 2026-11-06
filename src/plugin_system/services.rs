use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pig_protocol::{
    CoreError, Event, MessageDelivery, PermissionSettings, PluginAction, PluginErrorCode,
    PluginGitReadRequest, PluginPackageInfo, PluginProtocolError, PluginResource, PluginScope,
    PluginService, PluginServiceRequest, PluginSessionAction, PluginSource,
    PluginWorkspaceFileReadRequest, SecretString,
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
    /// `(request log path, key)` when this request started a side effect; its
    /// result is persisted here so a retry (including after a restart) replays
    /// instead of executing again.
    request_key: Option<(PathBuf, String)>,
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
        if let Err(error) = authorize_service(package, &request) {
            return ServiceExecution::Complete(self.reply(
                instance,
                epoch,
                generation,
                request_id,
                request.service,
                Err(error),
            ));
        }
        let mut forwarded_key = None;
        if is_forwarded_side_effect(request.service) {
            let key = side_effect_key(request.service, &request, request_id);
            let path = request_log_path(&storage_path(
                &self.data_dir,
                &super::manager::plugin_data_key(package),
            ));
            match begin_request(&path, &key) {
                Ok(BeginRequest::Done(result)) => {
                    return ServiceExecution::Complete(self.reply(
                        instance,
                        epoch,
                        generation,
                        request_id,
                        request.service,
                        result,
                    ));
                }
                Ok(BeginRequest::InFlight) => {
                    return ServiceExecution::Complete(self.reply(
                        instance,
                        epoch,
                        generation,
                        request_id,
                        request.service,
                        Err(protocol_error(
                            PluginErrorCode::Busy,
                            "this side-effecting request is still in flight; its result is unknown",
                        )
                        .retryable()),
                    ));
                }
                Ok(BeginRequest::Start) => forwarded_key = Some((path, key)),
                Err(error) => {
                    return ServiceExecution::Complete(self.reply(
                        instance,
                        epoch,
                        generation,
                        request_id,
                        request.service,
                        Err(error),
                    ));
                }
            }
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
            PluginService::StorageWrite => {
                let data_key = super::manager::plugin_data_key(package);
                let result = self.run_local_side_effect(
                    package,
                    request.service,
                    request_id,
                    &request,
                    || write_storage(&self.data_dir, &data_key, &request.payload),
                );
                ServiceExecution::Complete(self.reply(
                    instance,
                    epoch,
                    generation,
                    request_id,
                    request.service,
                    result,
                ))
            }
            PluginService::StorageList => ServiceExecution::Complete(self.reply(
                instance,
                epoch,
                generation,
                request_id,
                request.service,
                storage_list(&self.data_dir, &super::manager::plugin_data_key(package)),
            )),
            PluginService::StorageKeyRead => ServiceExecution::Complete(self.reply(
                instance,
                epoch,
                generation,
                request_id,
                request.service,
                storage_key_read(
                    &self.data_dir,
                    &super::manager::plugin_data_key(package),
                    &request.payload,
                ),
            )),
            PluginService::StorageKeyWrite | PluginService::StorageKeyDelete => {
                let data_key = super::manager::plugin_data_key(package);
                let result = self.run_local_side_effect(
                    package,
                    request.service,
                    request_id,
                    &request,
                    || {
                        if request.service == PluginService::StorageKeyWrite {
                            storage_key_write(&self.data_dir, &data_key, &request.payload)
                        } else {
                            storage_key_delete(&self.data_dir, &data_key, &request.payload)
                        }
                    },
                );
                ServiceExecution::Complete(self.reply(
                    instance,
                    epoch,
                    generation,
                    request_id,
                    request.service,
                    result,
                ))
            }
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
                if !pig_protocol::PLUGIN_HOST_NAVIGATION_TARGETS.contains(&page_id) {
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
                    }),
                )
            }
            _ => self.forward(
                package,
                instance,
                epoch,
                generation,
                request_id,
                request,
                forwarded_key,
            ),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn forward(
        &self,
        package: &PluginPackageInfo,
        instance: &str,
        epoch: u64,
        generation: u64,
        request_id: &str,
        request: PluginServiceRequest,
        request_key: Option<(PathBuf, String)>,
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
            PluginService::SessionCreate => {
                let Some(cwd) = payload_str(&request.payload, &["cwd"]) else {
                    return failure(protocol_error(
                        PluginErrorCode::InvalidOperation,
                        "session.create needs a cwd",
                    ));
                };
                let permissions = match request.payload.get("permissions") {
                    None | Some(serde_json::Value::Null) => None,
                    Some(value) => {
                        match serde_json::from_value::<PermissionSettings>(value.clone()) {
                            Ok(permissions) => Some(permissions),
                            Err(error) => {
                                return failure(protocol_error(
                                    PluginErrorCode::InvalidOperation,
                                    format!("invalid session permissions: {error}"),
                                ));
                            }
                        }
                    }
                };
                pig_protocol::Op::PluginSessionAction {
                    request_id: internal_id.clone(),
                    package: package.id.clone(),
                    digest: package.digest.clone(),
                    action: PluginSessionAction::Create {
                        cwd: PathBuf::from(cwd),
                        provider_id: payload_str(&request.payload, &["providerId"])
                            .map(str::to_owned),
                        model_id: payload_str(&request.payload, &["modelId"]).map(str::to_owned),
                        reasoning_level: payload_str(&request.payload, &["reasoningLevel"])
                            .map(str::to_owned),
                        permissions,
                    },
                }
            }
            PluginService::SessionSend => {
                let Some(session_id) = session_target(&request) else {
                    return failure(protocol_error(
                        PluginErrorCode::InvalidOperation,
                        "session.send needs a sessionId",
                    ));
                };
                let Some(content) = payload_str(&request.payload, &["content"]) else {
                    return failure(protocol_error(
                        PluginErrorCode::InvalidOperation,
                        "session.send needs content",
                    ));
                };
                let delivery = match request.payload.get("delivery") {
                    None | Some(serde_json::Value::Null) => MessageDelivery::default(),
                    Some(value) => match serde_json::from_value::<MessageDelivery>(value.clone()) {
                        Ok(delivery) => delivery,
                        Err(error) => {
                            return failure(protocol_error(
                                PluginErrorCode::InvalidOperation,
                                format!("invalid delivery: {error}"),
                            ));
                        }
                    },
                };
                pig_protocol::Op::PluginSessionAction {
                    request_id: internal_id.clone(),
                    package: package.id.clone(),
                    digest: package.digest.clone(),
                    action: PluginSessionAction::Send {
                        session_id: session_id.to_owned(),
                        content: content.to_owned(),
                        delivery,
                    },
                }
            }
            PluginService::SessionInterrupt => {
                let Some(session_id) = session_target(&request) else {
                    return failure(protocol_error(
                        PluginErrorCode::InvalidOperation,
                        "session.interrupt needs a sessionId",
                    ));
                };
                pig_protocol::Op::PluginSessionAction {
                    request_id: internal_id.clone(),
                    package: package.id.clone(),
                    digest: package.digest.clone(),
                    action: PluginSessionAction::Interrupt {
                        session_id: session_id.to_owned(),
                    },
                }
            }
            PluginService::SessionWait => {
                let Some(session_id) = session_target(&request) else {
                    return failure(protocol_error(
                        PluginErrorCode::InvalidOperation,
                        "session.wait needs a sessionId",
                    ));
                };
                let timeout_ms = request
                    .payload
                    .get("timeoutMs")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(30_000);
                pig_protocol::Op::PluginSessionAction {
                    request_id: internal_id.clone(),
                    package: package.id.clone(),
                    digest: package.digest.clone(),
                    action: PluginSessionAction::Wait {
                        session_id: session_id.to_owned(),
                        timeout_ms,
                    },
                }
            }
            PluginService::ModelRequest => {
                let Some(purpose) = payload_str(&request.payload, &["purpose"]) else {
                    return failure(protocol_error(
                        PluginErrorCode::InvalidOperation,
                        "model.request needs a purpose",
                    ));
                };
                let Some(prompt) = payload_str(&request.payload, &["prompt"]) else {
                    return failure(protocol_error(
                        PluginErrorCode::InvalidOperation,
                        "model.request needs a prompt",
                    ));
                };
                pig_protocol::Op::PluginSessionAction {
                    request_id: internal_id.clone(),
                    package: package.id.clone(),
                    digest: package.digest.clone(),
                    action: PluginSessionAction::Model {
                        session_id: session_target(&request).map(str::to_owned),
                        purpose: purpose.to_owned(),
                        system: payload_str(&request.payload, &["system"]).map(str::to_owned),
                        prompt: prompt.to_owned(),
                        max_output_tokens: request
                            .payload
                            .get("maxOutputTokens")
                            .and_then(serde_json::Value::as_u64),
                    },
                }
            }
            PluginService::AgentList => pig_protocol::Op::PluginSessionAction {
                request_id: internal_id.clone(),
                package: package.id.clone(),
                digest: package.digest.clone(),
                action: PluginSessionAction::AgentList {
                    session_id: session_target(&request).map(str::to_owned),
                },
            },
            PluginService::AgentStart => {
                let (Some(session_id), Some(profile), Some(prompt)) = (
                    session_target(&request),
                    payload_str(&request.payload, &["profile"]),
                    payload_str(&request.payload, &["prompt"]),
                ) else {
                    return failure(protocol_error(
                        PluginErrorCode::InvalidOperation,
                        "agent.start needs a sessionId, profile and prompt",
                    ));
                };
                pig_protocol::Op::PluginSessionAction {
                    request_id: internal_id.clone(),
                    package: package.id.clone(),
                    digest: package.digest.clone(),
                    action: PluginSessionAction::AgentStart {
                        session_id: session_id.to_owned(),
                        profile: profile.to_owned(),
                        prompt: prompt.to_owned(),
                        description: payload_str(&request.payload, &["description"])
                            .unwrap_or(prompt)
                            .to_owned(),
                        background: request
                            .payload
                            .get("background")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(true),
                    },
                }
            }
            PluginService::AgentSend => {
                let (Some(session_id), Some(agent_id), Some(message)) = (
                    session_target(&request),
                    payload_str(&request.payload, &["agentId"]),
                    payload_str(&request.payload, &["message"]),
                ) else {
                    return failure(protocol_error(
                        PluginErrorCode::InvalidOperation,
                        "agent.send needs a sessionId, agentId and message",
                    ));
                };
                pig_protocol::Op::PluginSessionAction {
                    request_id: internal_id.clone(),
                    package: package.id.clone(),
                    digest: package.digest.clone(),
                    action: PluginSessionAction::AgentSend {
                        session_id: session_id.to_owned(),
                        agent_id: agent_id.to_owned(),
                        message: message.to_owned(),
                    },
                }
            }
            PluginService::AgentWait => {
                let (Some(session_id), Some(agent_id)) = (
                    session_target(&request),
                    payload_str(&request.payload, &["agentId"]),
                ) else {
                    return failure(protocol_error(
                        PluginErrorCode::InvalidOperation,
                        "agent.wait needs a sessionId and agentId",
                    ));
                };
                let timeout_ms = request
                    .payload
                    .get("timeoutMs")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(30_000);
                pig_protocol::Op::PluginSessionAction {
                    request_id: internal_id.clone(),
                    package: package.id.clone(),
                    digest: package.digest.clone(),
                    action: PluginSessionAction::AgentWait {
                        session_id: session_id.to_owned(),
                        agent_id: agent_id.to_owned(),
                        timeout_ms,
                    },
                }
            }
            PluginService::AgentStop => {
                let (Some(session_id), Some(agent_id)) = (
                    session_target(&request),
                    payload_str(&request.payload, &["agentId"]),
                ) else {
                    return failure(protocol_error(
                        PluginErrorCode::InvalidOperation,
                        "agent.stop needs a sessionId and agentId",
                    ));
                };
                pig_protocol::Op::PluginSessionAction {
                    request_id: internal_id.clone(),
                    package: package.id.clone(),
                    digest: package.digest.clone(),
                    action: PluginSessionAction::AgentStop {
                        session_id: session_id.to_owned(),
                        agent_id: agent_id.to_owned(),
                    },
                }
            }
            PluginService::AgentRead => {
                let (Some(session_id), Some(agent_id)) = (
                    session_target(&request),
                    payload_str(&request.payload, &["agentId"]),
                ) else {
                    return failure(protocol_error(
                        PluginErrorCode::InvalidOperation,
                        "agent.read needs a sessionId and agentId",
                    ));
                };
                pig_protocol::Op::PluginSessionAction {
                    request_id: internal_id.clone(),
                    package: package.id.clone(),
                    digest: package.digest.clone(),
                    action: PluginSessionAction::AgentRead {
                        session_id: session_id.to_owned(),
                        agent_id: agent_id.to_owned(),
                    },
                }
            }
            PluginService::GitStatus
            | PluginService::GitDiff
            | PluginService::GitLog
            | PluginService::GitShow
            | PluginService::GitBranches => {
                let Some(session_id) = session_target(&request) else {
                    return failure(protocol_error(
                        PluginErrorCode::InvalidOperation,
                        "Git reads need a sessionId",
                    ));
                };
                let parsed =
                    serde_json::from_value::<PluginGitReadRequest>(request.payload.clone());
                match parsed {
                    Ok(request) => pig_protocol::Op::PluginGitRead {
                        request_id: internal_id.clone(),
                        package: package.id.clone(),
                        digest: package.digest.clone(),
                        session_id: session_id.to_owned(),
                        request,
                    },
                    Err(error) => {
                        return failure(protocol_error(
                            PluginErrorCode::InvalidOperation,
                            error.to_string(),
                        ));
                    }
                }
            }
            PluginService::WorkspaceFileRead => {
                let Some(session_id) = session_target(&request) else {
                    return failure(protocol_error(
                        PluginErrorCode::InvalidOperation,
                        "workspace.file.read needs a sessionId",
                    ));
                };
                match serde_json::from_value::<PluginWorkspaceFileReadRequest>(
                    request.payload.clone(),
                ) {
                    Ok(request) => pig_protocol::Op::PluginWorkspaceFileRead {
                        request_id: internal_id.clone(),
                        package: package.id.clone(),
                        digest: package.digest.clone(),
                        session_id: session_id.to_owned(),
                        request,
                    },
                    Err(error) => {
                        return failure(protocol_error(
                            PluginErrorCode::InvalidOperation,
                            error.to_string(),
                        ));
                    }
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
            request_key,
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
            Event::PluginServiceValue {
                request_id, result, ..
            } => (
                request_id,
                result
                    .as_ref()
                    .map(Clone::clone)
                    .map_err(PluginProtocolError::from_core),
            ),
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
            Event::PluginSessionResult {
                request_id, result, ..
            } => (
                request_id,
                result
                    .as_ref()
                    .map(Clone::clone)
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
        let storage = storage_path(&self.data_dir, &super::manager::plugin_data_key(package));
        [
            storage.clone(),
            kv_path(&storage),
            request_log_path(&storage),
        ]
        .iter()
        .filter_map(|path| std::fs::metadata(path).ok())
        .map(|metadata| metadata.len())
        .sum()
    }

    fn run_local_side_effect(
        &self,
        package: &PluginPackageInfo,
        service: PluginService,
        request_id: &str,
        request: &PluginServiceRequest,
        run: impl FnOnce() -> Result<serde_json::Value, PluginProtocolError>,
    ) -> Result<serde_json::Value, PluginProtocolError> {
        let key = side_effect_key(service, request, request_id);
        let path = request_log_path(&storage_path(
            &self.data_dir,
            &super::manager::plugin_data_key(package),
        ));
        match begin_request(&path, &key)? {
            BeginRequest::Done(result) => return result,
            BeginRequest::InFlight => {
                return Err(protocol_error(
                    PluginErrorCode::Busy,
                    "this side-effecting request is still in flight; its result is unknown",
                )
                .retryable());
            }
            BeginRequest::Start => {}
        }
        let result = run();
        finish_request(&path, &key, &result);
        result
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
        if let Some((path, key)) = &pending.request_key {
            finish_request(path, key, &result);
        }
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

/// Side effects whose result is persisted for replay. Storage writes, config
/// writes, credential changes and HTTP requests are the observable mutations a
/// plugin can request; a read is never logged.
fn is_forwarded_side_effect(service: PluginService) -> bool {
    matches!(
        service,
        PluginService::ConfigWrite
            | PluginService::CredentialSet
            | PluginService::CredentialClear
            | PluginService::HttpRequest
    )
}

fn side_effect_key(
    service: PluginService,
    request: &PluginServiceRequest,
    request_id: &str,
) -> String {
    let supplied = request
        .idempotency_key
        .as_deref()
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .unwrap_or(request_id);
    let supplied: String = supplied.chars().take(256).collect();
    format!("{}:{supplied}", service.as_str())
}

fn kv_path(storage: &Path) -> PathBuf {
    storage.with_file_name("kv.json")
}

fn request_log_path(storage: &Path) -> PathBuf {
    storage.with_file_name("requests.json")
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct KeyedStore {
    #[serde(default)]
    entries: BTreeMap<String, KeyedEntry>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct KeyedEntry {
    value: serde_json::Value,
    revision: u64,
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct RequestLog {
    #[serde(default)]
    entries: BTreeMap<String, RequestRecord>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct RequestRecord {
    at: u64,
    #[serde(default)]
    in_flight: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    result: Option<Result<serde_json::Value, PluginProtocolError>>,
}

enum BeginRequest {
    Done(Result<serde_json::Value, PluginProtocolError>),
    InFlight,
    Start,
}

const MAX_KEY_CHARS: usize = 128;
const MAX_KEYED_ENTRIES: usize = 512;
const MAX_REQUEST_RECORDS: usize = 256;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn load_json_file<T: serde::de::DeserializeOwned + Default>(
    path: &Path,
    what: &str,
) -> Result<T, PluginProtocolError> {
    match std::fs::read(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(protocol_error(
            PluginErrorCode::Transport,
            format!("{what}: {error}"),
        )),
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
            protocol_error(
                PluginErrorCode::Internal,
                format!("{what} is corrupt; refusing to guess: {error}"),
            )
        }),
    }
}

fn save_json_file<T: serde::Serialize>(
    path: &Path,
    value: &T,
    cap: usize,
) -> Result<(), PluginProtocolError> {
    let bytes = serde_json::to_vec(value)
        .map_err(|error| protocol_error(PluginErrorCode::Internal, error.to_string()))?;
    if bytes.len() > cap {
        return Err(protocol_error(
            PluginErrorCode::QuotaExceeded,
            "the plugin's private storage exceeds 1 MiB",
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| protocol_error(PluginErrorCode::Internal, "storage has no parent"))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| protocol_error(PluginErrorCode::Transport, error.to_string()))?;
    let temporary = path.with_extension("json.tmp");
    pig_core::config::write_private_file(&temporary, &bytes)
        .map_err(|error| protocol_error(PluginErrorCode::Transport, format!("{error:?}")))?;
    std::fs::rename(&temporary, path)
        .map_err(|error| protocol_error(PluginErrorCode::Transport, error.to_string()))?;
    Ok(())
}

fn begin_request(path: &Path, key: &str) -> Result<BeginRequest, PluginProtocolError> {
    let mut log: RequestLog = load_json_file(path, "the plugin request log")?;
    if let Some(record) = log.entries.get(key) {
        if record.in_flight {
            return Ok(BeginRequest::InFlight);
        }
        if let Some(result) = &record.result {
            return Ok(BeginRequest::Done(result.clone()));
        }
    }
    log.entries.insert(
        key.to_owned(),
        RequestRecord {
            at: now_secs(),
            in_flight: true,
            result: None,
        },
    );
    prune_request_log(&mut log);
    save_json_file(path, &log, MAX_STORAGE_BYTES)?;
    Ok(BeginRequest::Start)
}

fn finish_request(path: &Path, key: &str, result: &Result<serde_json::Value, PluginProtocolError>) {
    let Ok(mut log) = load_json_file::<RequestLog>(path, "the plugin request log") else {
        return;
    };
    log.entries.insert(
        key.to_owned(),
        RequestRecord {
            at: now_secs(),
            in_flight: false,
            result: Some(result.clone()),
        },
    );
    prune_request_log(&mut log);
    if let Err(error) = save_json_file(path, &log, MAX_STORAGE_BYTES) {
        tracing::warn!(
            detail = %error.message,
            "could not persist a plugin side-effect result; a retry will refuse to repeat it"
        );
    }
}

fn prune_request_log(log: &mut RequestLog) {
    while log.entries.len() > MAX_REQUEST_RECORDS {
        // Oldest record first; ties keep a deterministic order.
        let Some(oldest) = log
            .entries
            .iter()
            .min_by_key(|(key, record)| (record.at, key.as_str()))
            .map(|(key, _)| key.clone())
        else {
            break;
        };
        log.entries.remove(&oldest);
    }
}

fn valid_key(key: &str) -> Result<(), PluginProtocolError> {
    if key.is_empty() || key.chars().count() > MAX_KEY_CHARS || key.chars().any(char::is_control) {
        return Err(protocol_error(
            PluginErrorCode::InvalidOperation,
            format!("storage keys must be 1..{MAX_KEY_CHARS} printable characters"),
        ));
    }
    Ok(())
}

fn payload_key(payload: &serde_json::Value) -> Result<&str, PluginProtocolError> {
    let key = payload
        .get("key")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            protocol_error(
                PluginErrorCode::InvalidOperation,
                "a storage key is required",
            )
        })?;
    valid_key(key)?;
    Ok(key)
}

fn expected_revision(payload: &serde_json::Value) -> Option<u64> {
    payload
        .get("expectedRevision")
        .or_else(|| payload.get("expected_revision"))
        .and_then(serde_json::Value::as_u64)
}

fn stale_revision(current: u64) -> PluginProtocolError {
    protocol_error(
        PluginErrorCode::StaleRevision,
        format!("the key is at revision {current}"),
    )
    .with_details(serde_json::json!({ "currentRevision": current }))
}

fn load_keyed_store(path: &Path) -> Result<KeyedStore, PluginProtocolError> {
    load_json_file(path, "the plugin storage")
}

fn storage_list(data_dir: &Path, data_key: &str) -> Result<serde_json::Value, PluginProtocolError> {
    let store = load_keyed_store(&kv_path(&storage_path(data_dir, data_key)))?;
    let keys: Vec<serde_json::Value> = store
        .entries
        .iter()
        .map(|(key, entry)| {
            serde_json::json!({
                "key": key,
                "revision": entry.revision,
                "bytes": serde_json::to_vec(&entry.value).map_or(0, |bytes| bytes.len()),
            })
        })
        .collect();
    Ok(serde_json::json!({ "keys": keys }))
}

fn storage_key_read(
    data_dir: &Path,
    data_key: &str,
    payload: &serde_json::Value,
) -> Result<serde_json::Value, PluginProtocolError> {
    let key = payload_key(payload)?;
    let store = load_keyed_store(&kv_path(&storage_path(data_dir, data_key)))?;
    match store.entries.get(key) {
        Some(entry) => Ok(serde_json::json!({
            "value": entry.value,
            "revision": entry.revision,
        })),
        None => Ok(serde_json::json!({ "value": null, "revision": 0 })),
    }
}

fn storage_key_write(
    data_dir: &Path,
    data_key: &str,
    payload: &serde_json::Value,
) -> Result<serde_json::Value, PluginProtocolError> {
    let key = payload_key(payload)?.to_owned();
    let value = payload
        .get("value")
        .cloned()
        .ok_or_else(|| protocol_error(PluginErrorCode::InvalidOperation, "a value is required"))?;
    let path = kv_path(&storage_path(data_dir, data_key));
    let mut store = load_keyed_store(&path)?;
    let current = store
        .entries
        .get(&key)
        .map(|entry| entry.revision)
        .unwrap_or(0);
    if let Some(expected) = expected_revision(payload)
        && expected != current
    {
        return Err(stale_revision(current));
    }
    if !store.entries.contains_key(&key) && store.entries.len() >= MAX_KEYED_ENTRIES {
        return Err(protocol_error(
            PluginErrorCode::QuotaExceeded,
            "the plugin storage holds too many keys",
        ));
    }
    let revision = current.saturating_add(1);
    store.entries.insert(key, KeyedEntry { value, revision });
    save_json_file(&path, &store, MAX_STORAGE_BYTES)?;
    Ok(serde_json::json!({ "revision": revision }))
}

fn storage_key_delete(
    data_dir: &Path,
    data_key: &str,
    payload: &serde_json::Value,
) -> Result<serde_json::Value, PluginProtocolError> {
    let key = payload_key(payload)?;
    let path = kv_path(&storage_path(data_dir, data_key));
    let mut store = load_keyed_store(&path)?;
    let Some(entry) = store.entries.get(key) else {
        if let Some(expected) = expected_revision(payload)
            && expected != 0
        {
            return Err(stale_revision(0));
        }
        return Ok(serde_json::json!({ "deleted": false }));
    };
    if let Some(expected) = expected_revision(payload)
        && expected != entry.revision
    {
        return Err(stale_revision(entry.revision));
    }
    store.entries.remove(key);
    save_json_file(&path, &store, MAX_STORAGE_BYTES)?;
    Ok(serde_json::json!({ "deleted": true }))
}

pub(super) fn authorize_service(
    package: &PluginPackageInfo,
    request: &PluginServiceRequest,
) -> Result<(), PluginProtocolError> {
    let target: String = match request.service {
        PluginService::ConfigRead
        | PluginService::ConfigWrite
        | PluginService::CredentialSet
        | PluginService::CredentialClear => payload_str(&request.payload, &["path", "credential"])
            .unwrap_or_default()
            .to_owned(),
        PluginService::HttpRequest => payload_str(&request.payload, &["origin"])
            .unwrap_or_default()
            .to_owned(),
        PluginService::SessionRead => request.session_id.clone().unwrap_or_default(),
        PluginService::WorkspaceRead => request
            .workspace_id
            .clone()
            .unwrap_or_else(|| "*".to_owned()),
        PluginService::NavigationOpen => payload_str(&request.payload, &["pageId", "page_id"])
            .unwrap_or_default()
            .to_owned(),
        PluginService::SessionSubscribe
        | PluginService::SessionUnsubscribe
        | PluginService::SessionSend
        | PluginService::SessionInterrupt
        | PluginService::SessionWait
        | PluginService::AgentStart
        | PluginService::AgentSend
        | PluginService::AgentWait
        | PluginService::AgentStop
        | PluginService::AgentRead
        | PluginService::GitStatus
        | PluginService::GitDiff
        | PluginService::GitLog
        | PluginService::GitShow
        | PluginService::GitBranches
        | PluginService::WorkspaceFileRead => {
            session_target(request).unwrap_or_default().to_owned()
        }
        PluginService::AgentList => "*".to_owned(),
        PluginService::SessionCreate => payload_str(&request.payload, &["cwd", "workspaceId"])
            .map(str::to_owned)
            .or_else(|| request.workspace_id.clone())
            .unwrap_or_else(|| "*".to_owned()),
        PluginService::ModelRequest => payload_str(&request.payload, &["purpose"])
            .unwrap_or_default()
            .to_owned(),
        _ => "*".to_owned(),
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
        PluginService::SessionSubscribe | PluginService::SessionUnsubscribe => {
            Some("session.subscribe")
        }
        PluginService::SessionCreate => Some("session.create"),
        PluginService::SessionSend
        | PluginService::SessionInterrupt
        | PluginService::SessionWait => Some("session.control"),
        PluginService::ModelRequest => Some("model.request"),
        PluginService::AgentList
        | PluginService::AgentStart
        | PluginService::AgentSend
        | PluginService::AgentWait
        | PluginService::AgentStop
        | PluginService::AgentRead => Some("agent.delegate"),
        PluginService::GitStatus
        | PluginService::GitDiff
        | PluginService::GitLog
        | PluginService::GitShow
        | PluginService::GitBranches => Some("git.read"),
        PluginService::WorkspaceFileRead => Some("files.read"),
        _ => None,
    };
    if let Some(capability) = capability
        && !capability_granted(package, capability, &target)
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

pub(super) fn payload_str<'a>(payload: &'a serde_json::Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| payload.get(*key).and_then(serde_json::Value::as_str))
}

fn session_target(request: &PluginServiceRequest) -> Option<&str> {
    request
        .session_id
        .as_deref()
        .or_else(|| payload_str(&request.payload, &["sessionId", "session_id"]))
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
    fn keyed_storage_uses_revision_cas() {
        let root = std::env::temp_dir().join(format!(
            "pig-service-kv-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let data_key = "moon:user:default";
        let written = storage_key_write(
            &root,
            data_key,
            &serde_json::json!({"key": "alpha", "value": {"n": 1}}),
        )
        .expect("first write");
        assert_eq!(written["revision"], 1);
        assert_eq!(
            storage_key_read(&root, data_key, &serde_json::json!({"key": "alpha"})).unwrap()["value"]
                ["n"],
            1
        );
        let stale = storage_key_write(
            &root,
            data_key,
            &serde_json::json!({"key": "alpha", "value": {"n": 2}, "expectedRevision": 0}),
        )
        .expect_err("stale revision");
        assert_eq!(stale.code, PluginErrorCode::StaleRevision);
        assert_eq!(
            stale.details.as_ref().unwrap()["currentRevision"],
            1,
            "the conflict names the current revision"
        );
        let updated = storage_key_write(
            &root,
            data_key,
            &serde_json::json!({"key": "alpha", "value": {"n": 2}, "expectedRevision": 1}),
        )
        .expect("compare-and-swap");
        assert_eq!(updated["revision"], 2);
        let listed = storage_list(&root, data_key).unwrap();
        assert_eq!(listed["keys"][0]["key"], "alpha");
        assert_eq!(listed["keys"][0]["revision"], 2);
        let deleted = storage_key_delete(
            &root,
            data_key,
            &serde_json::json!({"key": "alpha", "expectedRevision": 2}),
        )
        .expect("delete");
        assert_eq!(deleted["deleted"], true);
        assert_eq!(
            storage_key_read(&root, data_key, &serde_json::json!({"key": "alpha"})).unwrap(),
            serde_json::json!({"value": null, "revision": 0})
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn an_idempotent_side_effect_is_replayed_instead_of_repeated() {
        let root = std::env::temp_dir().join(format!(
            "pig-service-idempotent-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let (agent, _agent_rx) = async_channel::unbounded();
        let gateway = HostServiceGateway::new(root.clone(), agent);
        let package = PluginPackageInfo {
            id: "moon".into(),
            name: "Moon".into(),
            version: "1".into(),
            digest: "sha256:test".into(),
            content_digest: "sha256:content".into(),
            root: PathBuf::new(),
            source: PluginSource::User,
            enabled: true,
            incompatible: None,
            backend: None,
            contributions: Default::default(),
            capabilities: Vec::new(),
        };
        let request = |_request_id: &str, key: Option<&str>| PluginServiceRequest {
            service: PluginService::StorageWrite,
            payload: serde_json::json!({"answer": 42}),
            session_id: None,
            workspace_id: None,
            idempotency_key: key.map(str::to_owned),
        };
        let first = gateway.execute(
            &package,
            "instance",
            1,
            1,
            "request-1",
            request("request-1", Some("write-1")),
        );
        let ServiceExecution::Complete(reply) = first else {
            panic!("the storage write completes locally");
        };
        reply.result.expect("first write");
        let storage = storage_path(&root, &super::super::manager::plugin_data_key(&package));
        assert!(storage.is_file());
        // A retry with the same key replays the recorded result even though the
        // file is gone: the effect is not executed a second time.
        std::fs::remove_file(&storage).expect("remove the storage file");
        let retried = gateway.execute(
            &package,
            "instance",
            1,
            1,
            "request-2",
            request("request-2", Some("write-1")),
        );
        let ServiceExecution::Complete(reply) = retried else {
            panic!("the retry completes locally");
        };
        reply.result.expect("replayed result");
        assert!(
            !storage.is_file(),
            "a replayed side effect must not run again"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn git_and_file_reads_require_their_own_capabilities() {
        let mut package = PluginPackageInfo {
            id: "moon".into(),
            name: "Moon".into(),
            version: "1".into(),
            digest: "sha256:test".into(),
            content_digest: "sha256:content".into(),
            root: PathBuf::new(),
            source: PluginSource::User,
            enabled: true,
            incompatible: None,
            backend: None,
            contributions: Default::default(),
            capabilities: vec![pig_protocol::PluginCapabilityRequest {
                name: "git.read".into(),
                targets: vec!["*".into()],
            }],
        };
        let git = PluginServiceRequest {
            service: PluginService::GitStatus,
            payload: serde_json::json!({"op": "status"}),
            session_id: Some("s1".into()),
            workspace_id: None,
            idempotency_key: None,
        };
        authorize_service(&package, &git).expect("git.read grants Git status");
        let file = PluginServiceRequest {
            service: PluginService::WorkspaceFileRead,
            payload: serde_json::json!({"path": "src/lib.rs"}),
            session_id: Some("s1".into()),
            workspace_id: None,
            idempotency_key: None,
        };
        assert!(authorize_service(&package, &file).is_err());
        package
            .capabilities
            .push(pig_protocol::PluginCapabilityRequest {
                name: "files.read".into(),
                targets: vec!["*".into()],
            });
        authorize_service(&package, &file).expect("files.read grants file reads");
    }

    #[tokio::test]
    async fn workspace_read_services_forward_and_settle_typed_ops() {
        let (agent, agent_rx) = async_channel::unbounded();
        let gateway = HostServiceGateway::new(PathBuf::new(), agent);
        let package = PluginPackageInfo {
            id: "moon".into(),
            name: "Moon".into(),
            version: "1".into(),
            digest: "sha256:test".into(),
            content_digest: "sha256:content".into(),
            root: PathBuf::new(),
            source: PluginSource::User,
            enabled: true,
            incompatible: None,
            backend: None,
            contributions: Default::default(),
            capabilities: vec![
                pig_protocol::PluginCapabilityRequest {
                    name: "git.read".into(),
                    targets: vec!["*".into()],
                },
                pig_protocol::PluginCapabilityRequest {
                    name: "files.read".into(),
                    targets: vec!["*".into()],
                },
            ],
        };
        let execute = |service, payload, request_id: &str| {
            gateway.execute(
                &package,
                "moon:user:default",
                1,
                1,
                request_id,
                PluginServiceRequest {
                    service,
                    payload,
                    session_id: Some("s1".into()),
                    workspace_id: None,
                    idempotency_key: None,
                },
            )
        };
        assert!(matches!(
            execute(
                PluginService::GitDiff,
                serde_json::json!({"op": "diff", "path": "src/lib.rs", "staged": true}),
                "git-1"
            ),
            ServiceExecution::Forwarded
        ));
        let pig_protocol::Op::PluginGitRead {
            request_id,
            session_id,
            request,
            ..
        } = agent_rx.recv().await.expect("git op")
        else {
            panic!("expected a Git read op");
        };
        assert_eq!(session_id, "s1");
        assert_eq!(
            request,
            pig_protocol::PluginGitReadRequest::Diff {
                path: "src/lib.rs".into(),
                staged: true
            }
        );
        let replies = gateway.handle_core_event(&Event::PluginServiceValue {
            request_id,
            package: "moon".into(),
            digest: "sha256:test".into(),
            result: Ok(serde_json::json!({"diff": "+two", "note": null})),
        });
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].request_id, "git-1");
        assert_eq!(replies[0].service, PluginService::GitDiff);
        assert_eq!(replies[0].result.as_ref().unwrap()["diff"], "+two");

        assert!(matches!(
            execute(
                PluginService::WorkspaceFileRead,
                serde_json::json!({
                    "path": "src/lib.rs",
                    "startLine": 2,
                    "endLine": 3
                }),
                "file-1"
            ),
            ServiceExecution::Forwarded
        ));
        let pig_protocol::Op::PluginWorkspaceFileRead {
            request_id,
            request,
            ..
        } = agent_rx.recv().await.expect("file op")
        else {
            panic!("expected a file read op");
        };
        assert_eq!(request.path, "src/lib.rs");
        assert_eq!(request.start_line, Some(2));
        assert_eq!(request.end_line, Some(3));
        let replies = gateway.handle_core_event(&Event::PluginServiceValue {
            request_id,
            package: "moon".into(),
            digest: "sha256:test".into(),
            result: Ok(serde_json::json!({"content": "two"})),
        });
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].request_id, "file-1");
        assert_eq!(replies[0].result.as_ref().unwrap()["content"], "two");
    }

    #[test]
    fn locale_fallback_is_stable() {
        assert_eq!(normalize_locale("zh_CN.UTF-8"), "zh-CN");
        assert_eq!(normalize_locale("C"), "en");
        assert_eq!(normalize_locale("en_US"), "en-US");
    }

    #[test]
    fn navigation_rejects_plugin_pages_and_allows_builtin_targets() {
        let (agent, _) = async_channel::bounded(1);
        let gateway = HostServiceGateway::new(PathBuf::new(), agent);
        let package = PluginPackageInfo {
            id: "moon".into(),
            name: "Moon".into(),
            version: "1".into(),
            digest: "sha256:test".into(),
            content_digest: "sha256:content".into(),
            root: PathBuf::new(),
            source: PluginSource::User,
            enabled: true,
            incompatible: None,
            backend: Some("backend.ts".into()),
            contributions: Default::default(),
            capabilities: vec![pig_protocol::PluginCapabilityRequest {
                name: "navigation.open".into(),
                targets: vec!["*".into()],
            }],
        };
        let navigate = |page_id: &str| {
            gateway.execute(
                &package,
                "moon:user:default",
                1,
                1,
                "request-1",
                PluginServiceRequest {
                    service: PluginService::NavigationOpen,
                    payload: serde_json::json!({"pageId": page_id}),
                    session_id: None,
                    workspace_id: None,
                    idempotency_key: None,
                },
            )
        };
        assert!(matches!(navigate("main"), ServiceExecution::Action(..)));
        let ServiceExecution::Complete(reply) = navigate("com.example.plugin/page") else {
            panic!("plugin page navigation must be rejected");
        };
        let Err(error) = reply.result else {
            panic!("plugin page navigation must fail");
        };
        assert_eq!(error.code, PluginErrorCode::InvalidOperation);
    }
}
