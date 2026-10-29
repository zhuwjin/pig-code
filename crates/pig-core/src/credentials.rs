use std::collections::{BTreeMap, HashMap};
use std::path::Path;
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

struct SystemCredentialStore {
    profile: String,
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
        Self { profile }
    }

    fn entry(&self, id: &CredentialId) -> Result<keyring::Entry, CredentialError> {
        keyring::Entry::new(SERVICE, &id.account(&self.profile))
            .map_err(|error| CredentialError::Unavailable(safe_keyring_error(error)))
    }
}

impl CredentialStore for SystemCredentialStore {
    fn get(&mut self, id: &CredentialId) -> Result<String, CredentialError> {
        let secret = self
            .entry(id)?
            .get_password()
            .map_err(|error| match error {
                keyring::Error::NoEntry => CredentialError::NoEntry,
                other => CredentialError::Unavailable(safe_keyring_error(other)),
            })?;
        if secret.trim().is_empty() {
            return Err(CredentialError::Empty);
        }
        Ok(secret)
    }

    fn set(&mut self, id: &CredentialId, secret: String) -> Result<(), CredentialError> {
        if secret.trim().is_empty() {
            return Err(CredentialError::Empty);
        }
        self.entry(id)?
            .set_password(&secret)
            .map_err(|error| CredentialError::Unavailable(safe_keyring_error(error)))
    }

    fn delete(&mut self, id: &CredentialId) -> Result<(), CredentialError> {
        self.entry(id)?
            .delete_credential()
            .map_err(|error| match error {
                keyring::Error::NoEntry => CredentialError::NoEntry,
                other => CredentialError::Unavailable(safe_keyring_error(other)),
            })
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
        match self.cached(&CredentialId::provider(&provider.id)) {
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
}
