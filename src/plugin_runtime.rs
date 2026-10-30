use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use nsh_engine::{HostChannel, JsonValue, package::PublicKey};

pub type PluginEmitter = Arc<dyn Fn(&str, JsonValue) + Send + Sync>;

pub struct LoadedPlugin {
    pub bundle: nsh_engine::Bundle,
    pub assets: PathBuf,
    pub app_id: String,
}

pub fn load_verified(
    package_path: &Path,
    root_path: &Path,
    data_dir: &Path,
) -> Result<LoadedPlugin, String> {
    let root = std::fs::read_to_string(root_path)
        .map_err(|error| format!("read plugin root key: {error}"))?;
    let root = PublicKey::from_base64(root.trim()).map_err(|error| error.to_string())?;
    let mut trust = nsh_engine::package::TrustStore::open(data_dir.join("trust"))
        .map_err(|error| error.to_string())?;
    trust.add_root(root);
    let bytes = std::fs::read(package_path)
        .map_err(|error| format!("read plugin package {}: {error}", package_path.display()))?;
    let package = nsh_engine::load_package(&mut trust, &bytes, &data_dir.join("packages"))?;
    Ok(LoadedPlugin {
        bundle: package.bundle,
        assets: package.assets,
        app_id: package.app.id,
    })
}

pub fn configure_host(host: &HostChannel, slot: &str, emit: PluginEmitter) {
    let mut params = serde_json::Map::new();
    params.insert("slot".into(), JsonValue::from(slot));
    host.set_params(params);
    for action in ["composer.submit", "composer.interrupt", "composer.setDraft"] {
        let emit = emit.clone();
        host.handle(action, move |call| {
            emit(
                "pluginAction",
                serde_json::json!({
                    "name": call.name(),
                    "data": call.data().clone(),
                }),
            );
            call.resolve(JsonValue::Null);
        });
    }
}

pub fn send_metrics(host: &HostChannel, value: &JsonValue) {
    let _ = host.send("metrics.changed", value.clone());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verified_package_loads_and_wrong_root_is_refused() {
        let root = nsh_engine::package::SecretKey::generate().expect("root key");
        let release = nsh_engine::package::SecretKey::generate().expect("release key");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_secs();
        let certificate = nsh_engine::package::certify(
            &root,
            &release.public(),
            &["com.pigcodets.plugins.*".into()],
            now.saturating_sub(60),
            now + 3600,
        );
        let contents = nsh_engine::package::Contents {
            app: nsh_engine::package::App {
                id: "com.pigcodets.plugins.test".into(),
                name: "Test".into(),
                version: "1.0.0".into(),
                build: 1,
            },
            capabilities: Some(serde_json::json!({ "host": ["composer.*"] })),
            source: "export default function App() { return null; }".into(),
            bytecode: None,
            source_map: None,
            assets: Vec::new(),
            min_runtime: None,
        };
        let bytes =
            nsh_engine::package::build(&contents, &release, &certificate).expect("build package");
        let directory = std::env::temp_dir().join(format!(
            "pig-code-ts-plugin-test-{}-{}",
            std::process::id(),
            now
        ));
        std::fs::create_dir_all(&directory).expect("create test directory");
        let package_path = directory.join("test.nshpkg");
        let root_path = directory.join("root.pub");
        std::fs::write(&package_path, bytes).expect("write package");
        std::fs::write(&root_path, root.public().to_base64()).expect("write root");
        let data_dir = directory.join("data");
        let loaded = load_verified(&package_path, &root_path, &data_dir).expect("load package");
        assert_eq!(loaded.app_id, "com.pigcodets.plugins.test");

        let wrong_root = nsh_engine::package::SecretKey::generate().expect("wrong root");
        std::fs::write(&root_path, wrong_root.public().to_base64()).expect("write wrong root");
        assert!(load_verified(&package_path, &root_path, &data_dir).is_err());
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn executable_plugin_drafts_submits_and_reports_rate() {
        let host = HostChannel::new();
        let (action_tx, action_rx) = std::sync::mpsc::channel();
        configure_host(
            &host,
            "composer",
            Arc::new(move |event, data| {
                action_tx
                    .send((event.to_owned(), data))
                    .expect("record action");
            }),
        );
        let bundle = nsh_testing::compile_tsx(
            "examples/executable-plugin/App.tsx",
            &[nsh_base::registry()],
        )
        .expect("compile plugin");
        let mut config = nsh_engine::EngineConfig::new(nsh_engine::App::Js(bundle));
        config.host = host.clone();
        config.ui_sets = vec![nsh_base::registry()];
        let mut app = nsh_testing::Headless::start(config).expect("start plugin");
        app.wait_for("写一句话…").expect("composer placeholder");
        app.type_text("id:plugin-composer-input", "read the mock file")
            .expect("type draft");
        app.press("id:plugin-send-button").expect("submit draft");
        send_metrics(
            &host,
            &serde_json::json!({
                "running": true,
                "tokensPerSecond": 42,
            }),
        );
        app.wait_for("≈ 42 token/s").expect("token rate");
        app.press("id:plugin-send-button").expect("interrupt");
        let actions = action_rx
            .try_iter()
            .filter_map(|(_, data)| data.get("name")?.as_str().map(str::to_owned))
            .collect::<Vec<_>>();
        assert!(
            actions.iter().any(|name| name == "composer.setDraft"),
            "{actions:?}"
        );
        assert!(
            actions.iter().any(|name| name == "composer.submit"),
            "{actions:?}"
        );
        assert!(
            actions.iter().any(|name| name == "composer.interrupt"),
            "{actions:?}"
        );
    }
}
