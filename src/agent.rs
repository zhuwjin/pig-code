use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nsh_core::prelude::*;
use pig_protocol::{ApprovalDecision, Event, ExecMode, MessageDelivery, Op};

static SELFTEST_ID: AtomicU64 = AtomicU64::new(0);

struct SelftestDir(PathBuf);

impl Drop for SelftestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The native connection to the Pig agent core.
#[class]
pub struct Agent {
    handle: pig_core::AgentHandle,
    cwd: PathBuf,
    data_dir: PathBuf,
    plugins: crate::plugin_system::PluginSystemManager,
    _selftest_dir: Option<SelftestDir>,
}

#[class]
impl Agent {
    /// Starts the agent in `cwd`, optionally using an isolated mock environment.
    pub fn new(cwd: Option<String>, selftest: Option<bool>) -> Self {
        let mut cwd = cwd
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let (handle, selftest_dir, data_dir) = if selftest == Some(true) {
            let id = SELFTEST_ID.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir()
                .join(format!("pig-code-ts-selftest-{}-{id}", std::process::id()));
            let workspace = root.join("workspace");
            let data_dir = root.join("data");
            std::fs::create_dir_all(&workspace).expect("create selftest workspace");
            std::fs::create_dir_all(&data_dir).expect("create selftest data dir");
            std::fs::write(
                workspace.join(pig_provider::mock::MOCK_FILE_NAME),
                pig_provider::mock::MOCK_FILE_CONTENT,
            )
            .expect("write selftest fixture");
            let port = pig_provider::mock::start_mock_server();
            let config_path = root.join("config.toml");
            std::fs::write(
                &config_path,
                format!(
                    r#"default_provider = "mock"
default_model = "mock-model"

[[providers]]
id = "mock"
name = "Mock"
base_url = "http://127.0.0.1:{port}/v1"
api_key = "mock-key"
api_format = "OpenAiChat"
enabled = true

[[providers.models]]
id = "mock-model"
context_window = 128000
max_output_tokens = 8192
reasoning_levels = ["low", "high"]
default_reasoning_level = "low"
"#
                ),
            )
            .expect("write selftest config");
            cwd = workspace.clone();
            (
                pig_core::spawn_agent_with_data_dir(Some(config_path), workspace, data_dir.clone()),
                Some(SelftestDir(root)),
                data_dir,
            )
        } else {
            let data_dir = pig_utils::data_dir();
            (pig_core::spawn_agent(None, cwd.clone()), None, data_dir)
        };
        let selftest_theme =
            (selftest == Some(true)).then(|| crate::theme::install_selftest_theme(&data_dir));
        for op in [Op::ListSessions, Op::ListWorkspaces, Op::GetConfig] {
            let _ = handle.ops.try_send(op);
        }
        let _ = handle.ops.try_send(Op::SyncPlugins {
            packages: crate::plugin_system::installed_sources(&data_dir),
        });
        if let Some(digest) = selftest_theme {
            let _ = handle.ops.try_send(Op::SetPluginEnabled {
                package: "acme.visual".into(),
                digest,
                enabled: true,
            });
        }
        Self {
            plugins: crate::plugin_system::PluginSystemManager::new(
                data_dir.clone(),
                handle.ops.clone(),
            ),
            handle,
            cwd,
            data_dir,
            _selftest_dir: selftest_dir,
        }
    }

    /// The workspace used for new sessions.
    #[getter]
    pub fn cwd(&self) -> String {
        self.cwd.display().to_string()
    }

    /// Sends any JSON-encoded `Op` from `pig-protocol`.
    pub fn send(&self, op: String) -> Result<(), String> {
        let op = serde_json::from_str(&op).map_err(|error| error.to_string())?;
        self.send_op(op)
    }

    /// Rediscovers user plugin packages in the app's data directory.
    pub fn sync_plugins(&self) -> Result<(), String> {
        self.send_op(Op::SyncPlugins {
            packages: crate::plugin_system::installed_sources(&self.data_dir),
        })
    }

    /// Enables or disables one exact plugin package snapshot.
    pub fn set_plugin_enabled(
        &self,
        package: String,
        digest: String,
        enabled: bool,
    ) -> Result<(), String> {
        self.send_op(Op::SetPluginEnabled {
            package,
            digest,
            enabled,
        })
    }

    /// Verifies and installs a signed plugin without rebuilding the app.
    pub async fn install_plugin(
        this: Instance<Self>,
        package: String,
        root: String,
    ) -> Result<String, String> {
        let plugins = this.with(|agent| agent.plugins.clone());
        tokio::task::spawn_blocking(move || {
            plugins.install(std::path::Path::new(&package), std::path::Path::new(&root))
        })
        .await
        .map_err(|error| error.to_string())?
        .map(|value| value.to_string())
    }

    /// Streams remote UI documents and plugin runtime events.
    pub async fn listen_plugins(this: Instance<Self>, #[args(events)] listener: Callback<String>) {
        let events = this.with(|agent| agent.plugins.events());
        while let Ok(event) = events.recv().await {
            listener.post(event);
        }
    }

    /// Routes a native UI event to the owning plugin engine.
    pub fn dispatch_plugin_event(
        &self,
        instance: String,
        node: String,
        kind: String,
        text: Option<String>,
    ) -> Result<(), String> {
        self.plugins
            .dispatch_ui_event(&instance, &node, &kind, text)
    }

    /// Loads validated theme definitions from an enabled-plugin snapshot.
    pub fn load_themes(&self, packages: String) -> Result<String, String> {
        crate::theme::load_themes(&packages)
    }

    /// Creates a session in `cwd` using the configured model.
    pub fn new_session(&self, cwd: String) -> Result<(), String> {
        self.send_op(Op::NewSession {
            cwd: PathBuf::from(cwd),
            provider_id: None,
            model_id: None,
            reasoning_level: None,
            permissions: Some(ExecMode::Auto.permissions()),
            plan_enabled: Some(false),
        })
    }

    /// Opens and replays an existing session.
    pub fn open_session(&self, session_id: String) -> Result<(), String> {
        self.send_op(Op::OpenSession { session_id })
    }

    /// Sends text to an existing session.
    pub fn send_message(&self, session_id: String, text: String) -> Result<(), String> {
        self.send_op(Op::SendMessage {
            session_id,
            content: text,
            files: Vec::new(),
            images: Vec::new(),
            permissions: ExecMode::Auto.permissions(),
            delivery: MessageDelivery::Queue,
        })
    }

    /// Interrupts the current turn.
    pub fn interrupt(&self, session_id: String) -> Result<(), String> {
        self.send_op(Op::Interrupt { session_id })
    }

    /// Allows or rejects one pending approval by request id.
    pub fn answer_approval(&self, request_id: String, allow: bool) -> Result<(), String> {
        self.send_op(Op::ApprovalReply {
            request_id,
            decision: if allow {
                ApprovalDecision::Allow
            } else {
                ApprovalDecision::Reject
            },
            feedback: None,
        })
    }

    /// Streams ordered JSON arrays of core events until the agent stops.
    pub async fn listen(this: Instance<Self>, #[args(events)] listener: Callback<String>) {
        let events = this.with(|agent| agent.handle.events.clone());
        let plugins = this.with(|agent| agent.plugins.clone());
        while let Ok(first) = events.recv().await {
            let mut batch = vec![first];
            while batch.len() < 128 {
                match events.try_recv() {
                    Ok(event) => batch.push(event),
                    Err(_) => break,
                }
            }
            for event in &batch {
                plugins.handle_core_event(event);
            }
            listener.post(encode_batch(&batch));
        }
    }
}

impl Agent {
    fn send_op(&self, op: Op) -> Result<(), String> {
        self.handle
            .ops
            .try_send(op)
            .map_err(|error| error.to_string())
    }
}

fn encode_batch(batch: &[Event]) -> String {
    serde_json::to_string(batch).unwrap_or_else(|error| {
        serde_json::json!([{ "BridgeError": { "detail": error.to_string() } }]).to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_batches_keep_the_protocol_order() {
        let batch = [
            Event::TurnStarted {
                session_id: "s1".into(),
                turn_id: "t1".into(),
            },
            Event::TextDelta {
                session_id: "s1".into(),
                item_id: "i1".into(),
                delta: "hello".into(),
            },
        ];
        let encoded = encode_batch(&batch);
        let decoded: Vec<Event> = serde_json::from_str(&encoded).expect("decode event batch");
        assert_eq!(decoded.len(), 2);
        assert!(matches!(
            &decoded[0],
            Event::TurnStarted { turn_id, .. } if turn_id == "t1"
        ));
        assert!(matches!(
            &decoded[1],
            Event::TextDelta { delta, .. } if delta == "hello"
        ));
    }

    #[test]
    fn generic_commands_use_the_existing_serde_contract() {
        let op: Op = serde_json::from_str(r#""ListSessions""#).expect("unit op");
        assert!(matches!(op, Op::ListSessions));
    }

    #[test]
    fn theme_preferences_round_trip_through_save_config() {
        let op = Op::SaveConfig {
            config: pig_protocol::AppConfig {
                theme: Some("ocean".into()),
                theme_mode: Some("dark".into()),
                ..Default::default()
            },
        };
        let encoded = serde_json::to_string(&op).expect("encode config");
        let decoded: Op = serde_json::from_str(&encoded).expect("decode config");
        assert!(matches!(
            decoded,
            Op::SaveConfig { config }
                if config.theme.as_deref() == Some("ocean")
                    && config.theme_mode.as_deref() == Some("dark")
        ));
    }
}
