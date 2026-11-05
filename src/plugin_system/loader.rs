use std::path::{Path, PathBuf};

use nsh_engine::package::PublicKey;
use pig_protocol::{InstalledPluginMetadata, PluginManifest, PluginPackageSource, PluginSource};

pub struct VerifiedPlugin {
    pub bundle: nsh_engine::Bundle,
    pub assets: PathBuf,
    pub manifest: PluginManifest,
    pub digest: String,
}

pub fn load_verified(
    package_path: &Path,
    root_path: &Path,
    data_dir: &Path,
) -> Result<VerifiedPlugin, String> {
    let root = std::fs::read_to_string(root_path)
        .map_err(|error| format!("read plugin root key: {error}"))?;
    let root = PublicKey::from_base64(root.trim()).map_err(|error| error.to_string())?;
    let mut trust = nsh_engine::package::TrustStore::open(data_dir.join("trust"))
        .map_err(|error| error.to_string())?;
    trust.add_root(root);
    if std::fs::metadata(package_path)
        .map_err(|error| format!("inspect plugin package: {error}"))?
        .len()
        > 16 * 1024 * 1024
    {
        return Err("plugin package exceeds 16 MiB".into());
    }
    let bytes = std::fs::read(package_path)
        .map_err(|error| format!("read plugin package {}: {error}", package_path.display()))?;
    let package = nsh_engine::load_package(&mut trust, &bytes, &data_dir.join("packages"))?;
    let manifest: PluginManifest = read_manifest(&package.assets)?;
    if manifest.id != package.app.id
        || manifest.version != package.app.version
        || manifest.build != package.app.build
    {
        return Err("plugin identity does not match the signed package".into());
    }
    if manifest.api_version != pig_protocol::PLUGIN_API_VERSION
        || manifest.protocols.ui != 1
        || manifest.protocols.services != 1
        || manifest.build == 0
    {
        return Err("unsupported plugin manifest or protocol version".into());
    }
    let digest = package.receipt.sha256;
    pig_core::config::write_settings_file(&package.assets.join(".pig-package.nshpkg"), &bytes)
        .map_err(|error| format!("store verified plugin package: {error}"))?;
    let stored_root = package.assets.join(".pig-root.pub");
    let same_root =
        std::fs::canonicalize(root_path).ok() == std::fs::canonicalize(&stored_root).ok();
    if !same_root {
        let root_bytes = std::fs::read(root_path)
            .map_err(|error| format!("read plugin root public key: {error}"))?;
        pig_core::config::write_settings_file(&stored_root, &root_bytes)
            .map_err(|error| format!("store plugin root public key: {error}"))?;
    }
    pig_core::config::write_settings_file(
        &package.assets.join(".pig-install.json"),
        &serde_json::to_vec(&InstalledPluginMetadata {
            digest: digest.clone(),
        })
        .map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("store plugin metadata: {error}"))?;
    Ok(VerifiedPlugin {
        bundle: package.bundle,
        assets: package.assets,
        manifest,
        digest,
    })
}

pub fn read_manifest(assets: &Path) -> Result<PluginManifest, String> {
    let path = assets.join(pig_protocol::PLUGIN_MANIFEST_FILE);
    let bytes = std::fs::read(&path)
        .map_err(|error| format!("read plugin manifest {}: {error}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|error| format!("invalid plugin manifest: {error}"))
}

pub fn installed_sources(data_dir: &Path) -> Vec<PluginPackageSource> {
    let mut sources = Vec::new();
    let root = data_dir.join("packages");
    let Ok(packages) = std::fs::read_dir(root) else {
        return sources;
    };
    for package in packages.filter_map(Result::ok) {
        let Ok(digests) = std::fs::read_dir(package.path()) else {
            continue;
        };
        for digest in digests.filter_map(Result::ok) {
            let assets = digest.path().join("assets");
            if assets.join(pig_protocol::PLUGIN_MANIFEST_FILE).is_file()
                && assets.join(".pig-package.nshpkg").is_file()
            {
                sources.push(PluginPackageSource {
                    root: assets,
                    source: PluginSource::User,
                });
            }
        }
    }
    sources.sort_by(|a, b| a.root.cmp(&b.root));
    sources.dedup_by(|a, b| a.root == b.root);
    sources
}

pub fn installed_package_path(assets: &Path) -> PathBuf {
    assets.join(".pig-package.nshpkg")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_token_stats_package_loads_without_a_backend_engine() {
        let directory =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-tmp/token-stats-plugin");
        std::fs::create_dir_all(&directory).expect("create token stats fixture directory");
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
        let plugin = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../pig-code-ts-plugins/packages/token-stats-plugin");
        let bundle = nsh_testing::compile_tsx(plugin.join("App.tsx"), &[nsh_base::registry()])
            .expect("compile token stats plugin");
        let (source, bytecode) = match bundle.code {
            nsh_engine::BundleCode::Source(source) => (source, None),
            nsh_engine::BundleCode::Bytecode(bytes) => (
                String::new(),
                Some((bytes, nsh_engine::BYTECODE_VERSION.to_owned())),
            ),
        };
        let manifest: PluginManifest = serde_json::from_slice(
            &std::fs::read(plugin.join(pig_protocol::PLUGIN_MANIFEST_FILE)).expect("token stats manifest"),
        )
        .expect("valid token stats manifest");
        let contents = nsh_engine::package::Contents {
            app: nsh_engine::package::App {
                id: manifest.id.clone(),
                name: "Token Stats".into(),
                version: manifest.version.clone(),
                build: manifest.build,
            },
            capabilities: Some(serde_json::json!({"host": ["pig.host"]})),
            source,
            bytecode,
            source_map: bundle.source_map.map(|map| map.to_string()),
            assets: vec![(
                pig_protocol::PLUGIN_MANIFEST_FILE.into(),
                std::fs::read(plugin.join(pig_protocol::PLUGIN_MANIFEST_FILE)).expect("manifest bytes"),
            )],
            min_runtime: None,
        };
        let bytes = nsh_engine::package::build(&contents, &release, &certificate)
            .expect("build signed token stats package");
        let package_path = directory.join("token-stats.nshpkg");
        let root_path = directory.join("root.pub");
        std::fs::write(&package_path, bytes).expect("write token stats package");
        std::fs::write(&root_path, root.public().to_base64()).expect("write root public key");
        let loaded = load_verified(&package_path, &root_path, &directory.join("data"))
            .expect("load signed token stats package");
        assert_eq!(loaded.manifest.contributions.ui.len(), 9);
        assert!(!loaded.manifest.contributions.is_engine_backed());
        let mut workers = Vec::new();
        for _ in 0..4 {
            let package_path = package_path.clone();
            let root_path = root_path.clone();
            let data_dir = directory.join("data");
            workers.push(std::thread::spawn(move || {
                for _ in 0..3 {
                    load_verified(&package_path, &root_path, &data_dir)?;
                }
                Ok::<(), String>(())
            }));
        }
        for worker in workers {
            worker
                .join()
                .expect("concurrent verifier did not panic")
                .expect("concurrent package verification succeeds");
        }
    }

    #[test]
    fn signed_moon_package_loads_and_wrong_root_is_refused() {
        let directory =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-tmp/moon-plugin");
        std::fs::create_dir_all(&directory).expect("create fixture directory");
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
        let bundle =
            nsh_testing::compile_tsx("examples/moon-plugin/App.tsx", &[nsh_base::registry()])
                .expect("compile moon plugin");
        let (source, bytecode) = match bundle.code {
            nsh_engine::BundleCode::Source(source) => (source, None),
            nsh_engine::BundleCode::Bytecode(bytes) => (
                String::new(),
                Some((bytes, nsh_engine::BYTECODE_VERSION.to_owned())),
            ),
        };
        let source_map = bundle.source_map.map(|map| map.to_string());
        let manifest_source = std::fs::read_to_string("examples/moon-plugin/pig-plugin.json")
            .expect("read moon manifest");
        let manifest: PluginManifest =
            serde_json::from_str(&manifest_source).expect("valid moon manifest");
        let contents = nsh_engine::package::Contents {
            app: nsh_engine::package::App {
                id: manifest.id.clone(),
                name: "Moon UI".into(),
                version: manifest.version.clone(),
                build: manifest.build,
            },
            capabilities: Some(serde_json::json!({
                "host": ["plugin.protocol", "pig.host"]
            })),
            source,
            bytecode,
            source_map,
            assets: vec![
                (
                    pig_protocol::PLUGIN_MANIFEST_FILE.into(),
                    std::fs::read("examples/moon-plugin/pig-plugin.json").expect("manifest bytes"),
                ),
                (
                    "moon-theme.json".into(),
                    std::fs::read("examples/moon-plugin/moon-theme.json").expect("theme bytes"),
                ),
                (
                    "moon-glass.json".into(),
                    std::fs::read("examples/moon-plugin/moon-glass.json").expect("style bytes"),
                ),
                (
                    "background.svg".into(),
                    std::fs::read("examples/moon-plugin/background.svg").expect("background bytes"),
                ),
                (
                    "skills/moon/SKILL.md".into(),
                    std::fs::read("examples/moon-plugin/skills/moon/SKILL.md")
                        .expect("skill bytes"),
                ),
            ],
            min_runtime: None,
        };
        let bytes = nsh_engine::package::build(&contents, &release, &certificate)
            .expect("build signed moon package");
        let package_path = directory.join("moon.nshpkg");
        let root_path = directory.join("root.pub");
        std::fs::write(&package_path, bytes).expect("write moon package");
        std::fs::write(&root_path, root.public().to_base64()).expect("write root public key");
        let loaded = load_verified(&package_path, &root_path, &directory.join("data"))
            .expect("load signed moon package");
        assert_eq!(loaded.manifest.id, "com.pigcodets.plugins.moon");
        assert_eq!(loaded.manifest.contributions.ui.len(), 4);
        let first_digest =
            pig_core::plugins::package_digest(&loaded.assets).expect("first content digest");
        load_verified(
            &installed_package_path(&loaded.assets),
            &root_path,
            &directory.join("data"),
        )
        .expect("reload signed moon package");
        let second_digest =
            pig_core::plugins::package_digest(&loaded.assets).expect("second content digest");
        assert_eq!(first_digest, second_digest);
        let wrong = nsh_engine::package::SecretKey::generate().expect("wrong root");
        let wrong_path = directory.join("wrong.pub");
        std::fs::write(&wrong_path, wrong.public().to_base64()).expect("write wrong public key");
        assert!(load_verified(&package_path, &wrong_path, &directory.join("wrong-data")).is_err());
    }
}
