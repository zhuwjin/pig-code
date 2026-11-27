use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;
use serde_json::{Value, json};

use crate::code_highlight::highlight_spans;
use crate::process_sample::ProcessSampler;
use crate::relay::RelayManager;
use crate::terminal::TerminalManager;
use crate::{DAEMON_PROTOCOL_VERSION, DaemonMessage};

#[derive(Clone)]
pub struct RpcContext {
    pub data_dir: PathBuf,
    pub cwd: PathBuf,
    pub selftest: bool,
    pub agent_ops: async_channel::Sender<pig_protocol::Op>,
    pub plugins: pig_plugin_host::PluginSystemManager,
    pub terminals: TerminalManager,
    pub process_sampler: Arc<ProcessSampler>,
    pub search_backfill_checked: Arc<AtomicBool>,
    pub relay: RelayManager,
    pub broadcast: Arc<dyn Fn(DaemonMessage) + Send + Sync>,
}

#[derive(Serialize)]
struct DirectoryEntry {
    name: String,
    path: String,
    is_dir: bool,
}

pub async fn handle_rpc(
    ctx: RpcContext,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    match method {
        "daemon.info" => Ok(json!({
            "cwd": ctx.cwd.display().to_string(),
            "data_dir": ctx.data_dir.display().to_string(),
            "protocol": DAEMON_PROTOCOL_VERSION,
            "pid": std::process::id(),
            "selftest": ctx.selftest,
        })),
        "system.sandbox_status" => {
            let status = pig_core::task::platform_sandbox_status();
            serde_json::to_value(status).map_err(|error| error.to_string())
        }
        "system.process_sample" => {
            let sample = ctx.process_sampler.sample();
            serde_json::to_value(sample).map_err(|error| error.to_string())
        }
        "system.list_fonts" => {
            let fonts = tokio::task::spawn_blocking(pig_core::fonts::installed_font_families)
                .await
                .map_err(|error| error.to_string())?;
            serde_json::to_value(fonts).map_err(|error| error.to_string())
        }
        "code.highlight" => {
            let code = required_str(&params, "code")?.to_string();
            let language = params
                .get("language")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let spans = tokio::task::spawn_blocking(move || highlight_spans(&code, &language))
                .await
                .map_err(|error| error.to_string())?;
            serde_json::to_value(spans).map_err(|error| error.to_string())
        }
        "session.search_content" => {
            let query = required_str(&params, "query")?.to_string();
            let data_dir = ctx.data_dir.clone();
            let backfill_checked = ctx.search_backfill_checked.clone();
            let hits = tokio::task::spawn_blocking(move || {
                let sessions_dir = data_dir.join("sessions");
                let indexed = (|| -> Result<_, String> {
                    let store = std::sync::Arc::new(std::sync::Mutex::new(
                        pig_core::store::Store::open(&data_dir)?,
                    ));
                    if !backfill_checked.swap(true, Ordering::AcqRel) {
                        let report =
                            pig_core::store::backfill_session_search(&store, &sessions_dir);
                        if report.failed > 0 {
                            tracing::warn!(
                                ?report,
                                "some legacy rollouts could not be indexed during daemon search backfill"
                            );
                        }
                    }
                    store
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .search_session_content(&query)
                })();
                match indexed {
                    Ok(hits) => hits,
                    Err(error) => {
                        tracing::warn!(%error, "daemon session search index unavailable; using JSONL fallback");
                        pig_core::rollout::search_sessions_by_content(&sessions_dir, &query)
                    }
                }
            })
            .await
            .map_err(|error| error.to_string())?;
            let encoded: Vec<_> = hits
                .into_iter()
                .map(|hit| {
                    json!({
                        "session_id": hit.session_id,
                        "before": hit.before,
                        "hit": hit.hit,
                        "after": hit.after,
                    })
                })
                .collect();
            Ok(Value::Array(encoded))
        }
        "fs.read_file" => {
            let path = required_str(&params, "path")?.to_string();
            let cwd = params
                .get("cwd")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .unwrap_or_else(|| ctx.cwd.clone());
            tokio::task::spawn_blocking(move || read_file_preview(&path, &cwd))
                .await
                .map_err(|error| error.to_string())?
        }
        "fs.list_directory" => {
            let requested = params
                .get("path")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| ctx.cwd.clone());
            let include_files = params
                .get("include_files")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            tokio::task::spawn_blocking(move || list_directory(&requested, include_files))
                .await
                .map_err(|error| error.to_string())?
        }
        "session.image_source" => {
            let session_id = required_str(&params, "session_id")?.to_string();
            let image_number = required_u32(&params, "image_number")?;
            let data_dir = ctx.data_dir.clone();
            let (bytes, mime) = tokio::task::spawn_blocking(move || {
                load_session_image(&data_dir, &session_id, image_number)
            })
            .await
            .map_err(|error| error.to_string())??;
            Ok(Value::String(format!(
                "data:{mime};base64,{}",
                pig_utils::image::base64_encode(&bytes)
            )))
        }
        "session.image" => {
            let session_id = required_str(&params, "session_id")?.to_string();
            let image_number = required_u32(&params, "image_number")?;
            let data_dir = ctx.data_dir.clone();
            let (bytes, mime) = tokio::task::spawn_blocking(move || {
                load_session_image(&data_dir, &session_id, image_number)
            })
            .await
            .map_err(|error| error.to_string())??;
            Ok(json!({ "bytes": bytes, "mime": mime }))
        }
        "terminal.open" => {
            let cwd = params
                .get("cwd")
                .and_then(Value::as_str)
                .filter(|s| {
                    let trimmed = s.trim();
                    !trimmed.is_empty() && trimmed != "."
                })
                .map(PathBuf::from)
                .unwrap_or_else(|| ctx.cwd.clone());
            let shell = params
                .get("shell")
                .and_then(Value::as_str)
                .map(str::to_string);
            let terminals = ctx.terminals.clone();
            let broadcast_out = ctx.broadcast.clone();
            let broadcast_exit = ctx.broadcast.clone();
            let info = tokio::task::spawn_blocking(move || {
                terminals.open(
                    &cwd,
                    shell.as_deref(),
                    move |terminal_id, data| {
                        (broadcast_out)(DaemonMessage::TerminalOutput { terminal_id, data });
                    },
                    move |terminal_id| {
                        (broadcast_exit)(DaemonMessage::TerminalExit { terminal_id });
                    },
                )
            })
            .await
            .map_err(|error| error.to_string())??;
            serde_json::to_value(info).map_err(|error| error.to_string())
        }
        "terminal.write" => {
            let terminal_id = required_str(&params, "terminal_id")?;
            let data = required_str(&params, "data")?;
            ctx.terminals.write(terminal_id, data)?;
            Ok(Value::Bool(true))
        }
        "terminal.resize" => {
            let terminal_id = required_str(&params, "terminal_id")?;
            let cols = required_u32(&params, "cols")?;
            let rows = required_u32(&params, "rows")?;
            ctx.terminals.resize(terminal_id, cols, rows)?;
            Ok(Value::Bool(true))
        }
        "terminal.close" => {
            let terminal_id = required_str(&params, "terminal_id")?;
            Ok(Value::Bool(ctx.terminals.close(terminal_id)))
        }
        "terminal.list" => {
            let items = ctx.terminals.list();
            serde_json::to_value(items).map_err(|error| error.to_string())
        }
        "plugin.sync" => {
            let packages = pig_plugin_host::installed_sources(&ctx.data_dir);
            ctx.agent_ops
                .send(pig_protocol::Op::SyncPlugins { packages })
                .await
                .map_err(|error| error.to_string())?;
            Ok(Value::Bool(true))
        }
        "plugin.set_enabled" => {
            let package = required_str(&params, "package")?.to_string();
            let digest = required_str(&params, "digest")?.to_string();
            let enabled = params
                .get("enabled")
                .and_then(Value::as_bool)
                .ok_or_else(|| "missing bool parameter `enabled`".to_string())?;
            ctx.agent_ops
                .send(pig_protocol::Op::SetPluginEnabled {
                    package,
                    digest,
                    enabled,
                })
                .await
                .map_err(|error| error.to_string())?;
            Ok(Value::Bool(true))
        }
        "plugin.install" => {
            let package = resolve_existing_path(required_str(&params, "package")?, &ctx.cwd);
            let root = resolve_existing_path(required_str(&params, "root")?, &ctx.cwd);
            let plugins = ctx.plugins.clone();
            tokio::task::spawn_blocking(move || plugins.install(&package, &root))
                .await
                .map_err(|error| error.to_string())?
        }
        "plugin.uninstall" => {
            let package = required_str(&params, "package")?;
            let digest = required_str(&params, "digest")?;
            ctx.plugins.uninstall(package, digest)?;
            Ok(Value::Bool(true))
        }
        "plugin.retry" => {
            let package = required_str(&params, "package")?;
            let digest = required_str(&params, "digest")?;
            ctx.plugins.retry(package, digest)?;
            Ok(Value::Bool(true))
        }
        "plugin.set_locale" => {
            let locale = required_str(&params, "locale")?;
            ctx.plugins.set_locale(locale);
            Ok(Value::Bool(true))
        }
        "plugin.dispatch_command" => {
            let instance = required_str(&params, "instance")?;
            let command = required_str(&params, "command")?;
            ctx.plugins.dispatch_command(instance, command)?;
            Ok(Value::Bool(true))
        }
        "plugin.load_themes" => {
            let packages = match params.get("packages") {
                Some(Value::String(s)) => s.clone(),
                Some(other) => serde_json::to_string(other).map_err(|error| error.to_string())?,
                None => return Err("missing parameter `packages`".to_string()),
            };
            let json = pig_plugin_host::load_themes(&packages)?;
            serde_json::from_str(&json).map_err(|error| error.to_string())
        }
        "plugin.marketplace_catalog" => {
            let index_url = params
                .get("index_url")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| pig_core::plugins::DEFAULT_MARKETPLACE_INDEX_URL.into());
            if ctx.selftest && index_url == pig_core::plugins::DEFAULT_MARKETPLACE_INDEX_URL {
                let empty = pig_protocol::PluginMarketplaceCatalog {
                    api_version: pig_protocol::PLUGIN_MARKETPLACE_API_VERSION,
                    packages: Vec::new(),
                };
                return serde_json::to_value(empty).map_err(|error| error.to_string());
            }
            let catalog = pig_core::plugins::fetch_catalog(&index_url)
                .await
                .map_err(|error| format!("{error:?}"))?;
            serde_json::to_value(catalog).map_err(|error| error.to_string())
        }
        "plugin.marketplace_sources" => {
            let sources = pig_plugin_host::marketplace::sources(&ctx.data_dir)?;
            serde_json::to_value(sources).map_err(|error| error.to_string())
        }
        "plugin.preview_marketplace" => {
            let index_url = required_str(&params, "index_url")?.to_string();
            let preview = pig_plugin_host::marketplace::preview(index_url).await?;
            serde_json::to_value(preview).map_err(|error| error.to_string())
        }
        "plugin.add_marketplace" => {
            let preview_value = params
                .get("preview")
                .ok_or_else(|| "missing parameter `preview`".to_string())?;
            let preview: pig_plugin_host::marketplace::MarketplacePreview = match preview_value {
                Value::String(text) => {
                    serde_json::from_str(text).map_err(|error| error.to_string())?
                }
                other => {
                    serde_json::from_value(other.clone()).map_err(|error| error.to_string())?
                }
            };
            let sources = pig_plugin_host::marketplace::add(&ctx.data_dir, preview)?;
            serde_json::to_value(sources).map_err(|error| error.to_string())
        }
        "plugin.remove_marketplace" => {
            let id = required_str(&params, "id")?;
            let sources = pig_plugin_host::marketplace::remove(&ctx.data_dir, id)?;
            serde_json::to_value(sources).map_err(|error| error.to_string())
        }
        "plugin.install_marketplace" => {
            let index_url = params
                .get("index_url")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| pig_core::plugins::DEFAULT_MARKETPLACE_INDEX_URL.into());
            let root = match params
                .get("root")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
            {
                Some(root) => PathBuf::from(root),
                None if index_url == pig_core::plugins::DEFAULT_MARKETPLACE_INDEX_URL => {
                    pig_plugin_host::marketplace::official_root(&ctx.data_dir).await?
                }
                None => pig_plugin_host::marketplace::trusted_root(&ctx.data_dir, &index_url)?,
            };
            let package_value = params
                .get("package")
                .ok_or_else(|| "missing parameter `package`".to_string())?;
            let package: pig_protocol::PluginMarketplacePackage = match package_value {
                Value::String(text) => {
                    serde_json::from_str(text).map_err(|error| error.to_string())?
                }
                other => {
                    serde_json::from_value(other.clone()).map_err(|error| error.to_string())?
                }
            };
            let downloaded =
                pig_core::plugins::download_signed_package(&ctx.data_dir, &index_url, &package)
                    .await
                    .map_err(|error| format!("{error:?}"))?;
            let plugins = ctx.plugins.clone();
            tokio::task::spawn_blocking(move || plugins.install(&downloaded.package_path, &root))
                .await
                .map_err(|error| error.to_string())?
        }
        "relay.status" => {
            let status = ctx.relay.status();
            serde_json::to_value(status).map_err(|error| error.to_string())
        }
        "relay.register" => {
            let relay_url = required_str(&params, "relay_url")?;
            let username = required_str(&params, "username")?;
            let password = required_str(&params, "password")?;
            let device_name = params.get("device_name").and_then(Value::as_str);
            let status = ctx
                .relay
                .register(relay_url, username, password, device_name)
                .await?;
            serde_json::to_value(status).map_err(|error| error.to_string())
        }
        "relay.login" => {
            let relay_url = required_str(&params, "relay_url")?;
            let username = required_str(&params, "username")?;
            let password = required_str(&params, "password")?;
            let device_name = params.get("device_name").and_then(Value::as_str);
            let status = ctx
                .relay
                .login(relay_url, username, password, device_name)
                .await?;
            serde_json::to_value(status).map_err(|error| error.to_string())
        }
        "relay.device_start" => {
            let relay_url = required_str(&params, "relay_url")?;
            let device_name = params.get("device_name").and_then(Value::as_str);
            let challenge = ctx.relay.device_start(relay_url, device_name).await?;
            serde_json::to_value(challenge).map_err(|error| error.to_string())
        }
        "relay.device_poll" => {
            let relay_url = required_str(&params, "relay_url")?;
            let device_code = required_str(&params, "device_code")?;
            let device_name = params.get("device_name").and_then(Value::as_str);
            ctx.relay
                .device_poll(relay_url, device_code, device_name)
                .await
        }
        "relay.approve_code" => {
            let user_code = required_str(&params, "user_code")?;
            let ok = ctx.relay.approve_device_code(user_code).await?;
            Ok(Value::Bool(ok))
        }
        "relay.logout" => {
            let status = ctx.relay.logout().await?;
            serde_json::to_value(status).map_err(|error| error.to_string())
        }
        "relay.devices" => {
            let devices = ctx.relay.list_devices().await?;
            serde_json::to_value(devices).map_err(|error| error.to_string())
        }
        "relay.revoke_device" => {
            let device_id = required_str(&params, "device_id")?;
            let ok = ctx.relay.revoke_device(device_id).await?;
            Ok(Value::Bool(ok))
        }
        "relay.reload" => {
            let status = ctx.relay.reload_from_disk();
            serde_json::to_value(status).map_err(|error| error.to_string())
        }
        "relay.sync_bots" => {
            let payload = ctx.relay.sync_bots().await?;
            serde_json::to_value(payload).map_err(|error| error.to_string())
        }
        "bot.list" => {
            let data_dir = ctx.data_dir.clone();
            let bots = tokio::task::spawn_blocking(move || -> Result<Vec<pig_protocol::BotProfile>, String> {
                let store = pig_core::store::Store::open(&data_dir)?;
                Ok(store.list_bots())
            })
            .await
            .map_err(|error| error.to_string())??;
            serde_json::to_value(bots).map_err(|error| error.to_string())
        }
        "bot.upsert" => {
            let bot_value = params
                .get("bot")
                .cloned()
                .unwrap_or(params);
            let bot: pig_protocol::BotProfile =
                serde_json::from_value(bot_value).map_err(|error| format!("invalid bot profile: {error}"))?;
            ctx.agent_ops
                .send(pig_protocol::Op::UpsertBot { bot })
                .await
                .map_err(|error| error.to_string())?;
            if ctx.relay.status().configured {
                let relay = ctx.relay.clone();
                tokio::spawn(async move {
                    let _ = relay.sync_bots().await;
                });
            }
            Ok(Value::Bool(true))
        }
        "bot.delete" => {
            let bot_id = required_str(&params, "bot_id")?.to_string();
            ctx.agent_ops
                .send(pig_protocol::Op::DeleteBot { bot_id })
                .await
                .map_err(|error| error.to_string())?;
            if ctx.relay.status().configured {
                let relay = ctx.relay.clone();
                tokio::spawn(async move {
                    let _ = relay.sync_bots().await;
                });
            }
            Ok(Value::Bool(true))
        }
        other => Err(format!("unknown daemon rpc method: {other}")),
    }
}

fn required_str<'a>(params: &'a Value, field: &str) -> Result<&'a str, String> {
    params
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing string parameter `{field}`"))
}

fn required_u32(params: &Value, field: &str) -> Result<u32, String> {
    params
        .get(field)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| format!("missing u32 parameter `{field}`"))
}

fn resolve_existing_path(raw: &str, cwd: &Path) -> PathBuf {
    let path = PathBuf::from(raw);
    if path.is_absolute() || path.exists() {
        return path;
    }
    let from_cwd = cwd.join(&path);
    if from_cwd.exists() {
        return from_cwd;
    }
    if let Ok(exe) = std::env::current_exe() {
        for ancestor in exe.ancestors().skip(1).take(5) {
            let candidate = ancestor.join(&path);
            if candidate.exists() {
                return candidate;
            }
        }
    }
    path
}

fn read_file_preview(path: &str, cwd: &Path) -> Result<Value, String> {
    let path = PathBuf::from(path);
    let path = if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    };
    let bytes = std::fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    const LIMIT: usize = 2 * 1024 * 1024;
    let truncated = bytes.len() > LIMIT;
    let content = String::from_utf8_lossy(&bytes[..bytes.len().min(LIMIT)]).into_owned();
    Ok(json!({ "content": content, "truncated": truncated }))
}

fn list_directory(path: &Path, include_files: bool) -> Result<Value, String> {
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    #[cfg(windows)]
    let resolved = {
        let s = resolved.to_string_lossy();
        if let Some(stripped) = s.strip_prefix(r"\\?\") {
            PathBuf::from(stripped)
        } else {
            resolved
        }
    };
    let mut entries = Vec::new();
    let read_dir =
        std::fs::read_dir(&resolved).map_err(|error| format!("{}: {error}", resolved.display()))?;
    for entry in read_dir.filter_map(Result::ok) {
        let entry_path = entry.path();
        let is_dir = entry
            .file_type()
            .map(|ft| ft.is_dir())
            .unwrap_or_else(|_| entry_path.is_dir());
        if !include_files && !is_dir {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        entries.push(DirectoryEntry {
            name,
            path: entry_path.display().to_string(),
            is_dir,
        });
    }
    entries.sort_by(|left, right| {
        right
            .is_dir
            .cmp(&left.is_dir)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
    });
    let parent = resolved
        .parent()
        .map(|parent| parent.display().to_string());
    let roots = filesystem_roots();
    Ok(json!({
        "path": resolved.display().to_string(),
        "parent": parent,
        "entries": entries,
        "roots": roots,
    }))
}

fn filesystem_roots() -> Vec<String> {
    #[cfg(windows)]
    {
        (b'A'..=b'Z')
            .filter_map(|drive| {
                let candidate = format!("{}:\\", drive as char);
                Path::new(&candidate).exists().then_some(candidate)
            })
            .collect()
    }
    #[cfg(not(windows))]
    {
        let mut roots = vec!["/".to_string()];
        if let Some(home) = std::env::var_os("HOME") {
            let home = PathBuf::from(home).display().to_string();
            if !home.is_empty() && home != "/" {
                roots.push(home);
            }
        }
        roots
    }
}

fn load_session_image(
    data_dir: &Path,
    session_id: &str,
    image_number: u32,
) -> Result<(Vec<u8>, String), String> {
    let directory = data_dir
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
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => "image/png",
    };
    let bytes = std::fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok((bytes, mime.to_string()))
}
