use std::path::{Path, PathBuf};

use nsh_engine::package::PublicKey;
use pig_protocol::{InstalledPluginMetadata, PluginManifest, PluginPackageSource, PluginSource};

pub struct VerifiedPlugin {
    pub backend: Option<nsh_engine::Bundle>,
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
    let backend = manifest
        .backend
        .as_ref()
        .map(|_| {
            let version = std::fs::read_to_string(
                package
                    .assets
                    .join(pig_protocol::PLUGIN_BACKEND_VERSION_FILE),
            )
            .map_err(|error| format!("read plugin backend bytecode version: {error}"))?;
            if version.trim() != nsh_engine::BYTECODE_VERSION {
                return Err("plugin backend requires a different NSH bytecode version".into());
            }
            let bytecode = std::fs::read(
                package
                    .assets
                    .join(pig_protocol::PLUGIN_BACKEND_BUNDLE_FILE),
            )
            .map_err(|error| format!("read signed plugin backend: {error}"))?;
            // SAFETY: this bytecode came from the verified signed package and matches
            // the compiler used by the currently linked NSH runtime.
            let mut bundle = unsafe { nsh_engine::Bundle::bytecode(bytecode) }
                .with_policy(package.bundle.policy.clone())
                .with_app_id(package.bundle.app_id.as_deref());
            bundle.package = package.bundle.package.clone();
            bundle.system_paths = package.bundle.system_paths.clone();
            Ok::<nsh_engine::Bundle, String>(bundle)
        })
        .transpose()?;
    Ok(VerifiedPlugin {
        backend,
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
        let backend =
            nsh_testing::compile_tsx("examples/moon-plugin/backend.ts", &[nsh_base::registry()])
                .expect("compile moon backend");
        let backend_bytecode = match backend.code {
            nsh_engine::BundleCode::Bytecode(bytes) => bytes,
            nsh_engine::BundleCode::Source(_) => panic!("backend bytecode is required"),
        };
        let manifest_source = std::fs::read_to_string("examples/moon-plugin/pig-plugin.json")
            .expect("read moon manifest");
        let manifest: PluginManifest =
            serde_json::from_str(&manifest_source).expect("valid moon manifest");
        let contents = nsh_engine::package::Contents {
            app: nsh_engine::package::App {
                id: manifest.id.clone(),
                name: "Moon Agent".into(),
                version: manifest.version.clone(),
                build: manifest.build,
            },
            capabilities: Some(serde_json::json!({
                "host": ["plugin.protocol"]
            })),
            source: String::new(),
            bytecode: None,
            source_map: None,
            assets: vec![
                (
                    pig_protocol::PLUGIN_BACKEND_BUNDLE_FILE.into(),
                    backend_bytecode,
                ),
                (
                    pig_protocol::PLUGIN_BACKEND_VERSION_FILE.into(),
                    nsh_engine::BYTECODE_VERSION.as_bytes().to_vec(),
                ),
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
        assert_eq!(loaded.manifest.contributions.tools.len(), 1);
        assert!(loaded.backend.is_some());
        let mut registry = pig_core::plugins::PluginRegistry::new(directory.join("registry"));
        let inspected = registry.sync(vec![PluginPackageSource {
            root: loaded.assets.clone(),
            source: PluginSource::User,
        }]);
        assert!(inspected.errors.is_empty(), "{:?}", inspected.errors);
        assert_eq!(inspected.packages.len(), 1);
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
        let mut workers = Vec::new();
        for _ in 0..4 {
            let package = installed_package_path(&loaded.assets);
            let root = root_path.clone();
            let data = directory.join("data");
            workers.push(std::thread::spawn(move || {
                for _ in 0..3 {
                    load_verified(&package, &root, &data)?;
                }
                Ok::<(), String>(())
            }));
        }
        for worker in workers {
            worker
                .join()
                .expect("verifier thread")
                .expect("verified package");
        }
        let wrong = nsh_engine::package::SecretKey::generate().expect("wrong root");
        let wrong_path = directory.join("wrong.pub");
        std::fs::write(&wrong_path, wrong.public().to_base64()).expect("write wrong public key");
        assert!(load_verified(&package_path, &wrong_path, &directory.join("wrong-data")).is_err());
    }
}
