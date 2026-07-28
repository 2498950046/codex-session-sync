use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;
use sync_core::codex_home_key;
use uuid::Uuid;

const MAPPING_SCHEMA_VERSION: u32 = 1;
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_MAPPINGS: usize = 512;
const FINGERPRINT_PREFIX: &str = "hmac-sha256:";
const FINGERPRINT_CONTEXT: &[u8] = b"codex-session-sync/namespace-mapping/v1\0";

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApiKeySource {
    TransientInput,
    ProviderEnvironment,
    AuthJson,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedIdentity {
    pub codex_home_key: String,
    pub provider: Option<String>,
    pub api_key_fingerprint: Option<String>,
    pub api_key_source: Option<ApiKeySource>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LocalIdentitySummary {
    pub codex_home_key: String,
    pub provider: Option<String>,
    pub api_key_available: bool,
    pub api_key_fingerprint_hint: Option<String>,
    pub api_key_source: Option<ApiKeySource>,
    pub warnings: Vec<String>,
}

impl From<&DetectedIdentity> for LocalIdentitySummary {
    fn from(identity: &DetectedIdentity) -> Self {
        Self {
            codex_home_key: identity.codex_home_key.clone(),
            provider: identity.provider.clone(),
            api_key_available: identity.api_key_fingerprint.is_some(),
            api_key_fingerprint_hint: identity
                .api_key_fingerprint
                .as_deref()
                .map(fingerprint_hint),
            api_key_source: identity.api_key_source,
            warnings: identity.warnings.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NamespaceMappingRule {
    pub id: Uuid,
    pub remote_id: Uuid,
    pub namespace_id: Uuid,
    pub label: String,
    pub api_key_fingerprint: Option<String>,
    pub provider: Option<String>,
    pub codex_home_key: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NamespaceMappingSummary {
    pub id: Uuid,
    pub remote_id: Uuid,
    pub namespace_id: Uuid,
    pub label: String,
    pub matches_api_key: bool,
    pub api_key_fingerprint_hint: Option<String>,
    pub provider: Option<String>,
    pub codex_home_key: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl From<&NamespaceMappingRule> for NamespaceMappingSummary {
    fn from(rule: &NamespaceMappingRule) -> Self {
        Self {
            id: rule.id,
            remote_id: rule.remote_id,
            namespace_id: rule.namespace_id,
            label: rule.label.clone(),
            matches_api_key: rule.api_key_fingerprint.is_some(),
            api_key_fingerprint_hint: rule.api_key_fingerprint.as_deref().map(fingerprint_hint),
            provider: rule.provider.clone(),
            codex_home_key: rule.codex_home_key.clone(),
            created_at: rule.created_at.clone(),
            updated_at: rule.updated_at.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct ManualNamespaceOverride {
    remote_id: Uuid,
    namespace_id: Uuid,
    codex_home_key: String,
    updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NamespaceMappingFile {
    schema_version: u32,
    mappings: Vec<NamespaceMappingRule>,
    overrides: Vec<ManualNamespaceOverride>,
}

impl Default for NamespaceMappingFile {
    fn default() -> Self {
        Self {
            schema_version: MAPPING_SCHEMA_VERSION,
            mappings: Vec::new(),
            overrides: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct NamespaceMappingStore {
    path: PathBuf,
}

impl NamespaceMappingStore {
    pub fn new(repository_root: impl AsRef<Path>) -> Self {
        Self {
            path: repository_root
                .as_ref()
                .join("config")
                .join("namespace-mappings-v1.json"),
        }
    }

    pub fn list(&self, remote_id: Uuid) -> Result<Vec<NamespaceMappingRule>> {
        Ok(self
            .load()?
            .mappings
            .into_iter()
            .filter(|mapping| mapping.remote_id == remote_id)
            .collect())
    }

    pub fn create(
        &self,
        remote_id: Uuid,
        namespace_id: Uuid,
        label: String,
        api_key_fingerprint: Option<String>,
        provider: Option<String>,
        codex_home_key: Option<String>,
    ) -> Result<NamespaceMappingRule> {
        let label = validate_label(&label)?;
        let provider = provider
            .map(|provider| normalize_provider(&provider))
            .transpose()?;
        let api_key_fingerprint = api_key_fingerprint
            .map(|fingerprint| validate_fingerprint(&fingerprint).map(str::to_string))
            .transpose()?;
        let codex_home_key = codex_home_key
            .map(|path| validate_path_matcher(&path).map(str::to_string))
            .transpose()?;
        if api_key_fingerprint.is_none() && provider.is_none() && codex_home_key.is_none() {
            bail!("a namespace mapping requires at least one matching condition");
        }
        validate_uuid_v7(remote_id, "remote")?;
        validate_uuid_v7(namespace_id, "namespace")?;

        let mut file = self.load()?;
        if file.mappings.len() >= MAX_MAPPINGS {
            bail!("namespace mapping limit of {MAX_MAPPINGS} was reached");
        }
        if file.mappings.iter().any(|mapping| {
            mapping.remote_id == remote_id
                && mapping.api_key_fingerprint == api_key_fingerprint
                && mapping.provider == provider
                && mapping.codex_home_key == codex_home_key
        }) {
            bail!("an identical namespace mapping condition already exists");
        }
        let now = Utc::now().to_rfc3339();
        let mapping = NamespaceMappingRule {
            id: Uuid::now_v7(),
            remote_id,
            namespace_id,
            label,
            api_key_fingerprint,
            provider,
            codex_home_key,
            created_at: now.clone(),
            updated_at: now,
        };
        file.mappings.push(mapping.clone());
        file.mappings.sort_by_key(|mapping| mapping.id);
        self.save(&file)?;
        Ok(mapping)
    }

    pub fn delete(&self, remote_id: Uuid, mapping_id: Uuid) -> Result<()> {
        let mut file = self.load()?;
        let before = file.mappings.len();
        file.mappings
            .retain(|mapping| mapping.remote_id != remote_id || mapping.id != mapping_id);
        if file.mappings.len() == before {
            bail!("namespace mapping {mapping_id} was not found");
        }
        self.save(&file)
    }

    pub fn manual_override(&self, remote_id: Uuid, codex_home_key: &str) -> Result<Option<Uuid>> {
        Ok(self
            .load()?
            .overrides
            .into_iter()
            .find(|entry| entry.remote_id == remote_id && entry.codex_home_key == codex_home_key)
            .map(|entry| entry.namespace_id))
    }

    pub fn set_manual_override(
        &self,
        remote_id: Uuid,
        namespace_id: Uuid,
        codex_home_key: String,
    ) -> Result<()> {
        validate_uuid_v7(remote_id, "remote")?;
        validate_uuid_v7(namespace_id, "namespace")?;
        validate_path_matcher(&codex_home_key)?;
        let mut file = self.load()?;
        let now = Utc::now().to_rfc3339();
        if let Some(entry) = file
            .overrides
            .iter_mut()
            .find(|entry| entry.remote_id == remote_id && entry.codex_home_key == codex_home_key)
        {
            entry.namespace_id = namespace_id;
            entry.updated_at = now;
        } else {
            file.overrides.push(ManualNamespaceOverride {
                remote_id,
                namespace_id,
                codex_home_key,
                updated_at: now,
            });
        }
        self.save(&file)
    }

    pub fn clear_manual_override(&self, remote_id: Uuid, codex_home_key: &str) -> Result<()> {
        let mut file = self.load()?;
        file.overrides
            .retain(|entry| entry.remote_id != remote_id || entry.codex_home_key != codex_home_key);
        self.save(&file)
    }

    fn load(&self) -> Result<NamespaceMappingFile> {
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(NamespaceMappingFile::default());
            }
            Err(error) => return Err(error).context("failed to open namespace mappings"),
        };
        let config_bytes = file
            .metadata()
            .context("failed to inspect namespace mappings")?
            .len();
        if config_bytes > MAX_CONFIG_BYTES {
            bail!("namespace mapping file exceeds the {MAX_CONFIG_BYTES} byte safety limit");
        }
        let file: NamespaceMappingFile = serde_json::from_reader(BufReader::new(file))
            .context("failed to parse namespace mappings")?;
        validate_mapping_file(&file)?;
        Ok(file)
    }

    fn save(&self, file: &NamespaceMappingFile) -> Result<()> {
        validate_mapping_file(file)?;
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temporary = self.path.with_file_name(format!(
            ".namespace-mappings-v1.write-{}.tmp",
            Uuid::now_v7()
        ));
        let mut output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        output.write_all(&serde_json::to_vec_pretty(file)?)?;
        output.sync_all()?;
        drop(output);
        if self.path.exists() {
            fs::remove_file(&self.path)?;
        }
        fs::rename(&temporary, &self.path)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NamespaceSelectionSource {
    ManualOverride,
    Mapping,
    ProfileDefault,
    Ambiguous,
    None,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NamespaceSelectionResolution {
    pub selected_namespace_id: Option<Uuid>,
    pub source: NamespaceSelectionSource,
    pub matched_mapping_id: Option<Uuid>,
    pub ambiguous_mapping_ids: Vec<Uuid>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NamespaceMappingState {
    pub remote_id: Uuid,
    pub automatic_enabled: bool,
    pub context: LocalIdentitySummary,
    pub mappings: Vec<NamespaceMappingSummary>,
    pub selection: NamespaceSelectionResolution,
}

pub fn build_mapping_state(
    remote_id: Uuid,
    automatic_enabled: bool,
    profile_default: Option<Uuid>,
    identity: &DetectedIdentity,
    rules: &[NamespaceMappingRule],
    manual_override: Option<Uuid>,
) -> NamespaceMappingState {
    NamespaceMappingState {
        remote_id,
        automatic_enabled,
        context: LocalIdentitySummary::from(identity),
        mappings: rules.iter().map(NamespaceMappingSummary::from).collect(),
        selection: resolve_namespace_selection(
            automatic_enabled,
            profile_default,
            identity,
            rules,
            manual_override,
        ),
    }
}

pub fn resolve_namespace_selection(
    automatic_enabled: bool,
    profile_default: Option<Uuid>,
    identity: &DetectedIdentity,
    rules: &[NamespaceMappingRule],
    manual_override: Option<Uuid>,
) -> NamespaceSelectionResolution {
    if !automatic_enabled {
        return fallback_resolution(profile_default);
    }
    if let Some(namespace_id) = manual_override {
        return NamespaceSelectionResolution {
            selected_namespace_id: Some(namespace_id),
            source: NamespaceSelectionSource::ManualOverride,
            matched_mapping_id: None,
            ambiguous_mapping_ids: Vec::new(),
        };
    }
    let mut matches = rules
        .iter()
        .filter_map(|rule| rule_match_score(rule, identity).map(|score| (score, rule)))
        .collect::<Vec<_>>();
    matches.sort_by(|(left_score, left), (right_score, right)| {
        right_score
            .cmp(left_score)
            .then_with(|| left.id.cmp(&right.id))
    });
    let Some((highest_score, first)) = matches.first().copied() else {
        return fallback_resolution(profile_default);
    };
    let highest = matches
        .into_iter()
        .take_while(|(score, _)| *score == highest_score)
        .map(|(_, rule)| rule)
        .collect::<Vec<_>>();
    if highest
        .iter()
        .any(|rule| rule.namespace_id != first.namespace_id)
    {
        return NamespaceSelectionResolution {
            selected_namespace_id: None,
            source: NamespaceSelectionSource::Ambiguous,
            matched_mapping_id: None,
            ambiguous_mapping_ids: highest.iter().map(|rule| rule.id).collect(),
        };
    }
    NamespaceSelectionResolution {
        selected_namespace_id: Some(first.namespace_id),
        source: NamespaceSelectionSource::Mapping,
        matched_mapping_id: Some(first.id),
        ambiguous_mapping_ids: Vec::new(),
    }
}

fn fallback_resolution(profile_default: Option<Uuid>) -> NamespaceSelectionResolution {
    NamespaceSelectionResolution {
        selected_namespace_id: profile_default,
        source: if profile_default.is_some() {
            NamespaceSelectionSource::ProfileDefault
        } else {
            NamespaceSelectionSource::None
        },
        matched_mapping_id: None,
        ambiguous_mapping_ids: Vec::new(),
    }
}

fn rule_match_score(rule: &NamespaceMappingRule, identity: &DetectedIdentity) -> Option<u8> {
    let mut score = 0;
    if let Some(fingerprint) = &rule.api_key_fingerprint {
        if identity.api_key_fingerprint.as_ref() != Some(fingerprint) {
            return None;
        }
        score += 4;
    }
    if let Some(provider) = &rule.provider {
        if identity.provider.as_ref() != Some(provider) {
            return None;
        }
        score += 2;
    }
    if let Some(codex_home) = &rule.codex_home_key {
        if &identity.codex_home_key != codex_home {
            return None;
        }
        score += 1;
    }
    (score > 0).then_some(score)
}

pub fn detect_local_identity(
    codex_home: &Path,
    server_url: &str,
    transient_api_key: Option<&str>,
) -> Result<DetectedIdentity> {
    detect_local_identity_with_env(codex_home, server_url, transient_api_key, |name| {
        std::env::var(name).ok()
    })
}

fn detect_local_identity_with_env(
    codex_home: &Path,
    server_url: &str,
    transient_api_key: Option<&str>,
    environment: impl Fn(&str) -> Option<String>,
) -> Result<DetectedIdentity> {
    let codex_home_key = codex_home_key(codex_home)?;
    let mut warnings = Vec::new();
    let config =
        read_optional_text(&codex_home.join("config.toml"), &mut warnings).and_then(|text| {
            match text.parse::<toml::Value>() {
                Ok(value) => Some(value),
                Err(error) => {
                    warnings.push(format!("config.toml could not be parsed: {error}"));
                    None
                }
            }
        });
    let provider_name = config
        .as_ref()
        .and_then(|config| config.get("model_provider"))
        .and_then(toml::Value::as_str);
    let provider = provider_name.map(normalize_provider).transpose()?;
    let environment_key = provider_name.and_then(|provider_name| {
        config
            .as_ref()?
            .get("model_providers")?
            .get(provider_name)?
            .get("env_key")?
            .as_str()
    });
    let auth_key =
        read_optional_text(&codex_home.join("auth.json"), &mut warnings).and_then(|text| {
            match serde_json::from_str::<Value>(&text) {
                Ok(Value::Object(object)) => object
                    .get("OPENAI_API_KEY")
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                    .or_else(|| object.get("api_key").and_then(Value::as_str))
                    .filter(|value| !value.trim().is_empty())
                    .map(str::to_string),
                Ok(_) => {
                    warnings.push("auth.json root must be an object".to_string());
                    None
                }
                Err(error) => {
                    warnings.push(format!("auth.json could not be parsed: {error}"));
                    None
                }
            }
        });

    let detected = if let Some(api_key) = transient_api_key.filter(|value| !value.trim().is_empty())
    {
        Some((api_key.trim().to_string(), ApiKeySource::TransientInput))
    } else if let Some(environment_key) = environment_key {
        if valid_environment_key(environment_key) {
            environment(environment_key)
                .filter(|value| !value.trim().is_empty())
                .map(|value| (value, ApiKeySource::ProviderEnvironment))
                .or_else(|| auth_key.map(|value| (value, ApiKeySource::AuthJson)))
        } else {
            warnings.push("provider env_key is invalid and was ignored".to_string());
            auth_key.map(|value| (value, ApiKeySource::AuthJson))
        }
    } else {
        auth_key.map(|value| (value, ApiKeySource::AuthJson))
    };
    let (api_key_fingerprint, api_key_source) = match detected {
        Some((api_key, source)) => match fingerprint_api_key(&api_key, server_url) {
            Ok(fingerprint) => (Some(fingerprint), Some(source)),
            Err(error) if source != ApiKeySource::TransientInput => {
                warnings.push(format!("the detected API key was ignored: {error}"));
                (None, None)
            }
            Err(error) => return Err(error),
        },
        None => (None, None),
    };
    Ok(DetectedIdentity {
        codex_home_key,
        provider,
        api_key_fingerprint,
        api_key_source,
        warnings,
    })
}

pub fn fingerprint_api_key(api_key: &str, normalized_server_url: &str) -> Result<String> {
    let api_key = api_key.trim();
    if api_key.is_empty()
        || api_key.len() > 16 * 1024
        || !api_key.bytes().all(|byte| byte.is_ascii_graphic())
    {
        bail!("API key must contain 1 to 16384 visible ASCII characters");
    }
    let mut hmac = HmacSha256::new_from_slice(api_key.as_bytes())
        .expect("HMAC-SHA256 accepts keys of any size");
    hmac.update(FINGERPRINT_CONTEXT);
    hmac.update(normalized_server_url.as_bytes());
    Ok(format!(
        "{FINGERPRINT_PREFIX}{}",
        hex::encode(hmac.finalize().into_bytes())
    ))
}

pub fn normalize_provider(provider: &str) -> Result<String> {
    let provider = provider.trim().to_ascii_lowercase();
    if provider.is_empty()
        || provider.len() > 128
        || !provider
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        bail!("provider must contain 1 to 128 ASCII letters, digits, '.', '-' or '_'");
    }
    Ok(provider)
}

fn read_optional_text(path: &Path, warnings: &mut Vec<String>) -> Option<String> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            warnings.push(format!(
                "{} could not be inspected: {error}",
                path.display()
            ));
            return None;
        }
    };
    if metadata.len() > MAX_CONFIG_BYTES {
        warnings.push(format!(
            "{} exceeds the {} byte safety limit",
            path.display(),
            MAX_CONFIG_BYTES
        ));
        return None;
    }
    match fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(error) => {
            warnings.push(format!("{} could not be read: {error}", path.display()));
            None
        }
    }
}

fn validate_mapping_file(file: &NamespaceMappingFile) -> Result<()> {
    if file.schema_version != MAPPING_SCHEMA_VERSION {
        bail!(
            "unsupported namespace mapping schema version {}",
            file.schema_version
        );
    }
    if file.mappings.len() > MAX_MAPPINGS {
        bail!("namespace mapping file exceeds the {MAX_MAPPINGS} rule limit");
    }
    let mut ids = BTreeSet::new();
    for mapping in &file.mappings {
        if !ids.insert(mapping.id) {
            bail!("namespace mapping file contains duplicate rule IDs");
        }
        validate_uuid_v7(mapping.id, "mapping")?;
        validate_uuid_v7(mapping.remote_id, "remote")?;
        validate_uuid_v7(mapping.namespace_id, "namespace")?;
        validate_label(&mapping.label)?;
        if let Some(fingerprint) = &mapping.api_key_fingerprint {
            validate_fingerprint(fingerprint)?;
        }
        if let Some(provider) = &mapping.provider
            && normalize_provider(provider)? != *provider
        {
            bail!("namespace mapping provider is not normalized");
        }
        if let Some(path) = &mapping.codex_home_key {
            validate_path_matcher(path)?;
        }
        if mapping.api_key_fingerprint.is_none()
            && mapping.provider.is_none()
            && mapping.codex_home_key.is_none()
        {
            bail!("namespace mapping has no matching condition");
        }
        validate_timestamp(&mapping.created_at)?;
        validate_timestamp(&mapping.updated_at)?;
    }
    let mut override_keys = BTreeSet::new();
    for entry in &file.overrides {
        validate_uuid_v7(entry.remote_id, "remote")?;
        validate_uuid_v7(entry.namespace_id, "namespace")?;
        validate_path_matcher(&entry.codex_home_key)?;
        validate_timestamp(&entry.updated_at)?;
        if !override_keys.insert((entry.remote_id, entry.codex_home_key.as_str())) {
            bail!("namespace mapping file contains duplicate manual overrides");
        }
    }
    Ok(())
}

fn validate_label(label: &str) -> Result<String> {
    let label = label.trim();
    if label.is_empty() || label.chars().count() > 128 || label.chars().any(char::is_control) {
        bail!("mapping label must contain 1 to 128 non-control characters");
    }
    Ok(label.to_string())
}

fn validate_fingerprint(fingerprint: &str) -> Result<&str> {
    let digest = fingerprint
        .strip_prefix(FINGERPRINT_PREFIX)
        .context("API-key fingerprint has an unsupported format")?;
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("API-key fingerprint is not canonical HMAC-SHA256");
    }
    Ok(fingerprint)
}

fn validate_path_matcher(path: &str) -> Result<&str> {
    if path.trim().is_empty() || path.chars().any(char::is_control) {
        bail!("Codex Home matcher is invalid");
    }
    Ok(path)
}

fn validate_uuid_v7(id: Uuid, field: &str) -> Result<()> {
    if id.get_version_num() != 7 {
        bail!("{field} ID must be a UUIDv7");
    }
    Ok(())
}

fn validate_timestamp(timestamp: &str) -> Result<()> {
    DateTime::parse_from_rfc3339(timestamp)
        .with_context(|| format!("invalid namespace mapping timestamp {timestamp}"))?;
    Ok(())
}

fn valid_environment_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 128
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn fingerprint_hint(fingerprint: &str) -> String {
    fingerprint
        .strip_prefix(FINGERPRINT_PREFIX)
        .unwrap_or(fingerprint)
        .chars()
        .take(12)
        .collect()
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn fingerprint_is_deterministic_server_scoped_and_contains_no_api_key() {
        let api_key = "example-api-key-secret-value";
        let first = fingerprint_api_key(api_key, "https://one.example/").unwrap();
        assert_eq!(
            first,
            fingerprint_api_key(api_key, "https://one.example/").unwrap()
        );
        assert_ne!(
            first,
            fingerprint_api_key(api_key, "https://two.example/").unwrap()
        );
        assert!(!first.contains(api_key));
        validate_fingerprint(&first).unwrap();
    }

    #[test]
    fn identity_detection_reads_provider_environment_without_exposing_the_key() {
        let temp = tempdir().unwrap();
        fs::write(
            temp.path().join("config.toml"),
            "model_provider = \"custom\"\n[model_providers.custom]\nenv_key = \"CUSTOM_API_KEY\"\n",
        )
        .unwrap();
        let api_key = "custom-secret-value";
        let identity =
            detect_local_identity_with_env(temp.path(), "https://sync.example/", None, |name| {
                (name == "CUSTOM_API_KEY").then(|| api_key.to_string())
            })
            .unwrap();
        assert_eq!(identity.provider.as_deref(), Some("custom"));
        assert_eq!(
            identity.api_key_source,
            Some(ApiKeySource::ProviderEnvironment)
        );
        assert!(!identity.api_key_fingerprint.unwrap().contains(api_key));
        assert!(identity.warnings.is_empty());
    }

    #[test]
    fn identity_detection_uses_the_exact_provider_table_name_for_environment_lookup() {
        let temp = tempdir().unwrap();
        fs::write(
            temp.path().join("config.toml"),
            "model_provider = \"CustomProvider\"\n[model_providers.CustomProvider]\nenv_key = \"CUSTOM_API_KEY\"\n",
        )
        .unwrap();
        let identity =
            detect_local_identity_with_env(temp.path(), "https://sync.example/", None, |name| {
                (name == "CUSTOM_API_KEY").then(|| "custom-secret-value".to_string())
            })
            .unwrap();

        assert_eq!(identity.provider.as_deref(), Some("customprovider"));
        assert_eq!(
            identity.api_key_source,
            Some(ApiKeySource::ProviderEnvironment)
        );
        assert!(identity.api_key_fingerprint.is_some());
    }

    #[test]
    fn identity_detection_falls_back_to_explicit_auth_json_field() {
        let temp = tempdir().unwrap();
        fs::write(
            temp.path().join("auth.json"),
            r#"{"OPENAI_API_KEY":"auth-json-secret"}"#,
        )
        .unwrap();
        let identity =
            detect_local_identity_with_env(temp.path(), "https://sync.example/", None, |_| None)
                .unwrap();
        assert_eq!(identity.api_key_source, Some(ApiKeySource::AuthJson));
        assert!(identity.api_key_fingerprint.is_some());
    }

    #[test]
    fn identity_detection_uses_api_key_when_openai_field_is_not_a_string() {
        let temp = tempdir().unwrap();
        fs::write(
            temp.path().join("auth.json"),
            r#"{"OPENAI_API_KEY":null,"api_key":"fallback-secret"}"#,
        )
        .unwrap();

        let identity =
            detect_local_identity_with_env(temp.path(), "https://sync.example/", None, |_| None)
                .unwrap();

        assert_eq!(identity.api_key_source, Some(ApiKeySource::AuthJson));
        assert!(identity.api_key_fingerprint.is_some());
    }

    #[test]
    fn identity_detection_uses_api_key_when_openai_field_is_blank() {
        let temp = tempdir().unwrap();
        fs::write(
            temp.path().join("auth.json"),
            r#"{"OPENAI_API_KEY":"  ","api_key":"fallback-secret"}"#,
        )
        .unwrap();

        let identity =
            detect_local_identity_with_env(temp.path(), "https://sync.example/", None, |_| None)
                .unwrap();

        assert_eq!(identity.api_key_source, Some(ApiKeySource::AuthJson));
        assert!(identity.api_key_fingerprint.is_some());
        assert!(identity.warnings.is_empty());
    }

    #[test]
    fn malformed_local_config_becomes_a_warning() {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join("config.toml"), "model_provider = [").unwrap();
        let identity =
            detect_local_identity_with_env(temp.path(), "https://sync.example/", None, |_| None)
                .unwrap();
        assert_eq!(identity.provider, None);
        assert_eq!(identity.warnings.len(), 1);
    }

    #[test]
    fn mapping_store_persists_only_fingerprint_and_supports_overrides() {
        let temp = tempdir().unwrap();
        let store = NamespaceMappingStore::new(temp.path());
        let remote_id = Uuid::now_v7();
        let namespace_id = Uuid::now_v7();
        let api_key = "never-store-this-api-key";
        let fingerprint = fingerprint_api_key(api_key, "https://sync.example/").unwrap();
        let rule = store
            .create(
                remote_id,
                namespace_id,
                "Personal".to_string(),
                Some(fingerprint.clone()),
                Some("OpenAI".to_string()),
                Some("c:/users/demo/.codex".to_string()),
            )
            .unwrap();
        let stored = store.list(remote_id).unwrap();
        assert_eq!(stored, vec![rule.clone()]);
        let raw =
            fs::read_to_string(temp.path().join("config/namespace-mappings-v1.json")).unwrap();
        assert!(raw.contains(&fingerprint));
        assert!(!raw.contains(api_key));

        store
            .set_manual_override(remote_id, namespace_id, "c:/users/demo/.codex".to_string())
            .unwrap();
        assert_eq!(
            store
                .manual_override(remote_id, "c:/users/demo/.codex")
                .unwrap(),
            Some(namespace_id)
        );
        store
            .clear_manual_override(remote_id, "c:/users/demo/.codex")
            .unwrap();
        assert_eq!(
            store
                .manual_override(remote_id, "c:/users/demo/.codex")
                .unwrap(),
            None
        );
        store.delete(remote_id, rule.id).unwrap();
        assert!(store.list(remote_id).unwrap().is_empty());
    }

    #[test]
    fn mapping_store_rejects_config_larger_than_the_safety_limit() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("config/namespace-mappings-v1.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            path,
            format!(
                r#"{{"schemaVersion":1,"mappings":[],"overrides":[],"padding":"{}"}}"#,
                "a".repeat(MAX_CONFIG_BYTES as usize)
            ),
        )
        .unwrap();

        let error = NamespaceMappingStore::new(temp.path())
            .list(Uuid::now_v7())
            .unwrap_err();

        assert!(error.to_string().contains("safety limit"));
    }

    #[test]
    fn selection_prefers_specific_rules_and_fails_closed_on_ambiguity() {
        let remote_id = Uuid::now_v7();
        let provider_namespace = Uuid::now_v7();
        let key_namespace = Uuid::now_v7();
        let identity = DetectedIdentity {
            codex_home_key: "home".to_string(),
            provider: Some("openai".to_string()),
            api_key_fingerprint: Some(format!("{FINGERPRINT_PREFIX}{}", "a".repeat(64))),
            api_key_source: Some(ApiKeySource::AuthJson),
            warnings: Vec::new(),
        };
        let provider_rule = rule(remote_id, provider_namespace, None, Some("openai"), None);
        let key_rule = rule(
            remote_id,
            key_namespace,
            identity.api_key_fingerprint.as_deref(),
            None,
            None,
        );
        let selected = resolve_namespace_selection(
            true,
            None,
            &identity,
            &[provider_rule.clone(), key_rule.clone()],
            None,
        );
        assert_eq!(selected.selected_namespace_id, Some(key_namespace));
        assert_eq!(selected.matched_mapping_id, Some(key_rule.id));

        let conflicting = rule(
            remote_id,
            Uuid::now_v7(),
            identity.api_key_fingerprint.as_deref(),
            None,
            None,
        );
        let ambiguous = resolve_namespace_selection(
            true,
            Some(provider_namespace),
            &identity,
            &[key_rule, conflicting],
            None,
        );
        assert_eq!(ambiguous.source, NamespaceSelectionSource::Ambiguous);
        assert_eq!(ambiguous.selected_namespace_id, None);
        assert_eq!(ambiguous.ambiguous_mapping_ids.len(), 2);

        let override_namespace = Uuid::now_v7();
        let overridden =
            resolve_namespace_selection(true, None, &identity, &[], Some(override_namespace));
        assert_eq!(overridden.source, NamespaceSelectionSource::ManualOverride);
        assert_eq!(overridden.selected_namespace_id, Some(override_namespace));
    }

    fn rule(
        remote_id: Uuid,
        namespace_id: Uuid,
        fingerprint: Option<&str>,
        provider: Option<&str>,
        codex_home: Option<&str>,
    ) -> NamespaceMappingRule {
        NamespaceMappingRule {
            id: Uuid::now_v7(),
            remote_id,
            namespace_id,
            label: "rule".to_string(),
            api_key_fingerprint: fingerprint.map(str::to_string),
            provider: provider.map(str::to_string),
            codex_home_key: codex_home.map(str::to_string),
            created_at: Utc::now().to_rfc3339(),
            updated_at: Utc::now().to_rfc3339(),
        }
    }
}
