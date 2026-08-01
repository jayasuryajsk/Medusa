//! Private provider credential storage and deterministic credential lookup.

use std::{collections::BTreeMap, env, fs, path::PathBuf};

use color_eyre::eyre::{Result, WrapErr, bail};
use serde::{Deserialize, Serialize};

use crate::{model::provider::ProviderDefinition, persistence::atomic_write_private};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CredentialFile {
    #[serde(default)]
    providers: BTreeMap<String, StoredCredential>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StoredCredential {
    ApiKey { key: String },
}

#[derive(Debug, Clone)]
pub struct CredentialStore {
    path: PathBuf,
}

impl CredentialStore {
    pub fn default_path() -> PathBuf {
        env::var_os("MEDUSA_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("XDG_DATA_HOME").map(|path| PathBuf::from(path).join("medusa")))
            .or_else(|| {
                env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share/medusa"))
            })
            .unwrap_or_else(|| PathBuf::from(".medusa-data"))
            .join("auth.json")
    }

    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn load() -> Self {
        Self::new(Self::default_path())
    }

    /// Resolve credentials from the private Medusa store first, then explicit
    /// environment variables. This matches OpenCode's stored-auth precedence.
    pub fn api_key(&self, provider: &ProviderDefinition) -> Option<String> {
        if let Some(key) = self
            .read_file()
            .ok()
            .and_then(|file| file.providers.get(&provider.id).cloned())
            .map(|credential| match credential {
                StoredCredential::ApiKey { key } => key,
            })
            .filter(|key| !key.trim().is_empty())
        {
            return Some(key);
        }
        for key in &provider.api_key_env {
            if let Some(value) = env_or_launchctl(key) {
                return Some(value);
            }
        }
        None
    }

    pub fn store_api_key(&self, provider: &str, key: &str) -> Result<()> {
        let provider = provider.trim().to_ascii_lowercase();
        let key = key.trim();
        if provider.is_empty() {
            bail!("credential provider cannot be empty");
        }
        if key.is_empty() {
            bail!("API key cannot be empty");
        }

        // A malformed store should be repaired deliberately. Silently
        // replacing it here could destroy credentials for other providers.
        let mut file = self.read_file()?;
        file.providers.insert(
            provider,
            StoredCredential::ApiKey {
                key: key.to_string(),
            },
        );
        let encoded =
            serde_json::to_string_pretty(&file).wrap_err("failed to encode credentials")?;
        atomic_write_private(&self.path, encoded)
            .wrap_err_with(|| format!("failed to write credentials {}", self.path.display()))
    }

    pub fn remove(&self, provider: &str) -> Result<bool> {
        let mut file = self.read_file()?;
        let removed = file
            .providers
            .remove(&provider.trim().to_ascii_lowercase())
            .is_some();
        if removed {
            let encoded =
                serde_json::to_string_pretty(&file).wrap_err("failed to encode credentials")?;
            atomic_write_private(&self.path, encoded)
                .wrap_err_with(|| format!("failed to write credentials {}", self.path.display()))?;
        }
        Ok(removed)
    }

    pub fn has_stored_key(&self, provider: &str) -> bool {
        let provider = provider.trim().to_ascii_lowercase();
        self.read_file()
            .ok()
            .is_some_and(|file| file.providers.contains_key(&provider))
    }

    fn read_file(&self) -> Result<CredentialFile> {
        if !self.path.exists() {
            return Ok(CredentialFile::default());
        }
        let raw = fs::read_to_string(&self.path)
            .wrap_err_with(|| format!("failed to read credentials {}", self.path.display()))?;
        serde_json::from_str(&raw)
            .wrap_err_with(|| format!("failed to parse credentials {}", self.path.display()))
    }
}

fn env_or_launchctl(key: &str) -> Option<String> {
    if let Ok(value) = env::var(key)
        && !value.trim().is_empty()
    {
        return Some(value);
    }

    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("launchctl")
            .arg("getenv")
            .arg(key)
            .output()
            .ok()?;
        if output.status.success() {
            let value = String::from_utf8(output.stdout).ok()?.trim().to_string();
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::provider::ProviderRegistry;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn path() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        env::temp_dir()
            .join(format!("medusa-auth-{}-{nonce}", std::process::id()))
            .join("auth.json")
    }

    #[test]
    fn stores_resolves_and_removes_private_provider_keys() {
        let path = path();
        let store = CredentialStore::new(path.clone());
        let registry = ProviderRegistry::builtins();
        let provider = registry.provider("openrouter").unwrap();

        store
            .store_api_key("openrouter", "secret-test-key")
            .unwrap();
        assert_eq!(store.api_key(provider).as_deref(), Some("secret-test-key"));
        assert!(store.has_stored_key("openrouter"));
        assert!(store.remove("openrouter").unwrap());
        assert!(store.api_key(provider).is_none());
        fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn refuses_to_overwrite_a_malformed_credential_store() {
        let path = path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "not json").unwrap();
        let store = CredentialStore::new(path.clone());

        let error = store
            .store_api_key("openrouter", "new-key")
            .unwrap_err()
            .to_string();

        assert!(error.contains("failed to parse credentials"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "not json");
        fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
