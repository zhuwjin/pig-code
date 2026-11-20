use std::path::{Path, PathBuf};

use nsh_engine::package::PublicKey;
use pig_core::plugins::DEFAULT_MARKETPLACE_INDEX_URL;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

const OFFICIAL_MARKETPLACE_FINGERPRINT: &str =
    "sha256:488ba14b57449a35989a3a1d27a8fdf7d9596d6e9c427b46c9669639104df9c3";
const SOURCES_FILE: &str = "plugin-marketplaces.json";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarketplaceSource {
    pub id: String,
    pub name: String,
    pub index_url: String,
    pub official: bool,
    pub fingerprint: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarketplacePreview {
    pub id: String,
    pub name: String,
    pub index_url: String,
    pub official: bool,
    pub fingerprint: String,
    pub public_key: String,
}

fn official_source() -> MarketplaceSource {
    MarketplaceSource {
        id: "official".into(),
        name: "pig-code-plugins".into(),
        index_url: DEFAULT_MARKETPLACE_INDEX_URL.into(),
        official: true,
        fingerprint: OFFICIAL_MARKETPLACE_FINGERPRINT.into(),
    }
}

fn custom_id(index_url: &str) -> String {
    format!("custom-{:x}", Sha256::digest(index_url.as_bytes()))
}

fn source_name(index_url: &str) -> String {
    let base = index_url
        .trim()
        .trim_end_matches('/')
        .strip_suffix("index.json")
        .unwrap_or(index_url.trim())
        .trim_end_matches('/');
    let parts: Vec<_> = base.split('/').filter(|part| !part.is_empty()).collect();
    let last = parts.last().copied().unwrap_or("marketplace");
    if matches!(last, "main" | "master") && parts.len() > 1 {
        parts[parts.len() - 2].to_owned()
    } else {
        last.to_owned()
    }
}

fn public_key_fingerprint(public_key: &str) -> Result<(String, String), String> {
    let key = PublicKey::from_base64(public_key.trim()).map_err(|error| error.to_string())?;
    let canonical = key.to_base64();
    let fingerprint = format!("sha256:{:x}", Sha256::digest(canonical.as_bytes()));
    Ok((canonical, fingerprint))
}

fn sources_path(data_dir: &Path) -> PathBuf {
    data_dir.join(SOURCES_FILE)
}

fn root_path(data_dir: &Path, id: &str) -> PathBuf {
    data_dir
        .join("plugin-marketplaces")
        .join(id)
        .join("root.pub")
}

fn read_custom_sources(data_dir: &Path) -> Result<Vec<MarketplaceSource>, String> {
    let path = sources_path(data_dir);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(format!(
                "read plugin marketplaces {}: {error}",
                path.display()
            ));
        }
    };
    let sources: Vec<MarketplaceSource> = serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid plugin marketplaces {}: {error}", path.display()))?;
    if sources.iter().any(|source| {
        source.official
            || source.id != custom_id(&source.index_url)
            || source.index_url == DEFAULT_MARKETPLACE_INDEX_URL
    }) {
        return Err("invalid persisted plugin marketplace identity".into());
    }
    Ok(sources)
}

fn write_custom_sources(data_dir: &Path, sources: &[MarketplaceSource]) -> Result<(), String> {
    let path = sources_path(data_dir);
    let bytes = serde_json::to_vec_pretty(sources).map_err(|error| error.to_string())?;
    pig_core::config::write_settings_file(&path, &bytes)
        .map_err(|error| format!("write plugin marketplaces {}: {error}", path.display()))
}

pub fn sources(data_dir: &Path) -> Result<Vec<MarketplaceSource>, String> {
    let mut custom = read_custom_sources(data_dir)?;
    custom.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
    Ok(std::iter::once(official_source()).chain(custom).collect())
}

pub async fn preview(index_url: String) -> Result<MarketplacePreview, String> {
    let index_url = index_url.trim().to_owned();
    pig_core::plugins::fetch_catalog(&index_url)
        .await
        .map_err(|error| format!("{error:?}"))?;
    let public_key = pig_core::plugins::fetch_root_public_key(&index_url)
        .await
        .map_err(|error| format!("{error:?}"))?;
    let (public_key, fingerprint) = public_key_fingerprint(&public_key)?;
    let official = index_url == DEFAULT_MARKETPLACE_INDEX_URL;
    if official && fingerprint != OFFICIAL_MARKETPLACE_FINGERPRINT {
        return Err("official marketplace root key fingerprint mismatch".into());
    }
    Ok(MarketplacePreview {
        id: if official {
            "official".into()
        } else {
            custom_id(&index_url)
        },
        name: if official {
            official_source().name
        } else {
            source_name(&index_url)
        },
        index_url,
        official,
        fingerprint,
        public_key,
    })
}

pub fn add(data_dir: &Path, preview: MarketplacePreview) -> Result<Vec<MarketplaceSource>, String> {
    if preview.official || preview.index_url == DEFAULT_MARKETPLACE_INDEX_URL {
        return Err("the official marketplace is already built in".into());
    }
    let expected_id = custom_id(&preview.index_url);
    if preview.id != expected_id {
        return Err("plugin marketplace preview identity mismatch".into());
    }
    let (public_key, fingerprint) = public_key_fingerprint(&preview.public_key)?;
    if fingerprint != preview.fingerprint {
        return Err("plugin marketplace root key fingerprint mismatch".into());
    }
    let mut custom = read_custom_sources(data_dir)?;
    if let Some(existing) = custom.iter().find(|source| source.id == expected_id) {
        if existing.fingerprint != fingerprint {
            return Err(
                "marketplace root key changed; remove and re-add it to trust a new key".into(),
            );
        }
        return sources(data_dir);
    }
    let source = MarketplaceSource {
        id: expected_id,
        name: source_name(&preview.index_url),
        index_url: preview.index_url,
        official: false,
        fingerprint,
    };
    let root = root_path(data_dir, &source.id);
    pig_core::config::write_settings_file(&root, format!("{public_key}\n").as_bytes())
        .map_err(|error| format!("store marketplace root public key: {error}"))?;
    custom.push(source);
    write_custom_sources(data_dir, &custom)?;
    sources(data_dir)
}

pub fn remove(data_dir: &Path, id: &str) -> Result<Vec<MarketplaceSource>, String> {
    if id == "official" {
        return Err("the built-in official marketplace cannot be removed".into());
    }
    let mut custom = read_custom_sources(data_dir)?;
    let previous = custom.len();
    custom.retain(|source| source.id != id);
    if custom.len() == previous {
        return Err("plugin marketplace was not found".into());
    }
    write_custom_sources(data_dir, &custom)?;
    let root = root_path(data_dir, id);
    if root.exists() {
        std::fs::remove_file(&root)
            .map_err(|error| format!("remove marketplace root public key: {error}"))?;
    }
    sources(data_dir)
}

pub fn trusted_root(data_dir: &Path, index_url: &str) -> Result<PathBuf, String> {
    let source = sources(data_dir)?
        .into_iter()
        .find(|source| source.index_url == index_url && !source.official)
        .ok_or_else(|| {
            "marketplace is not trusted; add and confirm its root key first".to_owned()
        })?;
    let path = root_path(data_dir, &source.id);
    let public_key = std::fs::read_to_string(&path)
        .map_err(|error| format!("read marketplace root public key: {error}"))?;
    let (_, fingerprint) = public_key_fingerprint(&public_key)?;
    if fingerprint != source.fingerprint {
        return Err("saved marketplace root key fingerprint mismatch".into());
    }
    Ok(path)
}

pub async fn official_root(data_dir: &Path) -> Result<PathBuf, String> {
    let public_key = pig_core::plugins::fetch_root_public_key(DEFAULT_MARKETPLACE_INDEX_URL)
        .await
        .map_err(|error| format!("{error:?}"))?;
    let (public_key, fingerprint) = public_key_fingerprint(&public_key)?;
    if fingerprint != OFFICIAL_MARKETPLACE_FINGERPRINT {
        return Err("official marketplace root key fingerprint mismatch".into());
    }
    let path = root_path(data_dir, "official");
    pig_core::config::write_settings_file(&path, format!("{public_key}\n").as_bytes())
        .map_err(|error| format!("store official marketplace root public key: {error}"))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "pig-marketplace-sources-{}-{sequence}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create marketplace test directory");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn preview(index_url: &str) -> MarketplacePreview {
        let key = nsh_engine::package::SecretKey::generate()
            .expect("generate test key")
            .public()
            .to_base64();
        let (_, fingerprint) = public_key_fingerprint(&key).expect("fingerprint");
        MarketplacePreview {
            id: custom_id(index_url),
            name: source_name(index_url),
            index_url: index_url.into(),
            official: false,
            fingerprint,
            public_key: key,
        }
    }

    #[test]
    fn custom_sources_persist_the_confirmed_key_and_refuse_silent_rotation() {
        let temp = TempDir::new();
        let url = "https://example.com/plugins/index.json";
        let first = preview(url);
        let added = add(&temp.0, first.clone()).expect("add marketplace");
        assert_eq!(added.len(), 2);
        assert!(!added[1].official);
        assert_eq!(added[1].fingerprint, first.fingerprint);
        assert_eq!(
            trusted_root(&temp.0, url).expect("trusted root"),
            root_path(&temp.0, &first.id)
        );
        add(&temp.0, first).expect("idempotent add");
        let rotated = preview(url);
        let error = add(&temp.0, rotated).expect_err("rotation must be explicit");
        assert!(error.contains("remove and re-add"));
        assert!(remove(&temp.0, "official").is_err());
        let remaining = remove(&temp.0, &custom_id(url)).expect("remove custom marketplace");
        assert_eq!(remaining, vec![official_source()]);
        assert!(trusted_root(&temp.0, url).is_err());
    }

    #[test]
    fn saved_root_tampering_is_detected_before_install() {
        let temp = TempDir::new();
        let url = "https://example.com/plugins/index.json";
        let first = preview(url);
        add(&temp.0, first.clone()).expect("add marketplace");
        let other = preview("https://other.example/index.json");
        std::fs::write(root_path(&temp.0, &first.id), other.public_key).expect("replace test root");
        let error = trusted_root(&temp.0, url).expect_err("tampering must fail");
        assert!(error.contains("fingerprint mismatch"));
    }

    #[test]
    fn marketplace_names_cover_raw_github_and_ordinary_urls() {
        assert_eq!(
            source_name("https://raw.githubusercontent.com/owner/plugins/main/index.json"),
            "plugins"
        );
        assert_eq!(source_name("https://example.com/index.json"), "example.com");
    }
}
