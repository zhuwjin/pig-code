use std::path::{Path, PathBuf};

use pig_protocol::{InstalledPluginMetadata, PluginManifest, PluginPackageSource, PluginSource};

use crate::package::{PublicKey, TrustStore, load_package};

pub struct VerifiedPlugin {
    pub backend: Option<String>,
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
    let root = PublicKey::from_base64(root.trim())?;
    let mut trust = TrustStore::open(data_dir.join("trust"))?;
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
    let package = load_package(&trust, &bytes, &data_dir.join("packages"))?;
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
    pig_core::config::write_settings_file(&installed_package_path(&package.assets), &bytes)
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
        .map(|declared| {
            let bundled = package.assets.join(pig_protocol::PLUGIN_BACKEND_JS_FILE);
            if bundled.is_file() {
                return std::fs::read_to_string(&bundled)
                    .map_err(|error| format!("read signed plugin backend JS: {error}"));
            }
            let direct = package.assets.join(declared);
            if direct.is_file() && declared.ends_with(".js") {
                return std::fs::read_to_string(&direct)
                    .map_err(|error| format!("read plugin backend JS {declared}: {error}"));
            }
            Err(format!(
                "signed plugin backend JS ({}) is missing",
                pig_protocol::PLUGIN_BACKEND_JS_FILE
            ))
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
                && installed_package_path(&assets).is_file()
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
    let pigpkg = assets.join(".pig-package.pigpkg");
    if pigpkg.is_file() {
        return pigpkg;
    }
    let legacy = assets.join(".pig-package.nshpkg");
    if legacy.is_file() {
        return legacy;
    }
    pigpkg
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::{PackageApp, PackageContents, SecretKey, build, certify};

    #[test]
    fn signed_package_loads_concurrently_and_wrong_root_is_refused() {
        let root_dir = std::env::temp_dir().join(format!(
            "pig-plugin-host-loader-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root_dir).expect("create fixture dir");
        let root = SecretKey::generate().expect("root key");
        let release = SecretKey::generate().expect("release key");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_secs();
        let certificate = certify(
            &root,
            &release.public(),
            &["com.pigcode.plugins.*".into()],
            now.saturating_sub(60),
            now + 3600,
        );
        let manifest_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/moon-plugin/pig-plugin.json");
        let manifest_bytes = std::fs::read(&manifest_path).expect("read moon manifest");
        let manifest: PluginManifest =
            serde_json::from_slice(&manifest_bytes).expect("parse moon manifest");
        let contents = PackageContents {
            app: PackageApp {
                id: manifest.id.clone(),
                name: "Moon Agent".into(),
                version: manifest.version.clone(),
                build: manifest.build,
            },
            assets: vec![
                (
                    pig_protocol::PLUGIN_BACKEND_JS_FILE.into(),
                    b"__pigHost.on('plugin.protocol', () => {});".to_vec(),
                ),
                (pig_protocol::PLUGIN_MANIFEST_FILE.into(), manifest_bytes),
            ],
        };
        let bytes = build(&contents, &release, &certificate).expect("build signed package");
        let package_path = root_dir.join("moon.pigpkg");
        let root_path = root_dir.join("root.pub");
        std::fs::write(&package_path, bytes).expect("write package");
        std::fs::write(&root_path, root.public().to_base64()).expect("write root pub");

        let loaded = load_verified(&package_path, &root_path, &root_dir.join("data"))
            .expect("load signed package");
        assert_eq!(loaded.manifest.id, "com.pigcode.plugins.moon");
        assert!(loaded.backend.is_some());

        let mut workers = Vec::new();
        for _ in 0..4 {
            let package = installed_package_path(&loaded.assets);
            let root = root_path.clone();
            let data = root_dir.join("data");
            workers.push(std::thread::spawn(move || {
                for _ in 0..3 {
                    load_verified(&package, &root, &data)?;
                }
                Ok::<(), String>(())
            }));
        }
        for worker in workers {
            worker.join().unwrap().unwrap();
        }

        let wrong = SecretKey::generate().expect("wrong root");
        let wrong_path = root_dir.join("wrong.pub");
        std::fs::write(&wrong_path, wrong.public().to_base64()).expect("write wrong pub");
        assert!(load_verified(&package_path, &wrong_path, &root_dir.join("wrong")).is_err());
        let _ = std::fs::remove_dir_all(root_dir);
    }
}
