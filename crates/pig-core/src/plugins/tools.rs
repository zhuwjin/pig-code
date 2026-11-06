#[cfg(test)]
use std::path::PathBuf;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use pig_protocol::{
    AppConfig, Event, PluginToolAvailability, PluginToolCall, PluginToolResult,
    PluginToolsSelection,
};
use pig_provider::ResolvedModel;

use crate::config::WorkspaceTrust;
use crate::tool::{Tool, ToolContext, ToolEffect};

#[derive(Clone, Debug, Default)]
pub struct PluginToolFilter {
    enabled: BTreeMap<(String, String), BTreeSet<String>>,
}

#[derive(Clone)]
pub struct PluginToolset {
    tools: Arc<Vec<PluginToolExecutor>>,
    dispatcher: PluginToolDispatcher,
    plugins: super::SharedPluginRegistry,
}

impl PluginToolset {
    pub fn new(
        plugins: &super::SharedPluginRegistry,
        trust: &WorkspaceTrust,
        config: Option<AppConfig>,
        session_id: &str,
    ) -> Self {
        let dispatcher = PluginToolDispatcher::new(session_id.to_owned());
        let tools = collect_tools(plugins, trust, config, &dispatcher);
        Self {
            tools,
            dispatcher,
            plugins: plugins.clone(),
        }
    }

    pub fn reload(&mut self, trust: &WorkspaceTrust, config: Option<AppConfig>) {
        self.tools = collect_tools(&self.plugins, trust, config, &self.dispatcher);
    }

    pub fn registry(&self) -> super::SharedPluginRegistry {
        self.plugins.clone()
    }

    pub fn tools(&self) -> Vec<Box<dyn Tool>> {
        self.tools
            .iter()
            .map(|tool| Box::new(tool.clone()) as Box<dyn Tool>)
            .collect()
    }

    pub fn tools_filtered(&self, filter: &PluginToolFilter) -> Vec<Box<dyn Tool>> {
        self.tools
            .iter()
            .filter(|tool| {
                tool.availability == PluginToolAvailability::Static
                    || filter
                        .enabled
                        .get(&(tool.package.clone(), tool.digest.clone()))
                        .is_some_and(|enabled| enabled.contains(&tool.name))
            })
            .map(|tool| Box::new(tool.clone()) as Box<dyn Tool>)
            .collect()
    }

    pub async fn select(
        &self,
        model: &ResolvedModel,
        agent_id: Option<&str>,
        tx: &async_channel::Sender<Event>,
    ) -> PluginToolFilter {
        let mut declared: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
        for tool in self
            .tools
            .iter()
            .filter(|tool| tool.availability == PluginToolAvailability::Plugin)
        {
            declared
                .entry((tool.package.clone(), tool.digest.clone()))
                .or_default()
                .insert(tool.name.clone());
        }
        if declared.is_empty() {
            return PluginToolFilter::default();
        }
        self.dispatcher.set_event_sender(tx.clone());
        let model = model_context(model);
        let mut queries = tokio::task::JoinSet::new();
        for ((package, digest), tools) in declared {
            let dispatcher = self.dispatcher.clone();
            let model = model.clone();
            let agent_id = agent_id.map(str::to_owned);
            queries.spawn(async move {
                let result = dispatcher
                    .select(package.clone(), digest.clone(), agent_id, model)
                    .await;
                (package, digest, tools, result)
            });
        }
        let mut filter = PluginToolFilter::default();
        while let Some(result) = queries.join_next().await {
            match result {
                Ok((package, digest, declared, Ok(selected))) => {
                    match validate_selection(&declared, selected) {
                        Ok(enabled) => {
                            filter.enabled.insert((package, digest), enabled);
                        }
                        Err(error) => {
                            tracing::warn!(
                                package,
                                "ignored an invalid plugin tool selection: {error}"
                            );
                        }
                    }
                }
                Ok((package, _, _, Err(error))) => {
                    tracing::warn!(package, "plugin tool selection failed: {error}");
                }
                Err(error) => {
                    tracing::warn!("plugin tool selection task failed: {error}");
                }
            }
        }
        filter
    }

    pub fn dispatcher(&self) -> PluginToolDispatcher {
        self.dispatcher.clone()
    }
}

fn model_context(model: &ResolvedModel) -> pig_protocol::PluginModelContext {
    pig_protocol::PluginModelContext {
        provider_name: model.provider_name.clone(),
        model_id: model.model.clone(),
        api_format: model.api_format,
        context_window: model.context_window,
        max_output_tokens: model.max_output_tokens,
        capabilities: pig_protocol::PluginModelCapabilities {
            native_web_search: pig_provider::native_web_search_enabled(model),
            input_image: model.input_image,
            input_pdf: model.input_pdf,
            structured_output: model.cap_structured,
            strict_tools: model.cap_strict_tools,
        },
    }
}

fn validate_selection(
    declared: &BTreeSet<String>,
    selected: Vec<String>,
) -> Result<BTreeSet<String>, String> {
    let mut enabled = BTreeSet::new();
    for name in selected {
        if !declared.contains(&name) {
            return Err(format!("tool `{name}` is not dynamically declared"));
        }
        if !enabled.insert(name.clone()) {
            return Err(format!("tool `{name}` was selected more than once"));
        }
    }
    Ok(enabled)
}

fn collect_tools(
    plugins: &super::SharedPluginRegistry,
    trust: &WorkspaceTrust,
    config: Option<AppConfig>,
    dispatcher: &PluginToolDispatcher,
) -> Arc<Vec<PluginToolExecutor>> {
    let reserved: std::collections::BTreeSet<String> = crate::tool::all()
        .into_iter()
        .map(|tool| tool.name().to_owned())
        .chain(
            ["Agent", "AgentSwarm", "SendMessage", "Skill"]
                .into_iter()
                .map(str::to_owned),
        )
        .collect();
    let mut names = reserved;
    let packages = {
        let registry = plugins.lock().expect("plugin registry lock");
        super::enabled_packages(&registry, trust)
    };
    let tools = packages
        .unwrap_or_else(|error| {
            tracing::warn!("could not resolve plugin tools: {error:?}");
            Vec::new()
        })
        .into_iter()
        .flat_map(|package| {
            let config = config.clone();
            let dispatcher = dispatcher.clone();
            let plugins = plugins.clone();
            package
                .contributions
                .tools
                .into_iter()
                .map(move |declaration| PluginToolExecutor {
                    name: declaration.name,
                    description: declaration.description,
                    parameters: declaration.parameters,
                    read_only: declaration.read_only,
                    availability: declaration.availability,
                    package: package.id.clone(),
                    digest: package.digest.clone(),
                    plugins: plugins.clone(),
                    config: config.clone(),
                    dispatcher: dispatcher.clone(),
                })
        })
        .filter(|tool| {
            if names.insert(tool.name.clone()) {
                true
            } else {
                tracing::warn!(
                    package = tool.package,
                    tool = tool.name,
                    "ignored a duplicate or reserved plugin tool name"
                );
                false
            }
        })
        .collect();
    Arc::new(tools)
}

#[derive(Clone)]
struct PluginToolExecutor {
    name: String,
    description: String,
    parameters: serde_json::Value,
    read_only: bool,
    availability: PluginToolAvailability,
    package: String,
    digest: String,
    plugins: super::SharedPluginRegistry,
    config: Option<AppConfig>,
    dispatcher: PluginToolDispatcher,
}

impl Tool for PluginToolExecutor {
    fn name(&self) -> &str {
        &self.name
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.parameters,
            }
        })
    }

    fn read_only(&self) -> bool {
        self.read_only
    }

    fn is_plugin(&self) -> bool {
        true
    }

    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        _ctx: ToolContext<'a>,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<ToolEffect, String>> + Send + 'a>> {
        Box::pin(async move {
            self.plugins
                .lock()
                .expect("plugin registry lock")
                .tool(
                    &self.package,
                    &self.digest,
                    &self.name,
                    self.config.as_ref(),
                )
                .map_err(|error| format!("{error:?}"))?;
            let result = self
                .dispatcher
                .call(
                    self.package.clone(),
                    self.digest.clone(),
                    self.name.clone(),
                    args,
                )
                .await?;
            match result {
                serde_json::Value::String(text) => Ok(ToolEffect::plain(text)),
                other => Ok(ToolEffect::plain(
                    serde_json::to_string_pretty(&other).unwrap_or_else(|_| other.to_string()),
                )),
            }
        })
    }
}

struct PendingPluginCall {
    package: String,
    digest: String,
    sender: async_channel::Sender<PluginToolResult>,
}

struct PendingPluginSelection {
    package: String,
    digest: String,
    sender: async_channel::Sender<PluginToolsSelection>,
}

#[derive(Clone)]
pub struct PluginToolDispatcher {
    session_id: String,
    pending: Arc<Mutex<BTreeMap<String, PendingPluginCall>>>,
    selections: Arc<Mutex<BTreeMap<String, PendingPluginSelection>>>,
    event_tx: Arc<Mutex<Option<async_channel::Sender<Event>>>>,
}

impl PluginToolDispatcher {
    fn new(session_id: String) -> Self {
        Self {
            session_id,
            pending: Arc::new(Mutex::new(BTreeMap::new())),
            selections: Arc::new(Mutex::new(BTreeMap::new())),
            event_tx: Arc::new(Mutex::new(None)),
        }
    }

    pub fn set_event_sender(&self, sender: async_channel::Sender<Event>) {
        *self.event_tx.lock().expect("plugin event sender lock") = Some(sender);
    }

    pub fn resolve(&self, result: PluginToolResult) {
        if result.session_id != self.session_id {
            tracing::warn!("ignored a plugin result for the wrong session");
            return;
        }
        let pending = {
            let mut pending = self.pending.lock().expect("plugin pending lock");
            if pending.get(&result.request_id).is_some_and(|pending| {
                pending.package != result.package || pending.digest != result.digest
            }) {
                tracing::warn!("ignored a plugin result for the wrong package or digest");
                return;
            }
            pending.remove(&result.request_id)
        };
        if let Some(pending) = pending {
            let _ = pending.sender.send_blocking(result);
        }
    }

    pub fn resolve_selection(&self, result: PluginToolsSelection) {
        if result.session_id != self.session_id {
            tracing::warn!("ignored a plugin tool selection for the wrong session");
            return;
        }
        let pending = {
            let mut selections = self.selections.lock().expect("plugin selections lock");
            if selections.get(&result.request_id).is_some_and(|pending| {
                pending.package != result.package || pending.digest != result.digest
            }) {
                tracing::warn!("ignored a plugin tool selection for the wrong package or digest");
                return;
            }
            selections.remove(&result.request_id)
        };
        if let Some(pending) = pending {
            let _ = pending.sender.send_blocking(result);
        }
    }

    async fn select(
        &self,
        package: String,
        digest: String,
        agent_id: Option<String>,
        model: pig_protocol::PluginModelContext,
    ) -> Result<Vec<String>, String> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let request_id = format!(
            "{}-plugin-selection-{}",
            self.session_id,
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let (sender, receiver) = async_channel::bounded(1);
        self.selections
            .lock()
            .expect("plugin selections lock")
            .insert(
                request_id.clone(),
                PendingPluginSelection {
                    package: package.clone(),
                    digest: digest.clone(),
                    sender,
                },
            );
        let mut guard = PendingSelection {
            dispatcher: self,
            request_id: &request_id,
            armed: true,
        };
        let sender = self
            .event_tx
            .lock()
            .expect("plugin event sender lock")
            .clone()
            .ok_or_else(|| "the plugin tool dispatcher is not attached to a session".to_owned())?;
        let query = pig_protocol::PluginToolsQuery {
            request_id: request_id.clone(),
            session_id: self.session_id.clone(),
            package,
            digest,
            agent_id,
            model,
        };
        sender
            .send(Event::PluginToolsQuery { query })
            .await
            .map_err(|_| "the plugin host is unavailable".to_owned())?;
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), receiver.recv())
            .await
            .map_err(|_| "the plugin tool selection timed out".to_owned())?
            .map_err(|_| "the plugin host closed the tool selection".to_owned())?
            .result
            .map_err(|error| format!("{error:?}"))?;
        guard.armed = false;
        Ok(result)
    }

    async fn call(
        &self,
        package: String,
        digest: String,
        tool: String,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let request_id = format!(
            "{}-plugin-tool-{}",
            self.session_id,
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let (sender, receiver) = async_channel::bounded(1);
        self.pending.lock().expect("plugin pending lock").insert(
            request_id.clone(),
            PendingPluginCall {
                package: package.clone(),
                digest: digest.clone(),
                sender,
            },
        );
        let mut guard = PendingCall {
            dispatcher: self,
            request_id: &request_id,
            armed: true,
        };
        let sender = self
            .event_tx
            .lock()
            .expect("plugin event sender lock")
            .clone()
            .ok_or_else(|| "the plugin tool dispatcher is not attached to a session".to_owned())?;
        let call = PluginToolCall {
            request_id: request_id.clone(),
            session_id: self.session_id.clone(),
            package,
            digest,
            tool,
            args,
        };
        sender
            .send(Event::PluginToolCall { call })
            .await
            .map_err(|_| "the plugin host is unavailable".to_owned())?;
        let result = tokio::time::timeout(std::time::Duration::from_secs(60), receiver.recv())
            .await
            .map_err(|_| "the plugin tool call timed out".to_owned())?
            .map_err(|_| "the plugin host closed the tool call".to_owned())?
            .result
            .map_err(|error| match error {
                pig_protocol::CoreError::PluginTransport { detail }
                | pig_protocol::CoreError::PluginPermission { detail } => detail,
                other => format!("{other:?}"),
            })?;
        guard.armed = false;
        Ok(result)
    }
}

struct PendingCall<'a> {
    dispatcher: &'a PluginToolDispatcher,
    request_id: &'a str,
    armed: bool,
}

impl Drop for PendingCall<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.dispatcher
                .pending
                .lock()
                .expect("plugin pending lock")
                .remove(self.request_id);
        }
    }
}

struct PendingSelection<'a> {
    dispatcher: &'a PluginToolDispatcher,
    request_id: &'a str,
    armed: bool,
}

impl Drop for PendingSelection<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.dispatcher
                .selections
                .lock()
                .expect("plugin selections lock")
                .remove(self.request_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn test_model(api_format: pig_protocol::ApiFormat) -> ResolvedModel {
        ResolvedModel {
            base_url: "http://localhost".into(),
            api_key: "not-exposed".into(),
            model: "test-model".into(),
            context_window: 128_000,
            max_output_tokens: 4096,
            api_format,
            reasoning_params: None,
            cap_structured: true,
            cap_strict_tools: true,
            cap_web_search: false,
            web_search_tool: None,
            input_image: true,
            input_pdf: true,
            anthropic_cache_control: false,
            replay_native_search: false,
            prompt_cache_key: None,
            provider_name: "test provider".into(),
            tool_settings: Default::default(),
        }
    }

    fn test_toolset() -> (TempDir, PluginToolset) {
        let root = std::env::temp_dir().join(format!(
            "pig-plugin-tool-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let temp = TempDir(root);
        let package = temp.0.join("package");
        let data = temp.0.join("data");
        std::fs::create_dir_all(&package).expect("package directory");
        std::fs::write(
            package.join("pig-plugin.json"),
            r#"{
              "apiVersion": 1,
              "id": "demo",
              "name": "Demo",
              "version": "1.0.0",
              "build": 1,
              "backend": "backend.ts",
              "minHostVersion": "1.0.0",
              "protocols": {"ui": 1, "services": 1},
              "capabilities": [],
              "contributions": {
                "actions": ["provide_tools"],
                "tools": [{
                  "name": "DemoRead",
                  "description": "Read demo data",
                  "read_only": true,
                  "parameters": {"type": "object"}
                }, {
                  "name": "DynamicRead",
                  "description": "Read dynamic demo data",
                  "read_only": true,
                  "availability": "plugin",
                  "parameters": {"type": "object"}
                }]
              }
            }"#,
        )
        .unwrap();
        std::fs::write(package.join("backend.ts"), "export {};").unwrap();
        let mut registry = super::super::PluginRegistry::new(data.clone());
        let snapshot = registry.sync(vec![pig_protocol::PluginPackageSource {
            root: package,
            source: pig_protocol::PluginSource::User,
        }]);
        assert!(snapshot.errors.is_empty(), "{:?}", snapshot.errors);
        let package = &snapshot.packages[0];
        registry.set_enabled("demo", &package.digest, true).unwrap();
        let plugins = registry.into_shared();
        let toolset = PluginToolset::new(
            &plugins,
            &WorkspaceTrust::untrusted(&temp.0),
            None,
            "s-plugin",
        );
        (temp, toolset)
    }

    #[tokio::test]
    async fn tool_results_are_bound_to_session_package_and_digest() {
        let (temp, toolset) = test_toolset();
        let tool = toolset.tools().into_iter().next().expect("plugin tool");
        assert!(tool.read_only());
        assert!(tool.is_plugin());
        assert_eq!(tool.schema()["function"]["name"], "DemoRead");
        let (events_tx, events_rx) = async_channel::unbounded();
        toolset.dispatcher().set_event_sender(events_tx);
        let state = crate::task::SessionToolState::for_test();
        let mut tracker = crate::tool::ChangeTracker::default();
        let cwd = temp.0.clone();
        let execution = tokio::spawn(async move {
            tool.execute(
                serde_json::json!({}),
                ToolContext {
                    cwd: &cwd,
                    tracker: &mut tracker,
                    state: &state,
                    fs_grant: None,
                    sandbox_override: Default::default(),
                    sandbox_profile: Default::default(),
                    additional_permissions: Default::default(),
                },
            )
            .await
        });
        let call = match events_rx.recv().await.expect("plugin call") {
            Event::PluginToolCall { call } => call,
            other => panic!("unexpected event: {other:?}"),
        };
        assert_eq!(call.session_id, "s-plugin");
        assert_eq!(call.tool, "DemoRead");
        let result = |digest: String| PluginToolResult {
            request_id: call.request_id.clone(),
            session_id: call.session_id.clone(),
            package: call.package.clone(),
            digest,
            result: Ok("finished".into()),
        };
        toolset.dispatcher().resolve(result("sha256:wrong".into()));
        assert_eq!(toolset.dispatcher.pending.lock().unwrap().len(), 1);
        toolset.dispatcher().resolve(result(call.digest));
        let effect = tokio::time::timeout(std::time::Duration::from_secs(2), execution)
            .await
            .expect("tool result timeout")
            .expect("tool task")
            .expect("tool effect");
        assert_eq!(effect.output, "finished");
        assert!(toolset.dispatcher.pending.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn dynamic_selection_uses_model_context_and_manifest_definitions() {
        let (_temp, toolset) = test_toolset();
        let (events_tx, events_rx) = async_channel::unbounded();
        let selecting = {
            let toolset = toolset.clone();
            tokio::spawn(async move {
                let mut model = test_model(pig_protocol::ApiFormat::OpenAiResponses);
                model.cap_web_search = true;
                toolset.select(&model, Some("a-child"), &events_tx).await
            })
        };
        let query = match events_rx.recv().await.expect("tool selection query") {
            Event::PluginToolsQuery { query } => query,
            other => panic!("unexpected event: {other:?}"),
        };
        assert_eq!(query.agent_id.as_deref(), Some("a-child"));
        assert!(query.model.capabilities.native_web_search);
        assert_eq!(query.model.model_id, "test-model");
        assert!(
            !serde_json::to_string(&query)
                .expect("serialize query")
                .contains("not-exposed")
        );
        let selection = |digest: String| PluginToolsSelection {
            request_id: query.request_id.clone(),
            session_id: query.session_id.clone(),
            package: query.package.clone(),
            digest,
            result: Ok(vec!["DynamicRead".into()]),
        };
        toolset
            .dispatcher()
            .resolve_selection(selection("sha256:wrong".into()));
        assert_eq!(toolset.dispatcher.selections.lock().unwrap().len(), 1);
        toolset
            .dispatcher()
            .resolve_selection(selection(query.digest));
        let filter = tokio::time::timeout(std::time::Duration::from_secs(2), selecting)
            .await
            .expect("selection timeout")
            .expect("selection task");
        let tools = toolset.tools_filtered(&filter);
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name(), "DemoRead");
        assert_eq!(tools[1].name(), "DynamicRead");
        assert_eq!(
            tools[1].schema()["function"]["description"],
            "Read dynamic demo data"
        );
    }

    #[tokio::test]
    async fn invalid_dynamic_selection_keeps_only_static_tools() {
        let (_temp, toolset) = test_toolset();
        let (events_tx, events_rx) = async_channel::unbounded();
        let selecting = {
            let toolset = toolset.clone();
            tokio::spawn(async move {
                toolset
                    .select(
                        &test_model(pig_protocol::ApiFormat::OpenAiChat),
                        None,
                        &events_tx,
                    )
                    .await
            })
        };
        let query = match events_rx.recv().await.expect("tool selection query") {
            Event::PluginToolsQuery { query } => query,
            other => panic!("unexpected event: {other:?}"),
        };
        assert!(!query.model.capabilities.native_web_search);
        toolset
            .dispatcher()
            .resolve_selection(PluginToolsSelection {
                request_id: query.request_id,
                session_id: query.session_id,
                package: query.package,
                digest: query.digest,
                result: Ok(vec!["NotDeclared".into()]),
            });
        let filter = selecting.await.expect("selection task");
        let tools = toolset.tools_filtered(&filter);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "DemoRead");
    }

    #[test]
    fn duplicate_dynamic_selection_is_rejected() {
        let declared = BTreeSet::from(["DynamicRead".to_owned()]);
        assert!(
            validate_selection(&declared, vec!["DynamicRead".into(), "DynamicRead".into()])
                .is_err()
        );
    }
}
