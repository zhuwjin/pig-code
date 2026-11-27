use std::collections::{BTreeMap, HashMap};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock, mpsc};

use pig_protocol::{CoreError, ProviderConfig, ProviderCredentialStatus};
use sha2::{Digest as _, Sha256};
use tokio::sync::oneshot;

const SERVICE: &str = "pig-code";

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CredentialId {
    Provider { provider_id: String },
    Plugin { package_id: String, key_id: String },
}

impl CredentialId {
    pub fn provider(provider_id: &str) -> Self {
        Self::Provider {
            provider_id: provider_id.to_owned(),
        }
    }

    pub fn plugin(package_id: &str, key_id: &str) -> Self {
        Self::Plugin {
            package_id: package_id.to_owned(),
            key_id: key_id.to_owned(),
        }
    }

    fn account(&self, profile: &str) -> String {
        match self {
            Self::Provider { provider_id } => {
                format!(
                    "profile-{profile}/provider/{}",
                    escape_component(provider_id)
                )
            }
            Self::Plugin { package_id, key_id } => format!(
                "profile-{profile}/plugin/{}/{}",
                escape_component(package_id),
                escape_component(key_id)
            ),
        }
    }
}

impl std::fmt::Display for CredentialId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Provider { provider_id } => write!(formatter, "provider:{provider_id}"),
            Self::Plugin { package_id, key_id } => {
                write!(formatter, "plugin:{package_id}:{key_id}")
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CredentialError {
    NoEntry,
    Empty,
    Invalid(String),
    Unavailable(String),
}

impl CredentialError {
    pub fn detail(&self) -> &str {
        match self {
            Self::NoEntry => "credential not found",
            Self::Empty => "credential is empty",
            Self::Invalid(detail) | Self::Unavailable(detail) => detail,
        }
    }

    pub fn into_core(self, id: &CredentialId) -> CoreError {
        CoreError::CredentialStore {
            credential: id.to_string(),
            detail: self.detail().to_owned(),
        }
    }
}

trait CredentialStore: Send + 'static {
    fn get(&mut self, id: &CredentialId) -> Result<String, CredentialError>;
    fn set(&mut self, id: &CredentialId, secret: String) -> Result<(), CredentialError>;
    fn delete(&mut self, id: &CredentialId) -> Result<(), CredentialError>;
}

struct FileCredentialStore {
    path: PathBuf,
    profile: String,
}

impl FileCredentialStore {
    fn new(data_dir: &Path, profile: String) -> Self {
        Self {
            path: data_dir.join("credentials.json"),
            profile,
        }
    }

    fn load_map(&self) -> Result<BTreeMap<String, String>, CredentialError> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(BTreeMap::new());
            }
            Err(_) => {
                return Err(CredentialError::Unavailable(
                    "failed to read local credential file".to_owned(),
                ));
            }
        };
        serde_json::from_slice(&bytes).map_err(|_| {
            CredentialError::Unavailable("local credential file is corrupted".to_owned())
        })
    }

    fn save_map(&self, map: &BTreeMap<String, String>) -> Result<(), CredentialError> {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent).map_err(|_| {
            CredentialError::Unavailable("failed to create credential directory".to_owned())
        })?;
        let tmp = parent.join(format!("credentials-{}.tmp", std::process::id()));
        let mut options = std::fs::OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp).map_err(|_| {
            CredentialError::Unavailable("failed to open local credential file".to_owned())
        })?;
        let encoded = serde_json::to_vec_pretty(map).map_err(|_| {
            CredentialError::Unavailable("failed to serialize local credentials".to_owned())
        })?;
        file.write_all(&encoded).map_err(|_| {
            CredentialError::Unavailable("failed to write local credential file".to_owned())
        })?;
        file.sync_all().map_err(|_| {
            CredentialError::Unavailable("failed to flush local credential file".to_owned())
        })?;
        drop(file);
        std::fs::rename(&tmp, &self.path).map_err(|_| {
            let _ = std::fs::remove_file(&tmp);
            CredentialError::Unavailable("failed to commit local credential file".to_owned())
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }
}

impl CredentialStore for FileCredentialStore {
    fn get(&mut self, id: &CredentialId) -> Result<String, CredentialError> {
        let map = self.load_map()?;
        let secret = map
            .get(&id.account(&self.profile))
            .cloned()
            .ok_or(CredentialError::NoEntry)?;
        if secret.trim().is_empty() {
            return Err(CredentialError::Empty);
        }
        Ok(secret)
    }

    fn set(&mut self, id: &CredentialId, secret: String) -> Result<(), CredentialError> {
        if secret.trim().is_empty() {
            return Err(CredentialError::Empty);
        }
        let mut map = self.load_map()?;
        map.insert(id.account(&self.profile), secret);
        self.save_map(&map)
    }

    fn delete(&mut self, id: &CredentialId) -> Result<(), CredentialError> {
        let mut map = self.load_map()?;
        let removed = map.remove(&id.account(&self.profile));
        if removed.is_none() {
            return Err(CredentialError::NoEntry);
        }
        self.save_map(&map)
    }
}

fn should_fallback_to_file_store(error: &keyring::Error) -> bool {
    #[cfg(target_os = "linux")]
    {
        matches!(
            error,
            keyring::Error::NoDefaultStore
                | keyring::Error::NotSupportedByStore(_)
                | keyring::Error::NoStorageAccess(_)
                | keyring::Error::PlatformFailure(_)
        )
    }
    #[cfg(not(target_os = "linux"))]
    {
        matches!(
            error,
            keyring::Error::NoDefaultStore | keyring::Error::NotSupportedByStore(_)
        )
    }
}

struct SystemCredentialStore {
    profile: String,
    fallback: FileCredentialStore,
    use_fallback: bool,
}

impl SystemCredentialStore {
    fn new(data_dir: &Path) -> Self {
        let canonical = data_dir
            .canonicalize()
            .unwrap_or_else(|_| data_dir.to_path_buf());
        let digest = Sha256::digest(canonical.to_string_lossy().as_bytes());
        let profile = digest[..4]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let fallback = FileCredentialStore::new(data_dir, profile.clone());
        Self {
            profile,
            fallback,
            use_fallback: false,
        }
    }

    fn activate_fallback(&mut self, reason: &str) {
        if !self.use_fallback {
            self.use_fallback = true;
            tracing::warn!(
                path = %self.fallback.path.display(),
                reason,
                "system keyring unavailable; falling back to mode-0600 local credential file"
            );
        }
    }

    fn entry(&self, id: &CredentialId) -> Result<keyring::Entry, keyring::Error> {
        keyring::Entry::new(SERVICE, &id.account(&self.profile))
    }
}

impl CredentialStore for SystemCredentialStore {
    fn get(&mut self, id: &CredentialId) -> Result<String, CredentialError> {
        if self.use_fallback {
            return self.fallback.get(id);
        }
        let result = self.entry(id).and_then(|entry| entry.get_password());
        match result {
            Ok(secret) => {
                if secret.trim().is_empty() {
                    return Err(CredentialError::Empty);
                }
                Ok(secret)
            }
            Err(keyring::Error::NoEntry) => Err(CredentialError::NoEntry),
            Err(error) if should_fallback_to_file_store(&error) => {
                self.activate_fallback(&safe_keyring_error(error));
                self.fallback.get(id)
            }
            Err(error) => Err(CredentialError::Unavailable(safe_keyring_error(error))),
        }
    }

    fn set(&mut self, id: &CredentialId, secret: String) -> Result<(), CredentialError> {
        if secret.trim().is_empty() {
            return Err(CredentialError::Empty);
        }
        if self.use_fallback {
            return self.fallback.set(id, secret);
        }
        let result = self.entry(id).and_then(|entry| entry.set_password(&secret));
        match result {
            Ok(()) => Ok(()),
            Err(error) if should_fallback_to_file_store(&error) => {
                self.activate_fallback(&safe_keyring_error(error));
                self.fallback.set(id, secret)
            }
            Err(error) => Err(CredentialError::Unavailable(safe_keyring_error(error))),
        }
    }

    fn delete(&mut self, id: &CredentialId) -> Result<(), CredentialError> {
        if self.use_fallback {
            return self.fallback.delete(id);
        }
        let result = self.entry(id).and_then(|entry| entry.delete_credential());
        match result {
            Ok(()) => Ok(()),
            Err(keyring::Error::NoEntry) => Err(CredentialError::NoEntry),
            Err(error) if should_fallback_to_file_store(&error) => {
                self.activate_fallback(&safe_keyring_error(error));
                self.fallback.delete(id)
            }
            Err(error) => Err(CredentialError::Unavailable(safe_keyring_error(error))),
        }
    }
}

#[derive(Default)]
struct MemoryCredentialStore {
    values: BTreeMap<CredentialId, String>,
    fallback: Option<String>,
}

impl CredentialStore for MemoryCredentialStore {
    fn get(&mut self, id: &CredentialId) -> Result<String, CredentialError> {
        self.values
            .get(id)
            .cloned()
            .or_else(|| self.fallback.clone())
            .ok_or(CredentialError::NoEntry)
    }

    fn set(&mut self, id: &CredentialId, secret: String) -> Result<(), CredentialError> {
        if secret.trim().is_empty() {
            return Err(CredentialError::Empty);
        }
        self.values.insert(id.clone(), secret);
        Ok(())
    }

    fn delete(&mut self, id: &CredentialId) -> Result<(), CredentialError> {
        self.values
            .remove(id)
            .map(|_| ())
            .ok_or(CredentialError::NoEntry)
    }
}

enum Command {
    Get(
        CredentialId,
        oneshot::Sender<Result<String, CredentialError>>,
    ),
    Set(
        CredentialId,
        String,
        oneshot::Sender<Result<String, CredentialError>>,
    ),
    Delete(CredentialId, oneshot::Sender<Result<(), CredentialError>>),
}

type CachedResult = Result<String, CredentialError>;

pub struct CredentialManager {
    commands: mpsc::Sender<Command>,
    cache: Arc<RwLock<HashMap<CredentialId, CachedResult>>>,
    fallback: Option<String>,
}

impl CredentialManager {
    pub fn system(data_dir: &Path) -> Self {
        Self::with_store(SystemCredentialStore::new(data_dir), None)
    }

    pub fn mock() -> Self {
        Self::with_store(
            MemoryCredentialStore {
                values: BTreeMap::new(),
                fallback: Some("mock-key".to_owned()),
            },
            Some("mock-key".to_owned()),
        )
    }

    pub fn empty_memory() -> Self {
        Self::with_store(MemoryCredentialStore::default(), None)
    }

    fn with_store(store: impl CredentialStore, fallback: Option<String>) -> Self {
        let (commands, receiver) = mpsc::channel::<Command>();
        std::thread::spawn(move || {
            let mut store = store;
            while let Ok(command) = receiver.recv() {
                match command {
                    Command::Get(id, reply) => {
                        let _ = reply.send(store.get(&id));
                    }
                    Command::Set(id, secret, reply) => {
                        let result = store.set(&id, secret.clone()).and_then(|()| {
                            let actual = store.get(&id)?;
                            if actual == secret {
                                Ok(actual)
                            } else {
                                Err(CredentialError::Unavailable(
                                    "the credential read-back did not match the submitted value"
                                        .to_owned(),
                                ))
                            }
                        });
                        let _ = reply.send(result);
                    }
                    Command::Delete(id, reply) => {
                        let _ = reply.send(store.delete(&id));
                    }
                }
            }
        });
        Self {
            commands,
            cache: Arc::new(RwLock::new(HashMap::new())),
            fallback,
        }
    }

    pub async fn get(&self, id: CredentialId) -> Result<String, CredentialError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Get(id.clone(), reply))
            .map_err(|_| {
                CredentialError::Unavailable("the credential worker has stopped".to_owned())
            })?;
        let result = response.await.map_err(|_| {
            CredentialError::Unavailable("the credential worker stopped responding".to_owned())
        })?;
        self.cache_result(id, result.clone());
        result
    }

    pub async fn set(&self, id: CredentialId, secret: String) -> Result<String, CredentialError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Set(id.clone(), secret, reply))
            .map_err(|_| {
                CredentialError::Unavailable("the credential worker has stopped".to_owned())
            })?;
        let result = response.await.map_err(|_| {
            CredentialError::Unavailable("the credential worker stopped responding".to_owned())
        })?;
        self.cache_result(id, result.clone());
        result
    }

    pub async fn delete(&self, id: CredentialId) -> Result<(), CredentialError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Delete(id.clone(), reply))
            .map_err(|_| {
                CredentialError::Unavailable("the credential worker has stopped".to_owned())
            })?;
        let result = response.await.map_err(|_| {
            CredentialError::Unavailable("the credential worker stopped responding".to_owned())
        })?;
        let cached = match &result {
            Ok(()) | Err(CredentialError::NoEntry) => Err(CredentialError::NoEntry),
            Err(error) => Err(error.clone()),
        };
        self.cache_result(id, cached);
        match result {
            Ok(()) | Err(CredentialError::NoEntry) => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn cache_result(&self, id: CredentialId, result: CachedResult) {
        self.cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, result);
    }

    pub fn cached_provider(&self, provider: &ProviderConfig) -> Result<String, CredentialError> {
        if !provider.requires_api_key {
            return Ok(String::new());
        }
        self.cached_credential(&CredentialId::provider(&provider.id))
    }

    /// Read one credential from the in-process cache (a plugin-owned
    /// credential for a contributed provider, or a provider key).
    pub fn cached_credential(&self, id: &CredentialId) -> Result<String, CredentialError> {
        match self.cached(id) {
            Some(result) => result,
            None => Err(CredentialError::NoEntry),
        }
    }

    pub fn provider_statuses(
        &self,
        providers: &[ProviderConfig],
    ) -> BTreeMap<String, ProviderCredentialStatus> {
        providers
            .iter()
            .map(|provider| {
                let status = if !provider.requires_api_key {
                    ProviderCredentialStatus::NotRequired
                } else {
                    match self.cached(&CredentialId::provider(&provider.id)) {
                        Some(Ok(_)) => ProviderCredentialStatus::Configured,
                        Some(Err(CredentialError::NoEntry)) | None => {
                            ProviderCredentialStatus::Missing
                        }
                        Some(Err(_)) => ProviderCredentialStatus::Unavailable,
                    }
                };
                (provider.id.clone(), status)
            })
            .collect()
    }

    pub fn is_configured(&self, id: &CredentialId) -> bool {
        matches!(self.cached(id), Some(Ok(_)))
    }

    pub async fn refresh_providers(&self, providers: &[ProviderConfig]) {
        for provider in providers
            .iter()
            .filter(|provider| provider.requires_api_key)
        {
            let _ = self.get(CredentialId::provider(&provider.id)).await;
        }
    }

    fn cached(&self, id: &CredentialId) -> Option<CachedResult> {
        self.cache
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .cloned()
            .or_else(|| self.fallback.clone().map(Ok))
    }
}

fn escape_component(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_') {
            escaped.push(char::from(byte));
        } else {
            escaped.push_str(&format!("%{byte:02X}"));
        }
    }
    escaped
}

fn safe_keyring_error(error: keyring::Error) -> String {
    match error {
        keyring::Error::NoEntry => "credential not found".to_owned(),
        keyring::Error::BadEncoding(_) | keyring::Error::BadDataFormat(_, _) => {
            "the stored credential is not valid UTF-8".to_owned()
        }
        keyring::Error::NoStorageAccess(_) => {
            "the system credential store is locked or unavailable".to_owned()
        }
        keyring::Error::NoDefaultStore | keyring::Error::NotSupportedByStore(_) => {
            "the system credential store is unavailable on this platform".to_owned()
        }
        keyring::Error::TooLong(name, limit) => {
            format!("credential attribute `{name}` exceeds the {limit}-byte limit")
        }
        keyring::Error::Invalid(name, detail) => {
            format!("invalid credential parameter `{name}`: {detail}")
        }
        keyring::Error::Ambiguous(_) => "multiple credentials match this entry".to_owned(),
        keyring::Error::BadStoreFormat(_) => {
            "the system credential store format is invalid".to_owned()
        }
        keyring::Error::PlatformFailure(_) => {
            "the system credential store operation failed".to_owned()
        }
        _ => "the system credential store operation failed".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn memory_store_supports_set_get_delete_and_status() {
        let manager = CredentialManager::empty_memory();
        let id = CredentialId::provider("deepseek");
        assert_eq!(
            manager.provider_statuses(&[ProviderConfig {
                id: "deepseek".into(),
                name: "DeepSeek".into(),
                base_url: "https://api.deepseek.com".into(),
                requires_api_key: true,
                api_format: pig_protocol::ApiFormat::OpenAiChat,
                enabled: true,
                models: vec![],
                icon: None,
                key_url: None,
            }])["deepseek"],
            ProviderCredentialStatus::Missing
        );
        manager.set(id.clone(), "canary".into()).await.expect("set");
        assert_eq!(manager.get(id.clone()).await.expect("get"), "canary");
        assert!(manager.is_configured(&id));
        manager.delete(id.clone()).await.expect("delete");
        assert!(!manager.is_configured(&id));
        manager.delete(id).await.expect("delete is idempotent");
    }

    #[test]
    fn accounts_escape_ids_and_are_profile_scoped() {
        let id = CredentialId::plugin("pig.websearch", "tavily/api");
        assert_eq!(
            id.account("abc123"),
            "profile-abc123/plugin/pig%2Ewebsearch/tavily%2Fapi"
        );
        assert_ne!(id.account("one"), id.account("two"));
    }

    #[test]
    fn keyring_errors_never_include_raw_bad_encoding_bytes() {
        let detail = safe_keyring_error(keyring::Error::BadEncoding(b"canary".to_vec()));
        assert!(!detail.contains("canary"));
    }

    #[test]
    fn file_credential_store_persists_reads_and_deletes_secrets() {
        let dir = std::env::temp_dir().join(format!(
            "pig-cred-file-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let mut store = FileCredentialStore::new(&dir, "abcd1234".to_owned());
        let id = CredentialId::provider("deepseek");
        assert_eq!(store.get(&id), Err(CredentialError::NoEntry));
        store.set(&id, "sk-test-123".to_owned()).expect("set");
        assert_eq!(store.get(&id).expect("get"), "sk-test-123");

        let mut reloaded = FileCredentialStore::new(&dir, "abcd1234".to_owned());
        assert_eq!(reloaded.get(&id).expect("reloaded get"), "sk-test-123");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(dir.join("credentials.json"))
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }

        reloaded.delete(&id).expect("delete");
        assert_eq!(reloaded.get(&id), Err(CredentialError::NoEntry));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
