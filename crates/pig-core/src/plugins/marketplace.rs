use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use pig_protocol::{
    CoreError, PLUGIN_MARKETPLACE_API_VERSION, PluginMarketplaceCatalog, PluginMarketplaceFile,
    PluginMarketplacePackage, SignedPluginDownload,
};
use sha2::{Digest as _, Sha256};

pub const DEFAULT_MARKETPLACE_INDEX_URL: &str =
    "https://raw.githubusercontent.com/zhuwjin/pig-code-plugins/main/index.json";

const MAX_CATALOG_BYTES: usize = 1024 * 1024;
const MAX_PACKAGE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_PACKAGE_FILES: usize = 10_000;
static NEXT_INSTALL: AtomicU64 = AtomicU64::new(1);

pub async fn fetch_catalog(index_url: &str) -> Result<PluginMarketplaceCatalog, CoreError> {
    let client = client()?;
    let bytes = download(&client, index_url, MAX_CATALOG_BYTES).await?;
    let catalog: PluginMarketplaceCatalog = serde_json::from_slice(&bytes)
        .map_err(|error| transport(format!("invalid marketplace index: {error}")))?;
    validate_catalog(&catalog)?;
    Ok(catalog)
}

pub async fn fetch_root_public_key(index_url: &str) -> Result<String, CoreError> {
    let client = client()?;
    let url = file_url(index_url, "root.pub")?;
    let bytes = download(&client, &url, 4096).await?;
    String::from_utf8(bytes)
        .map_err(|error| manifest(format!("invalid marketplace root key: {error}")))
}

fn validate_catalog(catalog: &PluginMarketplaceCatalog) -> Result<(), CoreError> {
    if catalog.api_version != PLUGIN_MARKETPLACE_API_VERSION {
        return Err(manifest(format!(
            "unsupported marketplace API {}; expected {PLUGIN_MARKETPLACE_API_VERSION}",
            catalog.api_version
        )));
    }
    let mut ids = BTreeSet::new();
    for package in &catalog.packages {
        validate_package(package)?;
        if !ids.insert(package.id.clone()) {
            return Err(manifest(format!(
                "duplicate marketplace package `{}`",
                package.id
            )));
        }
    }
    Ok(())
}

fn validate_package(package: &PluginMarketplacePackage) -> Result<(), CoreError> {
    super::registry::validate_id(&package.id)?;
    if package.name.trim().is_empty() || package.version.trim().is_empty() {
        return Err(manifest(
            "marketplace package name and version must not be empty",
        ));
    }
    if !valid_digest(&package.digest) {
        return Err(manifest("invalid marketplace package digest"));
    }
    if package.directory.contains('\\') {
        return Err(manifest(
            "marketplace package directories must use forward slashes",
        ));
    }
    super::registry::relative_file(&package.directory)?;
    if package.files.is_empty() || package.files.len() > MAX_PACKAGE_FILES {
        return Err(manifest(
            "marketplace package exceeds the file count limit or has no files",
        ));
    }
    let mut paths = BTreeSet::new();
    let mut total = 0_u64;
    for file in &package.files {
        validate_file(file)?;
        total = total
            .checked_add(file.size)
            .ok_or_else(|| manifest("marketplace package size overflow"))?;
        if total > MAX_PACKAGE_BYTES {
            return Err(manifest("marketplace package exceeds the size limit"));
        }
        if !paths.insert(file.path.clone()) {
            return Err(manifest(format!(
                "duplicate marketplace file `{}`",
                file.path
            )));
        }
    }
    Ok(())
}

fn validate_file(file: &PluginMarketplaceFile) -> Result<(), CoreError> {
    if file.path.contains('\\') {
        return Err(manifest("marketplace file paths must use forward slashes"));
    }
    super::registry::relative_file(&file.path)?;
    if file.size > MAX_FILE_BYTES {
        return Err(manifest(format!(
            "marketplace file `{}` exceeds the size limit",
            file.path
        )));
    }
    if file.sha256.len() != 64
        || !file
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(manifest(format!(
            "marketplace file `{}` has an invalid SHA-256",
            file.path
        )));
    }
    Ok(())
}

fn valid_digest(digest: &str) -> bool {
    let Some(hex) = digest.strip_prefix("sha256:") else {
        return false;
    };
    hex.len() == 64
        && hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Downloads one signed package without treating it as verified plugin code yet.
pub async fn download_signed_package(
    data_dir: &Path,
    index_url: &str,
    package: &PluginMarketplacePackage,
) -> Result<SignedPluginDownload, CoreError> {
    validate_package(package)?;
    if package.files.len() != 1 || !package.files[0].path.ends_with(".nshpkg") {
        return Err(manifest(
            "a signed marketplace package must contain exactly one .nshpkg file",
        ));
    }
    let file = &package.files[0];
    let client = client()?;
    let url = file_url(index_url, &format!("{}/{}", package.directory, file.path))?;
    let bytes = download(&client, &url, MAX_PACKAGE_BYTES as usize).await?;
    if bytes.len() as u64 != file.size {
        return Err(manifest("the signed package has an unexpected size"));
    }
    if format!("{:x}", Sha256::digest(&bytes)) != file.sha256 {
        return Err(manifest("the signed package failed its SHA-256 check"));
    }
    let directory = data_dir.join("plugins/downloads");
    std::fs::create_dir_all(&directory).map_err(io)?;
    crate::config::ensure_real_directory(&directory).map_err(io)?;
    let digest = package
        .digest
        .strip_prefix("sha256:")
        .unwrap_or(&package.digest);
    let target = directory.join(format!("{}-{digest}.nshpkg", package.id));
    let unique = NEXT_INSTALL.fetch_add(1, Ordering::Relaxed);
    let staging = directory.join(format!(".download-{}-{unique}", package.id));
    crate::config::write_settings_file(&staging, &bytes).map_err(io)?;
    if let Err(error) = std::fs::rename(&staging, &target) {
        let _ = std::fs::remove_file(&staging);
        return Err(io(error));
    }
    Ok(SignedPluginDownload {
        package: package.id.clone(),
        digest: package.digest.clone(),
        package_path: target,
    })
}

fn client() -> Result<reqwest::Client, CoreError> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("pig-code-plugin-marketplace/1")
        .build()
        .map_err(|error| transport(error.to_string()))
}

async fn download(client: &reqwest::Client, url: &str, limit: usize) -> Result<Vec<u8>, CoreError> {
    let url = parse_url(url)?;
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|error| transport(error.to_string()))?
        .error_for_status()
        .map_err(|error| transport(error.to_string()))?;
    if response
        .content_length()
        .is_some_and(|size| size > limit as u64)
    {
        return Err(manifest("marketplace response exceeds the size limit"));
    }
    let mut response = response;
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| transport(error.to_string()))?
    {
        if bytes.len() + chunk.len() > limit {
            return Err(manifest("marketplace response exceeds the size limit"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn file_url(index_url: &str, path: &str) -> Result<String, CoreError> {
    let base = parse_url(index_url)?;
    base.join(path)
        .map(|url| url.to_string())
        .map_err(|error| manifest(format!("invalid marketplace file URL: {error}")))
}

fn parse_url(url: &str) -> Result<reqwest::Url, CoreError> {
    let url = reqwest::Url::parse(url).map_err(|error| manifest(error.to_string()))?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(manifest(
            "marketplace URLs must be HTTP(S) URLs without credentials",
        ));
    }
    Ok(url)
}

fn transport(detail: impl Into<String>) -> CoreError {
    CoreError::PluginTransport {
        detail: detail.into(),
    }
}

fn manifest(detail: impl Into<String>) -> CoreError {
    CoreError::PluginManifest {
        detail: detail.into(),
    }
}

fn io(error: std::io::Error) -> CoreError {
    manifest(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(std::path::PathBuf);

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

    impl TempDir {
        fn new() -> Self {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "pig-marketplace-{}-{unique}-{sequence}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create marketplace test root");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fixture(root: &Path) -> PluginMarketplacePackage {
        std::fs::create_dir_all(root).expect("create package fixture");
        let bytes = b"signed fixture";
        std::fs::write(root.join("moon.nshpkg"), bytes).expect("write signed fixture");
        PluginMarketplacePackage {
            id: "demo".into(),
            name: "Demo".into(),
            version: "1.0.0".into(),
            directory: "packages/demo".into(),
            description: Default::default(),
            digest: format!("sha256:{:x}", Sha256::digest(bytes)),
            files: vec![PluginMarketplaceFile {
                path: "moon.nshpkg".into(),
                size: bytes.len() as u64,
                sha256: format!("{:x}", Sha256::digest(bytes)),
            }],
        }
    }

    fn server(responses: Vec<(String, Vec<u8>)>) -> (String, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind marketplace mock");
        let address = listener.local_addr().expect("mock address");
        let handle = std::thread::spawn(move || {
            for (path, body) in responses {
                let (mut stream, _) = listener.accept().expect("accept download");
                let mut bytes = Vec::new();
                let mut buffer = [0_u8; 2048];
                loop {
                    let count =
                        std::io::Read::read(&mut stream, &mut buffer).expect("read request");
                    assert!(count > 0);
                    bytes.extend_from_slice(&buffer[..count]);
                    if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8_lossy(&bytes);
                assert!(request.starts_with(&format!("GET {path} ")), "{request}");
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                std::io::Write::write_all(&mut stream, response.as_bytes()).expect("write headers");
                std::io::Write::write_all(&mut stream, &body).expect("write body");
            }
        });
        (format!("http://{address}/index.json"), handle)
    }

    #[tokio::test]
    async fn fetches_the_root_key_next_to_the_catalog() {
        let (index_url, server) = server(vec![(
            "/root.pub".into(),
            b"test-root-public-key\n".to_vec(),
        )]);
        let root = fetch_root_public_key(&index_url)
            .await
            .expect("download root key");
        assert_eq!(root, "test-root-public-key\n");
        server.join().expect("marketplace mock shutdown");
    }

    #[tokio::test]
    async fn downloads_a_signed_package_without_verifying_it_in_core() {
        let temp = TempDir::new();
        let bytes = b"signed-package-fixture".to_vec();
        let package = PluginMarketplacePackage {
            id: "moon".into(),
            name: "Moon".into(),
            version: "1.0.0".into(),
            directory: "packages/moon".into(),
            description: Default::default(),
            digest: format!("sha256:{:x}", Sha256::digest(&bytes)),
            files: vec![PluginMarketplaceFile {
                path: "moon.nshpkg".into(),
                size: bytes.len() as u64,
                sha256: format!("{:x}", Sha256::digest(&bytes)),
            }],
        };
        let (index_url, server) =
            server(vec![("/packages/moon/moon.nshpkg".into(), bytes.clone())]);
        let downloaded = download_signed_package(&temp.0.join("data"), &index_url, &package)
            .await
            .expect("download signed package");
        assert_eq!(
            std::fs::read(downloaded.package_path).expect("downloaded bytes"),
            bytes
        );
        server.join().expect("marketplace mock shutdown");
    }

    #[tokio::test]
    async fn a_hash_mismatch_preserves_the_existing_package() {
        let temp = TempDir::new();
        let target = temp.0.join("data/plugins/demo");
        let mut package = fixture(&target);
        let original = std::fs::read(target.join("moon.nshpkg")).expect("original package");
        package.files[0].sha256 = "0".repeat(64);
        package.digest = format!("sha256:{}", "0".repeat(64));
        let responses = vec![("/packages/demo/moon.nshpkg".to_owned(), original.clone())];
        let (index_url, server) = server(responses);
        let error = download_signed_package(&temp.0.join("data"), &index_url, &package)
            .await
            .expect_err("the modified hash must fail");
        assert!(matches!(error, CoreError::PluginManifest { .. }));
        assert_eq!(
            std::fs::read(target.join("moon.nshpkg")).expect("preserved package"),
            original
        );
        server.join().expect("marketplace mock shutdown");
    }

    #[test]
    fn package_paths_cannot_escape_the_repository_or_package() {
        let temp = TempDir::new();
        let mut package = fixture(&temp.0.join("fixture"));
        package.files[0].path = "../outside".into();
        assert!(validate_package(&package).is_err());
        package.files[0].path = "moon.nshpkg".into();
        package.directory = "../outside".into();
        assert!(validate_package(&package).is_err());
    }
}
