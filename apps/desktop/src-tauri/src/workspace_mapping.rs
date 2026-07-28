use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sync_core::{WorkspacePathMapper, WorkspacePathMapping, codex_home_key};
use uuid::Uuid;

const WORKSPACE_MAPPING_SCHEMA_VERSION: u32 = 1;
const MAX_MAPPINGS: usize = 128;
const MAX_CONFIG_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceMappingRule {
    pub id: Uuid,
    pub remote_id: Uuid,
    pub namespace_id: Uuid,
    pub codex_home_key: String,
    pub remote_prefix: String,
    pub local_prefix: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceMappingState {
    pub remote_id: Uuid,
    pub namespace_id: Uuid,
    pub codex_home_key: String,
    pub mappings: Vec<WorkspaceMappingRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceMappingFile {
    schema_version: u32,
    mappings: Vec<WorkspaceMappingRule>,
}

impl Default for WorkspaceMappingFile {
    fn default() -> Self {
        Self {
            schema_version: WORKSPACE_MAPPING_SCHEMA_VERSION,
            mappings: Vec::new(),
        }
    }
}

pub struct WorkspaceMappingStore {
    path: std::path::PathBuf,
}

impl WorkspaceMappingStore {
    pub fn new(repository_root: impl AsRef<Path>) -> Self {
        Self {
            path: repository_root
                .as_ref()
                .join("config")
                .join("workspace-mappings-v1.json"),
        }
    }

    pub fn state(
        &self,
        codex_home: &Path,
        remote_id: Uuid,
        namespace_id: Uuid,
    ) -> Result<WorkspaceMappingState> {
        let codex_home_key = codex_home_key(codex_home)?;
        let mappings = self.matching_rules(remote_id, namespace_id, &codex_home_key)?;
        Ok(WorkspaceMappingState {
            remote_id,
            namespace_id,
            codex_home_key,
            mappings,
        })
    }

    pub fn mapper(
        &self,
        codex_home: &Path,
        remote_id: Uuid,
        namespace_id: Uuid,
    ) -> Result<WorkspacePathMapper> {
        let codex_home_key = codex_home_key(codex_home)?;
        let mappings = self
            .matching_rules(remote_id, namespace_id, &codex_home_key)?
            .into_iter()
            .map(|mapping| WorkspacePathMapping {
                remote_prefix: mapping.remote_prefix,
                local_prefix: mapping.local_prefix,
            })
            .collect();
        WorkspacePathMapper::new(mappings)
    }

    pub fn create(
        &self,
        codex_home: &Path,
        remote_id: Uuid,
        namespace_id: Uuid,
        remote_prefix: String,
        local_prefix: String,
    ) -> Result<WorkspaceMappingState> {
        let codex_home_key = codex_home_key(codex_home)?;
        let mut file = self.load()?;
        if file.mappings.len() >= MAX_MAPPINGS {
            bail!("workspace mapping limit of {MAX_MAPPINGS} was reached");
        }
        let now = Utc::now().to_rfc3339();
        let candidate = WorkspaceMappingRule {
            id: Uuid::now_v7(),
            remote_id,
            namespace_id,
            codex_home_key: codex_home_key.clone(),
            remote_prefix: remote_prefix.trim().to_string(),
            local_prefix: local_prefix.trim().to_string(),
            created_at: now.clone(),
            updated_at: now,
        };
        validate_rule(&candidate)?;
        if !Path::new(&candidate.local_prefix).is_dir() {
            bail!(
                "local workspace mapping target is not an existing directory: {}",
                candidate.local_prefix
            );
        }
        let mut scoped = file
            .mappings
            .iter()
            .filter(|mapping| {
                mapping.remote_id == remote_id
                    && mapping.namespace_id == namespace_id
                    && mapping.codex_home_key == codex_home_key
            })
            .map(|mapping| WorkspacePathMapping {
                remote_prefix: mapping.remote_prefix.clone(),
                local_prefix: mapping.local_prefix.clone(),
            })
            .collect::<Vec<_>>();
        scoped.push(WorkspacePathMapping {
            remote_prefix: candidate.remote_prefix.clone(),
            local_prefix: candidate.local_prefix.clone(),
        });
        WorkspacePathMapper::new(scoped)?;
        file.mappings.push(candidate);
        self.save(&file)?;
        self.state(codex_home, remote_id, namespace_id)
    }

    pub fn delete(
        &self,
        codex_home: &Path,
        remote_id: Uuid,
        namespace_id: Uuid,
        mapping_id: Uuid,
    ) -> Result<WorkspaceMappingState> {
        let codex_home_key = codex_home_key(codex_home)?;
        let mut file = self.load()?;
        let previous = file.mappings.len();
        file.mappings.retain(|mapping| {
            mapping.id != mapping_id
                || mapping.remote_id != remote_id
                || mapping.namespace_id != namespace_id
                || mapping.codex_home_key != codex_home_key
        });
        if file.mappings.len() == previous {
            bail!("workspace mapping {mapping_id} was not found");
        }
        self.save(&file)?;
        self.state(codex_home, remote_id, namespace_id)
    }

    fn matching_rules(
        &self,
        remote_id: Uuid,
        namespace_id: Uuid,
        codex_home_key: &str,
    ) -> Result<Vec<WorkspaceMappingRule>> {
        let mut mappings = self
            .load()?
            .mappings
            .into_iter()
            .filter(|mapping| {
                mapping.remote_id == remote_id
                    && mapping.namespace_id == namespace_id
                    && mapping.codex_home_key == codex_home_key
            })
            .collect::<Vec<_>>();
        mappings.sort_by(|left, right| left.created_at.cmp(&right.created_at));
        Ok(mappings)
    }

    fn load(&self) -> Result<WorkspaceMappingFile> {
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(WorkspaceMappingFile::default());
            }
            Err(error) => return Err(error).context("failed to open workspace mappings"),
        };
        if file.metadata()?.len() > MAX_CONFIG_BYTES {
            bail!("workspace mapping file exceeds the {MAX_CONFIG_BYTES} byte safety limit");
        }
        let file: WorkspaceMappingFile = serde_json::from_reader(BufReader::new(file))
            .context("failed to parse workspace mappings")?;
        validate_file(&file)?;
        Ok(file)
    }

    fn save(&self, file: &WorkspaceMappingFile) -> Result<()> {
        validate_file(file)?;
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temporary = self.path.with_file_name(format!(
            ".workspace-mappings-v1.write-{}.tmp",
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

fn validate_file(file: &WorkspaceMappingFile) -> Result<()> {
    if file.schema_version != WORKSPACE_MAPPING_SCHEMA_VERSION {
        bail!(
            "unsupported workspace mapping schema version {}",
            file.schema_version
        );
    }
    if file.mappings.len() > MAX_MAPPINGS {
        bail!("workspace mapping file exceeds the {MAX_MAPPINGS} rule limit");
    }
    let mut ids = BTreeSet::new();
    let mut scopes = BTreeMap::<(Uuid, Uuid, String), Vec<WorkspacePathMapping>>::new();
    for mapping in &file.mappings {
        validate_rule(mapping)?;
        if !ids.insert(mapping.id) {
            bail!("workspace mapping file contains duplicate rule IDs");
        }
        scopes
            .entry((
                mapping.remote_id,
                mapping.namespace_id,
                mapping.codex_home_key.clone(),
            ))
            .or_default()
            .push(WorkspacePathMapping {
                remote_prefix: mapping.remote_prefix.clone(),
                local_prefix: mapping.local_prefix.clone(),
            });
    }
    for mappings in scopes.into_values() {
        WorkspacePathMapper::new(mappings)?;
    }
    Ok(())
}

fn validate_rule(mapping: &WorkspaceMappingRule) -> Result<()> {
    if mapping.remote_prefix.trim().is_empty() || mapping.local_prefix.trim().is_empty() {
        bail!("workspace mapping prefixes must not be empty");
    }
    if mapping.remote_prefix.chars().count() > 1024 || mapping.local_prefix.chars().count() > 1024 {
        bail!("workspace mapping prefixes must not exceed 1024 characters");
    }
    if mapping.remote_prefix.chars().any(char::is_control)
        || mapping.local_prefix.chars().any(char::is_control)
    {
        bail!("workspace mapping prefixes must not contain control characters");
    }
    chrono::DateTime::parse_from_rfc3339(&mapping.created_at)?;
    chrono::DateTime::parse_from_rfc3339(&mapping.updated_at)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn mappings_are_scoped_and_build_a_bidirectional_mapper() {
        let directory = tempdir().unwrap();
        let codex_home = directory.path().join("codex-home");
        let local_workspace = directory.path().join("workspace");
        fs::create_dir_all(&codex_home).unwrap();
        fs::create_dir_all(&local_workspace).unwrap();
        let remote_id = Uuid::now_v7();
        let namespace_id = Uuid::now_v7();
        let store = WorkspaceMappingStore::new(directory.path());
        let state = store
            .create(
                &codex_home,
                remote_id,
                namespace_id,
                "D:/projects".to_string(),
                local_workspace.to_string_lossy().into_owned(),
            )
            .unwrap();
        assert_eq!(state.mappings.len(), 1);
        let mapper = store.mapper(&codex_home, remote_id, namespace_id).unwrap();
        assert!(
            mapper
                .remote_to_local("D:/projects/demo")
                .unwrap()
                .replace('\\', "/")
                .ends_with("/workspace/demo")
        );
        assert!(
            store
                .state(&codex_home, remote_id, Uuid::now_v7())
                .unwrap()
                .mappings
                .is_empty()
        );
    }
}
