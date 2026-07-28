use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};

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

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspacePathCandidate {
    pub remote_path: String,
    pub suggested_subdirectory: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspacePullPlan {
    pub remote_id: Uuid,
    pub namespace_id: Uuid,
    pub remote_head: Option<String>,
    pub mapped_path_count: usize,
    pub existing_path_count: usize,
    pub unmapped_paths: Vec<WorkspacePathCandidate>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AutomaticWorkspaceMappingResult {
    pub state: WorkspaceMappingState,
    pub created_directories: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspacePathSelection {
    pub remote_path: String,
    pub local_path: String,
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

    pub fn pull_plan(
        &self,
        codex_home: &Path,
        remote_id: Uuid,
        namespace_id: Uuid,
        remote_head: Option<String>,
        remote_paths: Vec<String>,
    ) -> Result<WorkspacePullPlan> {
        let mapper = self.mapper(codex_home, remote_id, namespace_id)?;
        let (mapped_path_count, existing_path_count, unmapped_paths) =
            classify_remote_paths(remote_paths, &mapper);
        Ok(WorkspacePullPlan {
            remote_id,
            namespace_id,
            remote_head,
            mapped_path_count,
            existing_path_count,
            unmapped_paths,
        })
    }

    pub fn create_automatic(
        &self,
        codex_home: &Path,
        remote_id: Uuid,
        namespace_id: Uuid,
        selections: Vec<WorkspacePathSelection>,
    ) -> Result<AutomaticWorkspaceMappingResult> {
        let codex_home_key = codex_home_key(codex_home)?;
        let mut file = self.load()?;
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
        if selections.is_empty() {
            bail!("at least one workspace path selection is required");
        }
        if file.mappings.len() + selections.len() > MAX_MAPPINGS {
            bail!("automatic workspace mappings would exceed the {MAX_MAPPINGS} rule limit");
        }

        let now = Utc::now().to_rfc3339();
        let mut created_directories = Vec::<PathBuf>::new();
        let mut seen_remote_paths = BTreeSet::new();
        let mut new_rules = Vec::with_capacity(selections.len());
        for selection in selections {
            let remote_prefix = selection.remote_path.trim().to_string();
            let local_prefix = selection.local_path.trim().to_string();
            if remote_prefix.is_empty() || local_prefix.is_empty() {
                bail!("workspace path selections must contain both remote and local paths");
            }
            if !seen_remote_paths.insert(normalize_path_for_match(&remote_prefix)) {
                bail!("workspace path selections contain a duplicate remote path");
            }
            let local_path = PathBuf::from(&local_prefix);
            if local_path.exists() && !local_path.is_dir() {
                bail!(
                    "workspace mapping target exists but is not a directory: {}",
                    local_path.display()
                );
            }
            let rule = WorkspaceMappingRule {
                id: Uuid::now_v7(),
                remote_id,
                namespace_id,
                codex_home_key: codex_home_key.clone(),
                remote_prefix,
                local_prefix: local_prefix.clone(),
                created_at: now.clone(),
                updated_at: now.clone(),
            };
            validate_rule(&rule)?;
            scoped.push(WorkspacePathMapping {
                remote_prefix: rule.remote_prefix.clone(),
                local_prefix,
            });
            if !local_path.is_dir() {
                created_directories.push(local_path);
            }
            new_rules.push(rule);
        }
        WorkspacePathMapper::new(scoped)?;
        for directory in &created_directories {
            fs::create_dir_all(directory).with_context(|| {
                format!(
                    "failed to create automatic workspace directory {}",
                    directory.display()
                )
            })?;
        }
        file.mappings.extend(new_rules);
        self.save(&file)?;
        Ok(AutomaticWorkspaceMappingResult {
            state: self.state(codex_home, remote_id, namespace_id)?,
            created_directories: created_directories
                .into_iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
        })
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

pub fn collect_workspace_paths(threads: &[sync_core::ThreadBundle]) -> Vec<String> {
    let mut paths = BTreeMap::new();
    for thread in threads {
        if let Some(path) = thread.workspace.source_path.as_deref() {
            insert_workspace_path(&mut paths, path);
        }
        if let Some(rows) = thread.related_records.tables.get("threads") {
            for row in rows {
                if let Some(path) = row.get("cwd").and_then(serde_json::Value::as_str) {
                    insert_workspace_path(&mut paths, path);
                }
            }
        }
    }
    paths.into_values().collect()
}

fn insert_workspace_path(paths: &mut BTreeMap<String, String>, path: &str) {
    let path = path.trim();
    if !path.is_empty() {
        paths
            .entry(normalize_path_for_match(path))
            .or_insert_with(|| path.to_string());
    }
}

fn classify_remote_paths(
    remote_paths: Vec<String>,
    mapper: &WorkspacePathMapper,
) -> (usize, usize, Vec<WorkspacePathCandidate>) {
    let mut unique = BTreeMap::new();
    for path in remote_paths {
        insert_workspace_path(&mut unique, &path);
    }
    let mut mapped = 0;
    let mut existing = 0;
    let mut missing = Vec::new();
    for path in unique.into_values() {
        if mapper.remote_to_local(&path).is_some() {
            mapped += 1;
        } else if Path::new(&path).is_dir() {
            existing += 1;
        } else {
            missing.push(path);
        }
    }
    let missing = minimal_remote_roots(missing)
        .into_iter()
        .map(|remote_path| WorkspacePathCandidate {
            suggested_subdirectory: suggested_subdirectory(&remote_path),
            remote_path,
        })
        .collect();
    (mapped, existing, missing)
}

fn minimal_remote_roots(mut paths: Vec<String>) -> Vec<String> {
    paths.sort_by_key(|path| normalize_path_for_match(path).chars().count());
    let mut roots = Vec::<String>::new();
    for path in paths {
        if roots.iter().any(|root| path_is_same_or_child(&path, root)) {
            continue;
        }
        roots.push(path);
    }
    roots.sort_by_key(|path| normalize_path_for_match(path));
    roots
}

fn path_is_same_or_child(path: &str, parent: &str) -> bool {
    let path = normalize_path_for_match(path);
    let parent = normalize_path_for_match(parent);
    path == parent
        || path
            .strip_prefix(&parent)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn suggested_subdirectory(remote_path: &str) -> String {
    let normalized = remote_path.replace('\\', "/");
    let leaf = normalized
        .trim_end_matches('/')
        .rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or("project");
    let mut safe = leaf
        .chars()
        .map(|character| {
            if character.is_control() || r#"<>:\"/\\|?*"#.contains(character) {
                '_'
            } else {
                character
            }
        })
        .collect::<String>();
    safe = safe.trim_matches([' ', '.']).to_string();
    if safe.is_empty() || safe == "." || safe == ".." {
        "project".to_string()
    } else {
        safe
    }
}

fn normalize_path_for_match(path: &str) -> String {
    let normalized = path.replace('\\', "/").trim_end_matches('/').to_string();
    if normalized.starts_with("//")
        || normalized
            .as_bytes()
            .get(1)
            .is_some_and(|value| *value == b':')
    {
        normalized.to_ascii_lowercase()
    } else {
        normalized
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

    #[test]
    fn pull_plan_ignores_existing_and_mapped_paths_and_reduces_child_paths() {
        let directory = tempdir().unwrap();
        let codex_home = directory.path().join("codex-home");
        let mapped_root = directory.path().join("mapped");
        let existing = directory.path().join("already-here");
        fs::create_dir_all(&codex_home).unwrap();
        fs::create_dir_all(&mapped_root).unwrap();
        fs::create_dir_all(&existing).unwrap();
        let remote_id = Uuid::now_v7();
        let namespace_id = Uuid::now_v7();
        let store = WorkspaceMappingStore::new(directory.path());
        store
            .create(
                &codex_home,
                remote_id,
                namespace_id,
                "D:/mapped".to_string(),
                mapped_root.to_string_lossy().into_owned(),
            )
            .unwrap();

        let plan = store
            .pull_plan(
                &codex_home,
                remote_id,
                namespace_id,
                Some("sha256:head".to_string()),
                vec![
                    "D:/mapped/project".to_string(),
                    existing.to_string_lossy().into_owned(),
                    "Z:/missing/project".to_string(),
                    "Z:/missing/project/subdirectory".to_string(),
                ],
            )
            .unwrap();
        assert_eq!(plan.mapped_path_count, 1);
        assert_eq!(plan.existing_path_count, 1);
        assert_eq!(plan.unmapped_paths.len(), 1);
        assert_eq!(plan.unmapped_paths[0].remote_path, "Z:/missing/project");
        assert_eq!(plan.unmapped_paths[0].suggested_subdirectory, "project");
    }

    #[test]
    fn automatic_mapping_creates_selected_children_in_one_batch() {
        let directory = tempdir().unwrap();
        let codex_home = directory.path().join("codex-home");
        let parent = directory.path().join("projects");
        fs::create_dir_all(&codex_home).unwrap();
        fs::create_dir_all(&parent).unwrap();
        let remote_id = Uuid::now_v7();
        let namespace_id = Uuid::now_v7();
        let store = WorkspaceMappingStore::new(directory.path());

        let result = store
            .create_automatic(
                &codex_home,
                remote_id,
                namespace_id,
                vec![
                    WorkspacePathSelection {
                        remote_path: "D:/work/demo".to_string(),
                        local_path: parent.join("demo").to_string_lossy().into_owned(),
                    },
                    WorkspacePathSelection {
                        remote_path: "E:/personal/demo".to_string(),
                        local_path: parent.join("demo-2").to_string_lossy().into_owned(),
                    },
                ],
            )
            .unwrap();

        assert_eq!(result.state.mappings.len(), 2);
        assert_eq!(result.created_directories.len(), 2);
        assert!(parent.join("demo").is_dir());
        assert!(parent.join("demo-2").is_dir());
        let mapper = store.mapper(&codex_home, remote_id, namespace_id).unwrap();
        assert_ne!(
            mapper.remote_to_local("D:/work/demo"),
            mapper.remote_to_local("E:/personal/demo")
        );
    }
}
