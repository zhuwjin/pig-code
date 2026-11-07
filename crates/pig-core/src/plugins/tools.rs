#[cfg(test)]
use std::path::PathBuf;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use pig_protocol::{
    AppConfig, Event, PluginErrorCode, PluginProtocolError, PluginTool, PluginToolAvailability,
    PluginToolCall, PluginToolResult, PluginToolsSelection,
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
                tool.declaration.availability == PluginToolAvailability::Static
                    || filter
                        .enabled
                        .get(&(tool.package.clone(), tool.digest.clone()))
                        .is_some_and(|enabled| enabled.contains(&tool.declaration.name))
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
            .filter(|tool| tool.declaration.availability == PluginToolAvailability::Plugin)
        {
            declared
                .entry((tool.package.clone(), tool.digest.clone()))
                .or_default()
                .insert(tool.declaration.name.clone());
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
                    package: package.id.clone(),
                    digest: package.digest.clone(),
                    plugins: plugins.clone(),
                    config: config.clone(),
                    dispatcher: dispatcher.clone(),
                    declaration,
                })
        })
        .filter(|tool| {
            if names.insert(tool.declaration.name.clone()) {
                true
            } else {
                tracing::warn!(
                    package = tool.package,
                    tool = tool.declaration.name,
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
    package: String,
    digest: String,
    plugins: super::SharedPluginRegistry,
    config: Option<AppConfig>,
    dispatcher: PluginToolDispatcher,
    declaration: PluginTool,
}

impl Tool for PluginToolExecutor {
    fn name(&self) -> &str {
        &self.declaration.name
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": self.declaration.name,
                "description": self.declaration.description,
                "parameters": self.declaration.parameters,
            }
        })
    }

    fn read_only(&self) -> bool {
        self.declaration.read_only
    }

    fn is_plugin(&self) -> bool {
        true
    }

    fn plugin_tool(&self) -> Option<&PluginTool> {
        Some(&self.declaration)
    }

    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: ToolContext<'a>,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<ToolEffect, String>> + Send + 'a>> {
        Box::pin(async move {
            self.plugins
                .lock()
                .expect("plugin registry lock")
                .tool(
                    &self.package,
                    &self.digest,
                    &self.declaration.name,
                    self.config.as_ref(),
                )
                .map_err(|error| format!("{error:?}"))?;
            if let Err(issues) = super::schema::validate(&self.declaration.parameters, &args) {
                return Err(format!(
                    "plugin tool `{}` rejected the arguments [invalid_input]: {}",
                    self.declaration.name,
                    issues.join("; ")
                ));
            }
            let result = self
                .dispatcher
                .call(
                    self.package.clone(),
                    self.digest.clone(),
                    self.declaration.name.clone(),
                    args,
                    self.declaration.timeout(),
                )
                .await
                .map_err(|error| {
                    format!(
                        "plugin tool `{}` failed [{}]: {}",
                        self.declaration.name,
                        error.code.as_str(),
                        error.message
                    )
                })?;
            if let Some(output_schema) = &self.declaration.output_schema
                && let Err(issues) = super::schema::validate(output_schema, &result)
            {
                return Err(format!(
                    "plugin tool `{}` returned an invalid result [invalid_output]: {}",
                    self.declaration.name,
                    issues.join("; ")
                ));
            }
            let text = match result {
                serde_json::Value::String(text) => text,
                other => serde_json::to_string_pretty(&other).unwrap_or_else(|_| other.to_string()),
            };
            Ok(ToolEffect::plain(budget_output(
                text,
                self.declaration.output_budget(),
                ctx.state,
                &self.declaration.name,
            )))
        })
    }
}

/// Keeps a plugin result within its declared inline budget. A larger result is
/// written to the session's `tool-results` directory (already a read root for
/// the model) and replaced by a head/tail preview pointing at the file, so a
/// chatty tool cannot flood the model's context.
fn budget_output(
    text: String,
    budget: usize,
    state: &crate::task::SessionToolState,
    tool: &str,
) -> String {
    if text.len() <= budget {
        return text;
    }
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let id = format!(
        "plugin-{}-{}",
        tool.replace(['/', '\\'], "_"),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let spill = state.spill_path(&id);
    let target = spill.as_ref().filter(|path| {
        path.parent()
            .is_some_and(|parent| std::fs::create_dir_all(parent).is_ok())
            // `create_new` never follows an existing file or link: a planted
            // path makes the spill fail (and the output stay truncated) instead
            // of writing through it.
            && std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .and_then(|mut file| std::io::Write::write_all(&mut file, text.as_bytes()))
                .is_ok()
    });
    let marker = |omitted: usize| match &target {
        Some(path) => format!(
            "\n\n…[{omitted} bytes omitted; the full result is in {}]\n\n",
            path.display()
        ),
        None => format!("\n\n…[{omitted} bytes omitted; the result exceeds the inline limit]\n\n"),
    };
    // The maximum omitted count reserves enough room for the complete marker.
    // The final marker is usually shorter, leaving a few unused bytes rather
    // than pushing the model-facing result past its declared budget.
    let preview_budget = budget.saturating_sub(marker(text.len()).len());
    let head_budget = preview_budget.saturating_mul(3) / 4;
    let tail_budget = preview_budget.saturating_sub(head_budget);
    let head_end = char_boundary(&text, head_budget);
    let tail_start = char_boundary_rev(&text, text.len().saturating_sub(tail_budget));
    let head = &text[..head_end];
    let tail = &text[tail_start..];
    let omitted = text.len().saturating_sub(head.len() + tail.len());
    let mut output = format!("{head}{}{tail}", marker(omitted));
    if output.len() > budget {
        output.truncate(char_boundary(&output, budget));
    }
    output
}

fn char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn char_boundary_rev(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
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
    ) -> Result<Vec<String>, PluginProtocolError> {
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
            .ok_or_else(|| {
                PluginProtocolError::new(
                    PluginErrorCode::NotActive,
                    "the plugin tool dispatcher is not attached to a session",
                )
            })?;
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
            .map_err(|_| {
                PluginProtocolError::new(
                    PluginErrorCode::Transport,
                    "the plugin host is unavailable",
                )
            })?;
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), receiver.recv())
            .await
            .map_err(|_| {
                PluginProtocolError::new(
                    PluginErrorCode::Timeout,
                    "the plugin tool selection timed out",
                )
            })?
            .map_err(|_| {
                PluginProtocolError::new(
                    PluginErrorCode::Transport,
                    "the plugin host closed the tool selection",
                )
            })?
            .result
            .map_err(|error| error)?;
        guard.armed = false;
        Ok(result)
    }

    async fn call(
        &self,
        package: String,
        digest: String,
        tool: String,
        args: serde_json::Value,
        timeout: std::time::Duration,
    ) -> Result<serde_json::Value, PluginProtocolError> {
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
            .ok_or_else(|| {
                PluginProtocolError::new(
                    PluginErrorCode::NotActive,
                    "the plugin tool dispatcher is not attached to a session",
                )
            })?;
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
            .map_err(|_| {
                PluginProtocolError::new(
                    PluginErrorCode::Transport,
                    "the plugin host is unavailable",
                )
            })?;
        let result = tokio::time::timeout(timeout, receiver.recv())
            .await
            .map_err(|_| {
                PluginProtocolError::new(PluginErrorCode::Timeout, "the plugin tool call timed out")
                    .retryable()
            })?
            .map_err(|_| {
                PluginProtocolError::new(
                    PluginErrorCode::Transport,
                    "the plugin host closed the tool call",
                )
            })?
            .result?;
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
            // The caller is gone (turn cancellation or the dispatcher's own
            // timeout): tell the host so it can stop the backend and settle
            // the call instead of leaving it running to its deadline.
            if let Some(sender) = self
                .dispatcher
                .event_tx
                .lock()
                .expect("plugin event sender lock")
                .clone()
            {
                let _ = sender.try_send(Event::PluginToolCancel {
                    session_id: self.dispatcher.session_id.clone(),
                    request_id: self.request_id.to_owned(),
                });
            }
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
        test_toolset_with_tools(
            r#"[{
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
            }]"#,
        )
    }

    fn test_toolset_with_tools(tools: &str) -> (TempDir, PluginToolset) {
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
            format!(
                r#"{{
                  "apiVersion": 1,
                  "id": "demo",
                  "name": "Demo",
                  "version": "1.0.0",
                  "build": 1,
                  "backend": "backend.ts",
                  "minHostVersion": "1.0.0",
                  "protocols": {{"ui": 1, "services": 1}},
                  "capabilities": [],
                  "contributions": {{
                    "actions": ["provide_tools"],
                    "tools": {tools}
                  }}
                }}"#
            ),
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

    const CONTRACT_TOOL: &str = r#"[{
      "name": "ContractTool",
      "description": "A tool with a full contract",
      "read_only": true,
      "parameters": {
        "type": "object",
        "properties": {"text": {"type": "string", "minLength": 1}},
        "required": ["text"],
        "additionalProperties": false
      },
      "output_schema": {"type": "object", "required": ["echo"]},
      "max_output_bytes": 1024
    }]"#;

    async fn run_contract_tool(
        reply: impl FnOnce(&PluginToolCall) -> Result<serde_json::Value, PluginProtocolError>,
    ) -> Result<ToolEffect, String> {
        let (temp, toolset) = test_toolset_with_tools(CONTRACT_TOOL);
        let tool = toolset.tools().into_iter().next().expect("plugin tool");
        let (events_tx, events_rx) = async_channel::unbounded();
        toolset.dispatcher().set_event_sender(events_tx);
        let state = crate::task::SessionToolState::for_test();
        let cwd = temp.0.clone();
        let task_state = state.clone();
        let task = tokio::spawn(async move {
            let mut tracker = crate::tool::ChangeTracker::default();
            tool.execute(
                serde_json::json!({"text": "hello"}),
                ToolContext {
                    cwd: &cwd,
                    tracker: &mut tracker,
                    state: &task_state,
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
        toolset.dispatcher().resolve(PluginToolResult {
            request_id: call.request_id.clone(),
            session_id: call.session_id.clone(),
            package: call.package.clone(),
            digest: call.digest.clone(),
            result: reply(&call),
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .expect("tool result timeout")
            .expect("tool task")
    }

    #[tokio::test]
    async fn invalid_arguments_are_rejected_before_the_backend_is_called() {
        let (temp, toolset) = test_toolset_with_tools(CONTRACT_TOOL);
        let tool = toolset.tools().into_iter().next().expect("plugin tool");
        let (events_tx, events_rx) = async_channel::unbounded();
        toolset.dispatcher().set_event_sender(events_tx);
        let state = crate::task::SessionToolState::for_test();
        let mut tracker = crate::tool::ChangeTracker::default();
        let error = match tool
            .execute(
                serde_json::json!({"text": "", "extra": 1}),
                ToolContext {
                    cwd: &temp.0,
                    tracker: &mut tracker,
                    state: &state,
                    fs_grant: None,
                    sandbox_override: Default::default(),
                    sandbox_profile: Default::default(),
                    additional_permissions: Default::default(),
                },
            )
            .await
        {
            Ok(effect) => panic!("expected invalid arguments, got {}", effect.output),
            Err(error) => error,
        };
        assert!(error.contains("invalid_input"), "{error}");
        assert!(error.contains("$.text"), "{error}");
        assert!(error.contains("unexpected `extra`"), "{error}");
        assert!(
            events_rx.try_recv().is_err(),
            "the backend must not see an invalid call"
        );
    }

    #[tokio::test]
    async fn invalid_output_is_rejected_before_the_model_sees_it() {
        let error = match run_contract_tool(|_| Ok(serde_json::json!({"wrong": true}))).await {
            Ok(effect) => panic!("expected an invalid output, got {}", effect.output),
            Err(error) => error,
        };
        assert!(error.contains("invalid_output"), "{error}");
        assert!(error.contains("missing required `echo`"), "{error}");
    }

    #[tokio::test]
    async fn structured_backend_errors_keep_their_code() {
        let error = match run_contract_tool(|_| {
            Err(
                PluginProtocolError::new(PluginErrorCode::Timeout, "the backend gave up")
                    .retryable(),
            )
        })
        .await
        {
            Ok(effect) => panic!("expected a backend error, got {}", effect.output),
            Err(error) => error,
        };
        assert!(error.contains("[timeout]"), "{error}");
        assert!(error.contains("the backend gave up"), "{error}");
    }

    #[tokio::test]
    async fn oversized_results_are_spilled_and_replaced_by_a_preview() {
        let (temp, toolset) = test_toolset_with_tools(CONTRACT_TOOL);
        let tool = toolset.tools().into_iter().next().expect("plugin tool");
        let (events_tx, events_rx) = async_channel::unbounded();
        toolset.dispatcher().set_event_sender(events_tx);
        let state = crate::task::SessionToolState::for_test();
        let data_dir = state.data_dir().expect("test data dir").to_path_buf();
        let cwd = temp.0.clone();
        let task_state = state.clone();
        let task = tokio::spawn(async move {
            let mut tracker = crate::tool::ChangeTracker::default();
            tool.execute(
                serde_json::json!({"text": "hello"}),
                ToolContext {
                    cwd: &cwd,
                    tracker: &mut tracker,
                    state: &task_state,
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
        let result = serde_json::json!({"echo": "x".repeat(5000)});
        let rendered = serde_json::to_string_pretty(&result).unwrap();
        toolset.dispatcher().resolve(PluginToolResult {
            request_id: call.request_id,
            session_id: call.session_id,
            package: call.package,
            digest: call.digest,
            result: Ok(result),
        });
        let effect = tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .expect("tool result timeout")
            .expect("tool task")
            .expect("tool effect");
        assert!(effect.output.contains("bytes omitted"), "{}", effect.output);
        assert!(effect.output.len() < 2048, "preview stays small");
        let dir = data_dir.join("sessions/test/tool-results");
        let files: Vec<_> = std::fs::read_dir(&dir)
            .expect("spill directory")
            .filter_map(Result::ok)
            .collect();
        assert_eq!(files.len(), 1, "one spill file");
        assert_eq!(std::fs::read_to_string(files[0].path()).unwrap(), rendered);
    }

    #[test]
    fn output_preview_and_its_marker_stay_within_the_declared_budget() {
        let state = crate::task::SessionToolState::for_test();
        for budget in [1024, 1031, 2048] {
            let output = budget_output("界".repeat(2000), budget, &state, "ContractTool");
            assert!(output.len() <= budget, "{} > {budget}", output.len());
            assert!(output.contains("bytes omitted"), "{output}");
        }
    }

    const TIMEOUT_TOOL: &str = r#"[{
      "name": "SlowTool",
      "description": "Never answers in time",
      "read_only": true,
      "timeout_ms": 10,
      "parameters": {"type": "object"}
    }]"#;

    #[test]
    fn a_declared_approval_stance_changes_the_gate() {
        let tool = |approval: &str, read_only: bool| {
            let (temp, toolset) = test_toolset_with_tools(&format!(
                r#"[{{"name": "Gated", "description": "Gated", "read_only": {read_only}, "approval": "{approval}", "parameters": {{"type": "object"}}}}]"#
            ));
            (
                temp,
                toolset.tools().into_iter().next().expect("plugin tool"),
            )
        };
        let (_always, always) = tool("always", true);
        assert!(crate::tool::requires_approval(
            always.as_ref(),
            pig_protocol::ExecMode::Auto.permissions()
        ));
        assert!(!crate::tool::requires_approval(
            always.as_ref(),
            pig_protocol::ExecMode::Full.permissions()
        ));
        let (_never, never) = tool("never", true);
        assert!(!crate::tool::requires_approval(
            never.as_ref(),
            pig_protocol::ExecMode::ReadOnly.permissions()
        ));
        let (_default, default) = tool("on_request", false);
        assert!(crate::tool::requires_approval(
            default.as_ref(),
            pig_protocol::ExecMode::ReadOnly.permissions()
        ));
        assert!(!crate::tool::requires_approval(
            default.as_ref(),
            pig_protocol::ExecMode::Auto.permissions()
        ));
    }

    #[tokio::test]
    async fn a_late_result_after_a_timeout_is_settled_once_and_ignored() {
        let (temp, toolset) = test_toolset_with_tools(TIMEOUT_TOOL);
        let tool = toolset.tools().into_iter().next().expect("plugin tool");
        let (events_tx, events_rx) = async_channel::unbounded();
        toolset.dispatcher().set_event_sender(events_tx);
        let state = crate::task::SessionToolState::for_test();
        let cwd = temp.0.clone();
        let task_state = state.clone();
        let task = tokio::spawn(async move {
            let mut tracker = crate::tool::ChangeTracker::default();
            tool.execute(
                serde_json::json!({}),
                ToolContext {
                    cwd: &cwd,
                    tracker: &mut tracker,
                    state: &task_state,
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
        let error = match tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .expect("tool result timeout")
            .expect("tool task")
        {
            Ok(effect) => panic!("expected a timeout, got {}", effect.output),
            Err(error) => error,
        };
        assert!(error.contains("[timeout]"), "{error}");
        assert!(
            toolset.dispatcher.pending.lock().unwrap().is_empty(),
            "the call was settled"
        );

        // The backend answers after the fact: the dispatcher has no waiter and
        // must drop it instead of reviving the call.
        toolset.dispatcher().resolve(PluginToolResult {
            request_id: call.request_id,
            session_id: call.session_id,
            package: call.package,
            digest: call.digest,
            result: Ok(serde_json::json!({"late": true})),
        });
        assert!(toolset.dispatcher.pending.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn dropping_a_call_tells_the_host_to_cancel_it() {
        let (temp, toolset) = test_toolset_with_tools(TIMEOUT_TOOL);
        let tool = toolset.tools().into_iter().next().expect("plugin tool");
        let (events_tx, events_rx) = async_channel::unbounded();
        toolset.dispatcher().set_event_sender(events_tx);
        let state = crate::task::SessionToolState::for_test();
        let cwd = temp.0.clone();
        let task_state = state.clone();
        let task = tokio::spawn(async move {
            let mut tracker = crate::tool::ChangeTracker::default();
            let _ = tool
                .execute(
                    serde_json::json!({}),
                    ToolContext {
                        cwd: &cwd,
                        tracker: &mut tracker,
                        state: &task_state,
                        fs_grant: None,
                        sandbox_override: Default::default(),
                        sandbox_profile: Default::default(),
                        additional_permissions: Default::default(),
                    },
                )
                .await;
        });
        let call = match events_rx.recv().await.expect("plugin call") {
            Event::PluginToolCall { call } => call,
            other => panic!("unexpected event: {other:?}"),
        };
        task.abort();
        let _ = task.await;
        let cancel = tokio::time::timeout(std::time::Duration::from_secs(2), events_rx.recv())
            .await
            .expect("cancel timeout")
            .expect("cancel event");
        assert!(
            matches!(&cancel, Event::PluginToolCancel { request_id, .. } if request_id == &call.request_id),
            "unexpected event: {cancel:?}"
        );
        assert!(toolset.dispatcher.pending.lock().unwrap().is_empty());
    }
}
