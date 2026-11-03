use std::path::{Component, Path, PathBuf};

use nsh_engine::package::{Certificate, Contents, SecretKey};
use pig_protocol::{PluginManifest, PLUGIN_MANIFEST_FILE};
use serde::de::DeserializeOwned;

struct Args {
    plugin: PathBuf,
    key: PathBuf,
    cert: PathBuf,
    out: PathBuf,
}

fn usage() -> &'static str {
    "Usage: cargo run --example publish-plugin -- <plugin-directory> --key <release.secret> --cert <release.cert> --out <package.nshpkg>"
}

fn parse_args() -> Result<Args, String> {
    let mut plugin = None;
    let mut key = None;
    let mut cert = None;
    let mut out = None;
    let mut args = std::env::args().skip(1);
    if std::env::args().any(|arg| arg == "--help" || arg == "-h") {
        println!("{}", usage());
        std::process::exit(0);
    }
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--key" => key = Some(PathBuf::from(args.next().ok_or("--key needs a path")?)),
            "--cert" => cert = Some(PathBuf::from(args.next().ok_or("--cert needs a path")?)),
            "--out" | "-o" => out = Some(PathBuf::from(args.next().ok_or("--out needs a path")?)),
            _ if arg.starts_with('-') => return Err(format!("unknown option {arg}\n{}", usage())),
            _ if plugin.is_none() => plugin = Some(PathBuf::from(arg)),
            _ => return Err(format!("unexpected argument {arg}\n{}", usage())),
        }
    }
    Ok(Args {
        plugin: plugin.ok_or_else(|| usage().to_owned())?,
        key: key.ok_or_else(|| usage().to_owned())?,
        cert: cert.ok_or_else(|| usage().to_owned())?,
        out: out.ok_or_else(|| usage().to_owned())?,
    })
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T, String> {
    let bytes = std::fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|error| format!("invalid JSON {}: {error}", path.display()))
}

fn relative_path(path: &Path) -> Result<String, String> {
    if path.is_absolute() || path.components().any(|part| {
        matches!(part, Component::ParentDir | Component::RootDir | Component::Prefix(_))
    }) {
        return Err(format!("asset path must stay inside the plugin: {}", path.display()));
    }
    let value = path.to_string_lossy().replace('\\', "/");
    if value.is_empty() {
        return Err("asset path must not be empty".into());
    }
    Ok(value)
}

fn add_file(assets: &mut Vec<(String, Vec<u8>)>, root: &Path, relative: &Path) -> Result<(), String> {
    let name = relative_path(relative)?;
    let path = root.join(relative);
    let metadata = std::fs::symlink_metadata(&path)
        .map_err(|error| format!("inspect asset {}: {error}", path.display()))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(format!("asset is not a regular file: {}", path.display()));
    }
    let bytes = std::fs::read(&path).map_err(|error| format!("read asset {}: {error}", path.display()))?;
    assets.push((name, bytes));
    Ok(())
}

fn add_directory(assets: &mut Vec<(String, Vec<u8>)>, root: &Path, relative: &Path) -> Result<(), String> {
    let directory = root.join(relative);
    let mut entries = std::fs::read_dir(&directory)
        .map_err(|error| format!("read asset directory {}: {error}", directory.display()))?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("read asset directory {}: {error}", directory.display()))?;
    entries.sort();
    for path in entries {
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| format!("inspect asset {}: {error}", path.display()))?;
        if metadata.file_type().is_symlink() {
            return Err(format!("asset links are not allowed: {}", path.display()));
        }
        let child = relative.join(path.file_name().expect("directory entry name"));
        if metadata.is_dir() {
            add_directory(assets, root, &child)?;
        } else if metadata.is_file() {
            add_file(assets, root, &child)?;
        }
    }
    Ok(())
}

fn collect_assets(root: &Path, manifest: &serde_json::Value) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut assets = Vec::new();
    add_file(&mut assets, root, Path::new(PLUGIN_MANIFEST_FILE))?;
    if let Some(themes) = manifest["contributions"]["themes"].as_array() {
        for theme in themes {
            if let Some(entry) = theme["entry"].as_str() {
                add_file(&mut assets, root, Path::new(entry))?;
            }
        }
    }
    if let Some(skills) = manifest["contributions"]["skills"].as_array() {
        for skill in skills {
            if let Some(path) = skill["path"].as_str() {
                add_directory(&mut assets, root, Path::new(path))?;
            }
        }
    }
    let bundled = root.join("assets");
    if bundled.is_dir() {
        add_directory(&mut assets, root, Path::new("assets"))?;
    }
    assets.sort_by(|a, b| a.0.cmp(&b.0));
    assets.dedup_by(|a, b| a.0 == b.0);
    Ok(assets)
}

fn run() -> Result<(), String> {
    let args = parse_args()?;
    let root = args.plugin;
    let manifest_path = root.join(PLUGIN_MANIFEST_FILE);
    let manifest: PluginManifest = read_json(&manifest_path)?;
    let manifest_value: serde_json::Value = read_json(&manifest_path)?;
    let app_path = root.join("nsh.json");
    let app: serde_json::Value = read_json(&app_path)?;
    if app["id"].as_str() != Some(manifest.id.as_str())
        || app["version"].as_str() != Some(manifest.version.as_str())
        || app["build"].as_u64() != Some(manifest.build)
    {
        return Err("nsh.json and pig-plugin.json must have the same id, version and build".into());
    }
    let name = app["name"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| manifest.name.fallback());
    if name.trim().is_empty() {
        return Err("nsh.json needs a display name".into());
    }
    let bundle = nsh_testing::compile_tsx(root.join(&manifest.entry), &[nsh_base::registry()])?;
    let (source, bytecode) = match bundle.code {
        nsh_engine::BundleCode::Source(source) => (source, None),
        nsh_engine::BundleCode::Bytecode(bytes) => {
            (String::new(), Some((bytes, nsh_engine::BYTECODE_VERSION.to_owned())))
        }
    };
    let source_map = bundle.source_map.map(|map| map.to_string());
    let assets = collect_assets(&root, &manifest_value)?;
    let key = SecretKey::from_base64(
        &std::fs::read_to_string(&args.key)
            .map_err(|error| format!("read release key {}: {error}", args.key.display()))?,
    )
    .map_err(|error| error.to_string())?;
    let cert = Certificate::from_json(
        &std::fs::read_to_string(&args.cert)
            .map_err(|error| format!("read release certificate {}: {error}", args.cert.display()))?,
    )
    .map_err(|error| error.to_string())?;
    let contents = Contents {
        app: nsh_engine::package::App {
            id: manifest.id.clone(),
            name: name.to_owned(),
            version: manifest.version.clone(),
            build: manifest.build,
        },
        capabilities: app["capabilities"].as_object().map(|_| app["capabilities"].clone()),
        source,
        bytecode,
        source_map,
        assets: assets.clone(),
        min_runtime: app["minRuntime"].as_str().map(str::to_owned),
    };
    let bytes = nsh_engine::package::build(&contents, &key, &cert).map_err(|error| error.to_string())?;
    pig_core::config::write_settings_file(&args.out, &bytes)
        .map_err(|error| format!("write package {}: {error}", args.out.display()))?;
    println!(
        "built {}: {} {} (build {}), {} assets, {} bytes, release key {}",
        args.out.display(),
        manifest.id,
        manifest.version,
        manifest.build,
        assets.len(),
        bytes.len(),
        key.public().id(),
    );
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
