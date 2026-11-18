use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use nsh_core::{file_dialog::OpenDialog, prelude::*};
use nsh_engine::perf::ProcessSampler;
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
    process_sampler: Mutex<ProcessSampler>,
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
            let image_fixture =
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-tmp/conversation");
            std::fs::create_dir_all(&image_fixture).expect("create image fixture directory");
            std::fs::write(
                image_fixture.join("pixel.png"),
                pig_utils::image::base64_decode(
                    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNoaGj4DwAFhAKAjM1mJgAAAABJRU5ErkJggg==",
                )
                .expect("decode image fixture"),
            )
            .expect("write image fixture");
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
input_image = true
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
        for op in [Op::ListSessions, Op::ListWorkspaces, Op::GetConfig] {
            let _ = handle.ops.try_send(op);
        }
        let _ = handle.ops.try_send(Op::SyncPlugins {
            packages: crate::plugin_system::installed_sources(&data_dir),
        });
        Self {
            plugins: crate::plugin_system::PluginSystemManager::new(
                data_dir.clone(),
                handle.ops.clone(),
                cwd.clone(),
            ),
            handle,
            cwd,
            data_dir,
            process_sampler: Mutex::new(ProcessSampler::new()),
            _selftest_dir: selftest_dir,
        }
    }

    /// The workspace used for new sessions.
    #[getter]
    pub fn cwd(&self) -> String {
        self.cwd.display().to_string()
    }

    /// Installed font family names for the appearance font pickers.
    pub fn list_fonts(&self) -> Vec<String> {
        pig_core::fonts::installed_font_families()
    }

    /// The process' CPU, memory and GPU use as JSON: CPU is percent of one
    /// core since the previous call (null on the first), memory bytes, and
    /// GPU percent when the platform's performance monitor reports it (null
    /// from this sampler).
    pub fn process_sample(&self) -> String {
        let mut sampler = self
            .process_sampler
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let sample = sampler.sample();
        serde_json::json!({
            "cpu": sample.cpu,
            "memory": sample.memory,
            "gpu": sample.gpu,
        })
        .to_string()
    }

    /// Chooses a workspace directory with the system's folder picker.
    pub async fn pick_directory(this: Instance<Agent>) -> Result<Option<String>, String> {
        let _ = &this;
        let picked = pick_paths(OpenDialog::folders())
            .await
            .map_err(|error| error.to_string())?;
        Ok(picked
            .and_then(|paths| paths.into_iter().next())
            .map(|path| path.display().to_string()))
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

    /// Stops and removes one exact plugin installation.
    pub fn uninstall_plugin(&self, package: String, digest: String) -> Result<(), String> {
        self.plugins.uninstall(&package, &digest)
    }

    /// Reads the signed-plugin marketplace catalog.
    pub async fn marketplace_catalog(
        this: Instance<Self>,
        index_url: Option<String>,
    ) -> Result<String, String> {
        let index_url =
            index_url.unwrap_or_else(|| pig_core::plugins::DEFAULT_MARKETPLACE_INDEX_URL.into());
        if this.with(|agent| agent._selftest_dir.is_some())
            && index_url == pig_core::plugins::DEFAULT_MARKETPLACE_INDEX_URL
        {
            return serde_json::to_string(&pig_protocol::PluginMarketplaceCatalog {
                api_version: pig_protocol::PLUGIN_MARKETPLACE_API_VERSION,
                packages: Vec::new(),
            })
            .map_err(|error| error.to_string());
        }
        let catalog = pig_core::plugins::fetch_catalog(&index_url)
            .await
            .map_err(|error| format!("{error:?}"))?;
        serde_json::to_string(&catalog).map_err(|error| error.to_string())
    }

    /// Lists the built-in and explicitly trusted marketplaces.
    pub fn marketplace_sources(&self) -> Result<String, String> {
        let sources = crate::plugin_system::marketplace::sources(&self.data_dir)?;
        serde_json::to_string(&sources).map_err(|error| error.to_string())
    }

    /// Validates a marketplace and reads its root key without trusting it.
    pub async fn preview_marketplace(
        this: Instance<Self>,
        index_url: String,
    ) -> Result<String, String> {
        let _ = &this;
        let preview = crate::plugin_system::marketplace::preview(index_url).await?;
        serde_json::to_string(&preview).map_err(|error| error.to_string())
    }

    /// Persists a marketplace only after the user confirms its previewed root key.
    pub fn add_marketplace(&self, preview: String) -> Result<String, String> {
        let preview: crate::plugin_system::marketplace::MarketplacePreview =
            serde_json::from_str(&preview).map_err(|error| error.to_string())?;
        let sources = crate::plugin_system::marketplace::add(&self.data_dir, preview)?;
        serde_json::to_string(&sources).map_err(|error| error.to_string())
    }

    /// Removes a custom marketplace without removing its installed packages.
    pub fn remove_marketplace(&self, id: String) -> Result<String, String> {
        let sources = crate::plugin_system::marketplace::remove(&self.data_dir, &id)?;
        serde_json::to_string(&sources).map_err(|error| error.to_string())
    }

    /// Downloads and verifies one signed marketplace package.
    pub async fn install_marketplace_plugin(
        this: Instance<Self>,
        package: String,
        index_url: Option<String>,
        root: Option<String>,
    ) -> Result<String, String> {
        let index_url =
            index_url.unwrap_or_else(|| pig_core::plugins::DEFAULT_MARKETPLACE_INDEX_URL.into());
        let data_dir = this.with(|agent| agent.data_dir.clone());
        let root = match root.filter(|root| !root.trim().is_empty()) {
            Some(root) => PathBuf::from(root),
            None if index_url == pig_core::plugins::DEFAULT_MARKETPLACE_INDEX_URL => {
                crate::plugin_system::marketplace::official_root(&data_dir).await?
            }
            None => crate::plugin_system::marketplace::trusted_root(&data_dir, &index_url)?,
        };
        let package: pig_protocol::PluginMarketplacePackage =
            serde_json::from_str(&package).map_err(|error| error.to_string())?;
        let downloaded =
            pig_core::plugins::download_signed_package(&data_dir, &index_url, &package)
                .await
                .map_err(|error| format!("{error:?}"))?;
        let plugins = this.with(|agent| agent.plugins.clone());
        tokio::task::spawn_blocking(move || plugins.install(&downloaded.package_path, &root))
            .await
            .map_err(|error| error.to_string())?
            .map(|value| value.to_string())
    }

    /// Streams plugin lifecycle, service, command and contribution events.
    pub async fn listen_plugins(this: Instance<Self>, #[args(events)] listener: Callback<String>) {
        let events = this.with(|agent| agent.plugins.events());
        while let Ok(event) = events.recv().await {
            listener.post(event);
        }
    }

    /// Propagates the host locale to every independent plugin backend.
    pub fn set_plugin_locale(&self, locale: String) {
        self.plugins.set_locale(&locale);
    }

    /// Retries a failed instance after the automatic backoff budget is exhausted.
    pub fn retry_plugin(&self, package: String, digest: String) -> Result<(), String> {
        self.plugins.retry(&package, &digest)
    }

    /// Invokes a registered command on its owning independent backend.
    pub fn dispatch_plugin_command(&self, instance: String, command: String) -> Result<(), String> {
        self.plugins.dispatch_command(&instance, &command)
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

    /// Searches persisted conversation text without blocking the UI thread.
    pub async fn search_session_content(
        this: Instance<Self>,
        query: String,
    ) -> Result<String, String> {
        let sessions_dir = this.with(|agent| agent.data_dir.join("sessions"));
        let hits = tokio::task::spawn_blocking(move || {
            pig_core::rollout::search_sessions_by_content(&sessions_dir, &query)
        })
        .await
        .map_err(|error| error.to_string())?;
        let hits: Vec<_> = hits
            .into_iter()
            .map(|hit| {
                serde_json::json!({
                    "session_id": hit.session_id,
                    "before": hit.before,
                    "hit": hit.hit,
                    "after": hit.after,
                })
            })
            .collect();
        serde_json::to_string(&hits).map_err(|error| error.to_string())
    }

    /// Reads a UTF-8 text preview for the right-side file panel.
    pub fn read_file(&self, path: String, cwd: String) -> Result<String, String> {
        let path = PathBuf::from(path);
        let path = if path.is_absolute() {
            path
        } else {
            PathBuf::from(cwd).join(path)
        };
        let bytes = std::fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        const LIMIT: usize = 2 * 1024 * 1024;
        let truncated = bytes.len() > LIMIT;
        let content = String::from_utf8_lossy(&bytes[..bytes.len().min(LIMIT)]).into_owned();
        serde_json::to_string(&serde_json::json!({ "content": content, "truncated": truncated }))
            .map_err(|error| error.to_string())
    }

    /// Returns one persisted session attachment as an image data URL.
    pub fn session_image_source(
        &self,
        session_id: String,
        image_number: u32,
    ) -> Result<String, String> {
        let (bytes, mime) = self.load_session_image(&session_id, image_number)?;
        Ok(format!(
            "data:{mime};base64,{}",
            pig_utils::image::base64_encode(&bytes)
        ))
    }

    /// Returns one persisted session attachment for restoring a recalled draft.
    pub fn session_image(&self, session_id: String, image_number: u32) -> Result<String, String> {
        let (bytes, mime) = self.load_session_image(&session_id, image_number)?;
        serde_json::to_string(&serde_json::json!({ "bytes": bytes, "mime": mime }))
            .map_err(|error| error.to_string())
    }

    /// Streams ordered JSON arrays of core events until the agent stops.
    pub async fn listen(this: Instance<Self>, #[args(events)] listener: Callback<String>) {
        let events = this.with(|agent| agent.handle.journal.clone());
        let plugins = this.with(|agent| agent.plugins.clone());
        let mut projection = crate::conversation_projection::ProjectionRegistry::new();
        while let Ok(first) = events.recv().await {
            let mut batch = vec![first];
            while batch.len() < 128 {
                match events.try_recv() {
                    Ok(event) => batch.push(event),
                    Err(_) => break,
                }
            }
            for envelope in &batch {
                plugins.handle_core_event(&envelope.event);
            }
            let mut ui_batch = Vec::with_capacity(batch.len());
            for envelope in &batch {
                let Some(resync) = append_projected(&mut projection, envelope, &mut ui_batch)
                else {
                    continue;
                };
                match events.replay_or_snapshot(&envelope.epoch, resync.cursor) {
                    Ok(missed) => {
                        for missed in &missed {
                            if append_projected(&mut projection, missed, &mut ui_batch).is_some() {
                                report_projection_error(
                                    &listener,
                                    "journal replay did not restore projection order".into(),
                                );
                                return;
                            }
                        }
                    }
                    Err(snapshot) => {
                        if let Err(error) = projection.reset_with_snapshot(&snapshot) {
                            report_projection_error(&listener, error);
                            return;
                        }
                        let Some(session_id) = resync.session_id else {
                            report_projection_error(
                                &listener,
                                "global projection resync requires a journal snapshot".into(),
                            );
                            return;
                        };
                        if let Err(error) = this.with(|agent| {
                            agent.send_op(Op::OpenSession {
                                session_id: session_id.clone(),
                            })
                        }) {
                            report_projection_error(&listener, error);
                            return;
                        }
                    }
                }
            }
            if !ui_batch.is_empty() {
                listener.post(encode_batch(&ui_batch));
            }
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

    fn load_session_image(
        &self,
        session_id: &str,
        image_number: u32,
    ) -> Result<(Vec<u8>, String), String> {
        let directory = self
            .data_dir
            .join("sessions")
            .join(format!("{session_id}.media"));
        let expected = image_number.to_string();
        let path = std::fs::read_dir(&directory)
            .map_err(|error| format!("{}: {error}", directory.display()))?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.file_stem()
                    .is_some_and(|stem| stem == expected.as_str())
            })
            .ok_or_else(|| format!("image {image_number} not found"))?;
        let mime = match path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("png")
        {
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            _ => "image/png",
        };
        let bytes = std::fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        Ok((bytes, mime.to_string()))
    }
}

fn append_projected(
    projection: &mut crate::conversation_projection::ProjectionRegistry,
    envelope: &pig_protocol::EventEnvelope,
    output: &mut Vec<Event>,
) -> Option<crate::conversation_projection::ProjectionResync> {
    let projected = projection.project(envelope);
    output.extend(projected.events);
    if projected.pass {
        output.push(envelope.event.clone());
    }
    projected.resync
}

fn report_projection_error(listener: &Callback<String>, detail: String) {
    listener.post(serde_json::json!([{ "BridgeError": { "detail": detail } }]).to_string());
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
                run: None,
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
    fn selftest_image_fixture_is_accepted_by_the_model_pipeline() {
        let bytes = pig_utils::image::base64_decode(
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNoaGj4DwAFhAKAjM1mJgAAAABJRU5ErkJggg==",
        )
        .expect("decode fixture");
        pig_core::tool::compress_image_for_model(&bytes, "image/png")
            .expect("compress selftest image fixture");
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
