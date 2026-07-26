use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use keyring::Entry;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::remote::{SecretToken, normalize_server_url};

const REMOTE_CONFIG_SCHEMA_VERSION: u32 = 1;
const CREDENTIAL_SERVICE: &str = "codex-session-sync";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RemoteProfile {
    pub id: Uuid,
    pub display_name: String,
    pub server_url: String,
    pub selected_namespace_id: Option<Uuid>,
    #[serde(default)]
    pub automatic_namespace_selection: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RemoteProfileSummary {
    #[serde(flatten)]
    pub profile: RemoteProfile,
    pub credential_configured: bool,
    pub insecure_http: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteConfigFile {
    schema_version: u32,
    profiles: Vec<RemoteProfile>,
}

impl Default for RemoteConfigFile {
    fn default() -> Self {
        Self {
            schema_version: REMOTE_CONFIG_SCHEMA_VERSION,
            profiles: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RemoteProfileStore {
    path: PathBuf,
}

impl RemoteProfileStore {
    pub fn new(repository_root: impl AsRef<Path>) -> Self {
        Self {
            path: repository_root
                .as_ref()
                .join("config")
                .join("remotes-v1.json"),
        }
    }

    pub fn list(&self, credentials: &dyn CredentialStore) -> Result<Vec<RemoteProfileSummary>> {
        self.load()?
            .profiles
            .into_iter()
            .map(|profile| {
                let credential_configured = credentials.has(&profile.id)?;
                let insecure_http = normalize_server_url(&profile.server_url)?.scheme() == "http";
                Ok(RemoteProfileSummary {
                    profile,
                    credential_configured,
                    insecure_http,
                })
            })
            .collect()
    }

    pub fn get(&self, remote_id: Uuid) -> Result<RemoteProfile> {
        self.load()?
            .profiles
            .into_iter()
            .find(|profile| profile.id == remote_id)
            .with_context(|| format!("remote profile {remote_id} was not found"))
    }

    pub fn upsert(
        &self,
        remote_id: Uuid,
        display_name: String,
        server_url: String,
    ) -> Result<RemoteProfile> {
        let display_name = display_name.trim();
        if display_name.is_empty() || display_name.chars().count() > 128 {
            bail!("remote display name must contain 1 to 128 characters");
        }
        if display_name.chars().any(char::is_control) {
            bail!("remote display name must not contain control characters");
        }
        let server_url = normalize_server_url(&server_url)?.to_string();
        let mut config = self.load()?;
        let now = Utc::now().to_rfc3339();
        let profile = if let Some(profile) = config
            .profiles
            .iter_mut()
            .find(|profile| profile.id == remote_id)
        {
            profile.display_name = display_name.to_string();
            profile.server_url = server_url;
            profile.updated_at = now;
            profile.clone()
        } else {
            let profile = RemoteProfile {
                id: remote_id,
                display_name: display_name.to_string(),
                server_url,
                selected_namespace_id: None,
                automatic_namespace_selection: false,
                created_at: now.clone(),
                updated_at: now,
            };
            config.profiles.push(profile.clone());
            profile
        };
        config
            .profiles
            .sort_by(|left, right| left.created_at.cmp(&right.created_at));
        self.save(&config)?;
        Ok(profile)
    }

    pub fn select_namespace(&self, remote_id: Uuid, namespace_id: Uuid) -> Result<RemoteProfile> {
        let mut config = self.load()?;
        let profile = config
            .profiles
            .iter_mut()
            .find(|profile| profile.id == remote_id)
            .with_context(|| format!("remote profile {remote_id} was not found"))?;
        profile.selected_namespace_id = Some(namespace_id);
        profile.updated_at = Utc::now().to_rfc3339();
        let profile = profile.clone();
        self.save(&config)?;
        Ok(profile)
    }

    pub fn set_automatic_namespace_selection(
        &self,
        remote_id: Uuid,
        enabled: bool,
    ) -> Result<RemoteProfile> {
        let mut config = self.load()?;
        let profile = config
            .profiles
            .iter_mut()
            .find(|profile| profile.id == remote_id)
            .with_context(|| format!("remote profile {remote_id} was not found"))?;
        profile.automatic_namespace_selection = enabled;
        profile.updated_at = Utc::now().to_rfc3339();
        let profile = profile.clone();
        self.save(&config)?;
        Ok(profile)
    }

    fn load(&self) -> Result<RemoteConfigFile> {
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(RemoteConfigFile::default());
            }
            Err(error) => return Err(error).context("failed to open remote profile configuration"),
        };
        let config: RemoteConfigFile = serde_json::from_reader(BufReader::new(file))
            .context("failed to parse remote profile configuration")?;
        if config.schema_version != REMOTE_CONFIG_SCHEMA_VERSION {
            bail!(
                "unsupported remote profile schema version {}",
                config.schema_version
            );
        }
        Ok(config)
    }

    fn save(&self, config: &RemoteConfigFile) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temporary = self
            .path
            .with_file_name(format!(".remotes-v1.write-{}.tmp", Uuid::now_v7()));
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(&serde_json::to_vec_pretty(config)?)?;
        file.sync_all()?;
        drop(file);
        if self.path.exists() {
            fs::remove_file(&self.path)?;
        }
        fs::rename(&temporary, &self.path)?;
        Ok(())
    }
}

pub trait CredentialStore: Send + Sync {
    fn set(&self, remote_id: &Uuid, token: &SecretToken) -> Result<()>;
    fn get(&self, remote_id: &Uuid) -> Result<SecretToken>;
    fn has(&self, remote_id: &Uuid) -> Result<bool>;
    #[cfg(test)]
    fn delete(&self, remote_id: &Uuid) -> Result<()>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemCredentialStore;

impl CredentialStore for SystemCredentialStore {
    fn set(&self, remote_id: &Uuid, token: &SecretToken) -> Result<()> {
        entry(remote_id)?
            .set_password(token.expose())
            .context("failed to store server token in the operating-system credential store")
    }

    fn get(&self, remote_id: &Uuid) -> Result<SecretToken> {
        let value = entry(remote_id)?
            .get_password()
            .context("server token is unavailable in the operating-system credential store")?;
        SecretToken::new(value)
    }

    fn has(&self, remote_id: &Uuid) -> Result<bool> {
        match entry(remote_id)?.get_password() {
            Ok(_) => Ok(true),
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(error) => {
                Err(error).context("failed to query the operating-system credential store")
            }
        }
    }

    #[cfg(test)]
    fn delete(&self, remote_id: &Uuid) -> Result<()> {
        match entry(remote_id)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(error).context("failed to delete the operating-system credential"),
        }
    }
}

fn entry(remote_id: &Uuid) -> Result<Entry> {
    Entry::new(CREDENTIAL_SERVICE, &format!("remote:{remote_id}"))
        .context("failed to open the operating-system credential store")
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use tempfile::tempdir;

    use super::*;

    #[derive(Default)]
    struct MemoryCredentials(Mutex<HashMap<Uuid, String>>);

    impl CredentialStore for MemoryCredentials {
        fn set(&self, remote_id: &Uuid, token: &SecretToken) -> Result<()> {
            self.0
                .lock()
                .unwrap()
                .insert(*remote_id, token.expose().to_string());
            Ok(())
        }

        fn get(&self, remote_id: &Uuid) -> Result<SecretToken> {
            SecretToken::new(
                self.0
                    .lock()
                    .unwrap()
                    .get(remote_id)
                    .context("missing token")?
                    .clone(),
            )
        }

        fn has(&self, remote_id: &Uuid) -> Result<bool> {
            Ok(self.0.lock().unwrap().contains_key(remote_id))
        }

        fn delete(&self, remote_id: &Uuid) -> Result<()> {
            self.0.lock().unwrap().remove(remote_id);
            Ok(())
        }
    }

    #[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
    struct SystemCredentialCleanup {
        remote_id: Uuid,
    }

    #[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
    impl Drop for SystemCredentialCleanup {
        fn drop(&mut self) {
            let _ = SystemCredentialStore.delete(&self.remote_id);
        }
    }

    #[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
    fn assert_native_credential_backend(entry: &Entry) {
        #[cfg(target_os = "windows")]
        assert!(
            entry
                .get_credential()
                .is::<keyring::windows::WinCredential>(),
            "keyring is not using Windows Credential Manager"
        );

        #[cfg(target_os = "macos")]
        assert!(
            entry.get_credential().is::<keyring::macos::MacCredential>(),
            "keyring is not using macOS Keychain"
        );

        #[cfg(target_os = "linux")]
        assert!(
            entry
                .get_credential()
                .is::<keyring::keyutils_persistent::KeyutilsPersistentCredential>(),
            "keyring is not using the persistent Linux credential backend"
        );
    }

    #[test]
    #[ignore = "writes an isolated entry to the operating-system credential store"]
    #[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
    fn system_credential_store_round_trip_uses_native_backend_and_cleans_up() {
        let remote_id = Uuid::now_v7();
        let cleanup = SystemCredentialCleanup { remote_id };
        let credentials = SystemCredentialStore;
        credentials.delete(&remote_id).unwrap();

        let platform_entry = entry(&remote_id).unwrap();
        assert_native_credential_backend(&platform_entry);

        let token = SecretToken::new(format!(
            "credential-smoke-{}-{}",
            Uuid::now_v7(),
            Uuid::now_v7()
        ))
        .unwrap();
        credentials.set(&remote_id, &token).unwrap();
        assert!(credentials.has(&remote_id).unwrap());

        let restored = credentials.get(&remote_id).unwrap();
        assert!(
            restored.expose() == token.expose(),
            "system credential round-trip returned a different value"
        );

        credentials.delete(&remote_id).unwrap();
        assert!(!credentials.has(&remote_id).unwrap());
        drop(cleanup);
    }

    #[test]
    fn profiles_persist_without_credentials_and_keep_namespace_selection() {
        let temp = tempdir().unwrap();
        let store = RemoteProfileStore::new(temp.path());
        let credentials = MemoryCredentials::default();
        let profile = store
            .upsert(
                Uuid::now_v7(),
                "Personal".to_string(),
                "https://example.test".to_string(),
            )
            .unwrap();
        let token = SecretToken::new("server-token-123456".to_string()).unwrap();
        credentials.set(&profile.id, &token).unwrap();
        let namespace_id = Uuid::now_v7();
        store.select_namespace(profile.id, namespace_id).unwrap();

        let profiles = store.list(&credentials).unwrap();
        assert_eq!(profiles.len(), 1);
        assert!(profiles[0].credential_configured);
        assert_eq!(
            profiles[0].profile.selected_namespace_id,
            Some(namespace_id)
        );
        assert!(!profiles[0].profile.automatic_namespace_selection);
        store
            .set_automatic_namespace_selection(profile.id, true)
            .unwrap();
        assert!(store.get(profile.id).unwrap().automatic_namespace_selection);
        let raw = fs::read_to_string(temp.path().join("config/remotes-v1.json")).unwrap();
        assert!(!raw.contains(token.expose()));
    }

    #[test]
    fn profiles_created_before_automatic_selection_load_with_it_disabled() {
        let temp = tempdir().unwrap();
        let remote_id = Uuid::now_v7();
        let created_at = "2026-07-27T00:00:00Z";
        let path = temp.path().join("config/remotes-v1.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "schemaVersion": REMOTE_CONFIG_SCHEMA_VERSION,
                "profiles": [{
                    "id": remote_id,
                    "displayName": "Legacy remote",
                    "serverUrl": "https://example.test/",
                    "selectedNamespaceId": null,
                    "createdAt": created_at,
                    "updatedAt": created_at
                }]
            }))
            .unwrap(),
        )
        .unwrap();

        let store = RemoteProfileStore::new(temp.path());
        let profile = store.get(remote_id).unwrap();

        assert!(!profile.automatic_namespace_selection);
        assert_eq!(profile.display_name, "Legacy remote");
    }
}
