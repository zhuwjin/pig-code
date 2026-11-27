use std::collections::BTreeMap;
use std::io::{Cursor, Read as _, Write as _};
use std::path::{Component, Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::{Signature, Signer as _, SigningKey, Verifier as _, VerifyingKey};
use rand_core::OsRng;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

const SIGNATURE_FILE: &str = "MANIFEST.sig.json";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicKey(VerifyingKey);

impl PublicKey {
    pub fn from_base64(input: &str) -> Result<Self, String> {
        let bytes = STANDARD
            .decode(input.trim())
            .map_err(|error| format!("invalid base64 public key: {error}"))?;
        let array: [u8; 32] = bytes
            .try_into()
            .map_err(|_| "Ed25519 public key must be 32 bytes".to_string())?;
        let key = VerifyingKey::from_bytes(&array)
            .map_err(|error| format!("invalid Ed25519 public key: {error}"))?;
        Ok(Self(key))
    }

    pub fn to_base64(&self) -> String {
        STANDARD.encode(self.0.as_bytes())
    }

    pub fn verify(&self, message: &[u8], signature_b64: &str) -> Result<(), String> {
        let bytes = STANDARD
            .decode(signature_b64.trim())
            .map_err(|error| format!("invalid signature base64: {error}"))?;
        let array: [u8; 64] = bytes
            .try_into()
            .map_err(|_| "Ed25519 signature must be 64 bytes".to_string())?;
        let signature = Signature::from_bytes(&array);
        self.0
            .verify(message, &signature)
            .map_err(|_| "Ed25519 signature verification failed".to_string())
    }
}

#[derive(Clone)]
pub struct SecretKey(SigningKey);

impl SecretKey {
    pub fn generate() -> Result<Self, String> {
        Ok(Self(SigningKey::generate(&mut OsRng)))
    }

    pub fn public(&self) -> PublicKey {
        PublicKey(self.0.verifying_key())
    }

    pub fn sign_base64(&self, message: &[u8]) -> String {
        let signature = self.0.sign(message);
        STANDARD.encode(signature.to_bytes())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CertificatePayload {
    pub release_key: String,
    pub scopes: Vec<String>,
    pub not_before: u64,
    pub not_after: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Certificate {
    pub payload: CertificatePayload,
    pub signature: String,
}

pub fn certify(
    root: &SecretKey,
    release_public: &PublicKey,
    scopes: &[String],
    not_before: u64,
    not_after: u64,
) -> Certificate {
    let payload = CertificatePayload {
        release_key: release_public.to_base64(),
        scopes: scopes.to_vec(),
        not_before,
        not_after,
    };
    let encoded = serde_json::to_vec(&payload).expect("serialize certificate payload");
    let signature = root.sign_base64(&encoded);
    Certificate { payload, signature }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PackageApp {
    pub id: String,
    pub name: String,
    pub version: String,
    pub build: u64,
}

#[derive(Clone, Debug)]
pub struct PackageContents {
    pub app: PackageApp,
    pub assets: Vec<(String, Vec<u8>)>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SignedManifestPayload {
    app: PackageApp,
    certificate: Certificate,
    files: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SignedManifest {
    payload: SignedManifestPayload,
    signature: String,
}

pub struct TrustStore {
    roots: Vec<PublicKey>,
}

impl TrustStore {
    pub fn open(_dir: PathBuf) -> Result<Self, String> {
        Ok(Self { roots: Vec::new() })
    }

    pub fn add_root(&mut self, root: PublicKey) {
        if !self.roots.contains(&root) {
            self.roots.push(root);
        }
    }
}

pub struct PackageReceipt {
    pub sha256: String,
}

pub struct LoadedPackage {
    pub app: PackageApp,
    pub assets: PathBuf,
    pub receipt: PackageReceipt,
}

fn normalize_asset_path(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim().replace('\\', "/");
    if trimmed.is_empty() || trimmed == SIGNATURE_FILE {
        return Err(format!("invalid package asset path: {raw}"));
    }
    let path = Path::new(&trimmed);
    if path.is_absolute() {
        return Err(format!("package asset path must be relative: {raw}"));
    }
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            _ => return Err(format!("unsafe package asset path: {raw}")),
        }
    }
    if parts.is_empty() {
        return Err(format!("empty package asset path: {raw}"));
    }
    Ok(parts.join("/"))
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn scope_matches(scope: &str, app_id: &str) -> bool {
    if scope == "*" || scope == app_id {
        return true;
    }
    if let Some(prefix) = scope.strip_suffix('*') {
        return app_id.starts_with(prefix);
    }
    false
}

pub fn build(
    contents: &PackageContents,
    release: &SecretKey,
    certificate: &Certificate,
) -> Result<Vec<u8>, String> {
    let mut files = BTreeMap::new();
    let mut normalized_assets = Vec::with_capacity(contents.assets.len());
    for (path, bytes) in &contents.assets {
        let normalized = normalize_asset_path(path)?;
        if files.insert(normalized.clone(), sha256_hex(bytes)).is_some() {
            return Err(format!("duplicate asset in plugin package: {normalized}"));
        }
        normalized_assets.push((normalized, bytes.as_slice()));
    }
    let payload = SignedManifestPayload {
        app: contents.app.clone(),
        certificate: certificate.clone(),
        files,
    };
    let payload_bytes = serde_json::to_vec(&payload).map_err(|error| error.to_string())?;
    let signature = release.sign_base64(&payload_bytes);
    let manifest = SignedManifest { payload, signature };
    let manifest_bytes = serde_json::to_vec(&manifest).map_err(|error| error.to_string())?;

    let mut buffer = Cursor::new(Vec::new());
    {
        let mut writer = ZipWriter::new(&mut buffer);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        writer
            .start_file(SIGNATURE_FILE, options)
            .map_err(|error| error.to_string())?;
        writer
            .write_all(&manifest_bytes)
            .map_err(|error| error.to_string())?;
        for (path, bytes) in normalized_assets {
            writer
                .start_file(path, options)
                .map_err(|error| error.to_string())?;
            writer.write_all(bytes).map_err(|error| error.to_string())?;
        }
        writer.finish().map_err(|error| error.to_string())?;
    }
    Ok(buffer.into_inner())
}

pub fn load_package(
    trust: &TrustStore,
    bytes: &[u8],
    packages_dir: &Path,
) -> Result<LoadedPackage, String> {
    let digest = sha256_hex(bytes);
    let mut archive = ZipArchive::new(Cursor::new(bytes))
        .map_err(|error| format!("invalid signed plugin archive: {error}"))?;

    let manifest: SignedManifest = {
        let mut entry = archive
            .by_name(SIGNATURE_FILE)
            .map_err(|_| format!("plugin package missing {SIGNATURE_FILE}"))?;
        let mut raw = Vec::new();
        entry
            .read_to_end(&mut raw)
            .map_err(|error| format!("read {SIGNATURE_FILE}: {error}"))?;
        serde_json::from_slice(&raw)
            .map_err(|error| format!("invalid {SIGNATURE_FILE}: {error}"))?
    };

    let cert = &manifest.payload.certificate;
    let cert_bytes = serde_json::to_vec(&cert.payload).map_err(|error| error.to_string())?;
    let root_valid = trust
        .roots
        .iter()
        .any(|root| root.verify(&cert_bytes, &cert.signature).is_ok());
    if !root_valid {
        return Err("plugin certificate is not signed by a trusted root key".to_string());
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_secs();
    if now < cert.payload.not_before || now > cert.payload.not_after {
        return Err("plugin signing certificate is expired or not yet valid".to_string());
    }
    if !cert
        .payload
        .scopes
        .iter()
        .any(|scope| scope_matches(scope, &manifest.payload.app.id))
    {
        return Err(format!(
            "plugin signing certificate does not authorize {}",
            manifest.payload.app.id
        ));
    }
    let release_key = PublicKey::from_base64(&cert.payload.release_key)?;
    let payload_bytes =
        serde_json::to_vec(&manifest.payload).map_err(|error| error.to_string())?;
    release_key.verify(&payload_bytes, &manifest.signature)?;

    let mut extracted_files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| format!("read plugin archive entry: {error}"))?;
        if entry.is_dir() {
            continue;
        }
        let name = entry
            .name()
            .map_err(|error| format!("invalid archive entry name: {error}"))?
            .to_string();
        if name == SIGNATURE_FILE {
            continue;
        }
        let normalized = normalize_asset_path(&name)?;
        let expected_hash = manifest
            .payload
            .files
            .get(&normalized)
            .ok_or_else(|| format!("unsigned file in plugin package: {normalized}"))?;
        let mut data = Vec::new();
        entry
            .read_to_end(&mut data)
            .map_err(|error| format!("extract {normalized}: {error}"))?;
        if sha256_hex(&data) != *expected_hash {
            return Err(format!("file digest mismatch in plugin package: {normalized}"));
        }
        extracted_files.insert(normalized, data);
    }
    if extracted_files.len() != manifest.payload.files.len() {
        return Err("plugin package is missing one or more signed files".to_string());
    }

    let safe_id = normalize_asset_path(&manifest.payload.app.id)?;
    let target_root = packages_dir.join(&safe_id).join(&digest);
    let assets_dir = target_root.join("assets");
    if !assets_dir.is_dir() {
        let parent = target_root
            .parent()
            .ok_or_else(|| "invalid package target directory".to_string())?;
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        let staging = parent.join(format!(
            ".staging-{}-{}-{}",
            digest,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let staging_assets = staging.join("assets");
        std::fs::create_dir_all(&staging_assets).map_err(|error| error.to_string())?;
        for (rel, data) in &extracted_files {
            let dest = staging_assets.join(rel);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
            std::fs::write(&dest, data).map_err(|error| error.to_string())?;
        }
        if std::fs::rename(&staging, &target_root).is_err() {
            let _ = std::fs::remove_dir_all(&staging);
            if !assets_dir.is_dir() {
                return Err(format!(
                    "failed to finalize extracted plugin directory {}",
                    target_root.display()
                ));
            }
        }
    }

    Ok(LoadedPackage {
        app: manifest.payload.app,
        assets: assets_dir,
        receipt: PackageReceipt { sha256: digest },
    })
}
