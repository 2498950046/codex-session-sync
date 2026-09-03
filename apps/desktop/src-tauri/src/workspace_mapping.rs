use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sync_core::{
    WorkspacePathMapper, WorkspacePathMapping, codex_home_key, scan_codex_home,
    scan_codex_home_workspace_usage,
};
use uuid::Uuid;

const WORKSPACE_MAPPING_SCHEMA_VERSION: u32 = 1;
const MAX_MAPPINGS: usize = 128;
const MAX_CONFIG_BYTES: u64 = 256 * 1024;
const MAX_CLEANUP_SCAN_ENTRIES: usize = 4096;
const MAX_CODEX_GLOBAL_STATE_BYTES: u64 = 16 * 1024 * 1024;
const WORKSPACE_CLEANUP_JOURNAL_SCHEMA_VERSION: u32 = 1;

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
pub struct WorkspaceCleanupCandidate {
    pub path: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum WorkspaceDirectoryState {
    Missing,
    Empty,
    NonEmpty,
    NotDirectory,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspacePathMappingSummary {
    pub id: Uuid,
    pub remote_prefix: String,
    pub local_prefix: String,
    pub inherited: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspacePathEntry {
    pub path: String,
    pub active_count: usize,
    pub archived_count: usize,
    pub mappings: Vec<WorkspacePathMappingSummary>,
    pub codex_project_names: Vec<String>,
    pub directory_state: WorkspaceDirectoryState,
    pub cleanup_eligible: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceCleanupReport {
    pub scanned_roots: Vec<String>,
    pub entries: Vec<WorkspacePathEntry>,
    pub candidates: Vec<WorkspaceCleanupCandidate>,
}

/// Removes only Codex project definitions whose every configured root has no
/// active or archived conversation below it. Workspace directories are never
/// touched by this repair.
pub fn remove_empty_codex_projects(codex_home: &Path) -> Result<usize> {
    let Some(global_state) = load_codex_global_state(codex_home)? else {
        return Ok(0);
    };
    // `threads` database rows can outlive their rollout (including after a
    // failed older cleanup). They are useful diagnostics, but must not keep an
    // otherwise empty sidebar project alive. Only a valid semantic rollout is
    // evidence that a project still contains a conversation.
    let referenced_paths = scan_codex_home(codex_home)?
        .threads
        .into_iter()
        .filter_map(|thread| thread.workspace.source_path)
        .collect::<Vec<_>>();
    let selected_paths = global_state
        .project_paths
        .iter()
        .filter(|project| {
            !referenced_paths
                .iter()
                .any(|path| path_is_same_or_child(path, &project.path))
        })
        .map(|project| normalize_path_for_match(&project.path))
        .collect::<BTreeSet<_>>();
    if selected_paths.is_empty() {
        return Ok(0);
    }

    let mut updated = global_state.value.clone();
    let result = remove_codex_project_paths(&mut updated, &selected_paths)?;
    if !result.changed {
        return Ok(0);
    }
    let bytes = serde_json::to_vec(&updated)?;
    replace_file_if_hash_matches(
        &global_state.path,
        &sha256_bytes(&global_state.bytes),
        &bytes,
    )?;
    Ok(result.removed_projects)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct QuarantinedWorkspaceDirectory {
    pub original_path: String,
    pub quarantine_path: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceCleanupResult {
    pub quarantined: Vec<QuarantinedWorkspaceDirectory>,
    pub removed_codex_projects: usize,
    pub removed_thread_assignments: usize,
    pub backup_path: Option<String>,
    pub journal_path: String,
}

#[derive(Debug, Clone)]
struct CodexProjectPath {
    project_name: String,
    path: String,
}

#[derive(Debug, Clone)]
struct CodexGlobalState {
    path: PathBuf,
    bytes: Vec<u8>,
    value: Value,
    project_paths: Vec<CodexProjectPath>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum WorkspaceCleanupStatus {
    Preparing,
    BackedUp,
    ProjectStateUpdated,
    Completed,
    RolledBack,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceCleanupJournal {
    schema_version: u32,
    operation_id: Uuid,
    codex_home: PathBuf,
    requested_paths: Vec<String>,
    status: WorkspaceCleanupStatus,
    global_state_path: Option<PathBuf>,
    global_state_backup: Option<PathBuf>,
    original_state_sha256: Option<String>,
    updated_state_sha256: Option<String>,
    planned_quarantines: Vec<QuarantinedWorkspaceDirectory>,
    removed_codex_projects: usize,
    removed_thread_assignments: usize,
    created_at: String,
    updated_at: String,
    error: Option<String>,
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

    pub fn cleanup_report(
        &self,
        codex_home: &Path,
        remote_id: Uuid,
        namespace_id: Uuid,
    ) -> Result<WorkspaceCleanupReport> {
        let codex_home_key = codex_home_key(codex_home)?;
        let file = self.load()?;
        let global_state = load_codex_global_state(codex_home)?;
        let workspace_paths = scan_codex_home_workspace_usage(codex_home)?;
        let scoped_mappings = file
            .mappings
            .iter()
            .filter(|mapping| {
                mapping.remote_id == remote_id
                    && mapping.namespace_id == namespace_id
                    && mapping.codex_home_key == codex_home_key
            })
            .collect::<Vec<_>>();
        let mut scan_roots = BTreeMap::<String, PathBuf>::new();
        for path in scoped_mappings
            .iter()
            .map(|mapping| mapping.local_prefix.as_str())
            .chain(global_state.as_ref().into_iter().flat_map(|state| {
                state
                    .project_paths
                    .iter()
                    .map(|project| project.path.as_str())
            }))
        {
            insert_cleanup_scan_root(&mut scan_roots, Path::new(path))?;
        }

        let repository_root = self.repository_root()?;
        let mut protected_paths = file
            .mappings
            .iter()
            .map(|mapping| normalize_path_for_match(&mapping.local_prefix))
            .collect::<BTreeSet<_>>();
        protected_paths.insert(normalize_path_for_match(&codex_home.to_string_lossy()));
        protected_paths.insert(normalize_path_for_match(&repository_root.to_string_lossy()));
        for usage in &workspace_paths {
            protected_paths.insert(normalize_path_for_match(&usage.path));
        }

        let mut entries = BTreeMap::<String, WorkspacePathEntry>::new();
        for mapping in &scoped_mappings {
            workspace_path_entry(&mut entries, &mapping.local_prefix);
        }
        for usage in workspace_paths {
            let entry = workspace_path_entry(&mut entries, &usage.path);
            entry.active_count += usage.active_count;
            entry.archived_count += usage.archived_count;
        }
        let project_path_identities = global_state
            .as_ref()
            .map(|state| {
                state
                    .project_paths
                    .iter()
                    .map(|project| normalize_path_for_match(&project.path))
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default();
        if let Some(state) = &global_state {
            for project in &state.project_paths {
                let entry = workspace_path_entry(&mut entries, &project.path);
                if !entry.codex_project_names.contains(&project.project_name) {
                    entry.codex_project_names.push(project.project_name.clone());
                }
            }
        }

        for root in scan_roots.values() {
            let mut entry_count = 0_usize;
            for entry in fs::read_dir(root)
                .with_context(|| format!("failed to inspect workspace parent {}", root.display()))?
            {
                entry_count += 1;
                if entry_count > MAX_CLEANUP_SCAN_ENTRIES {
                    bail!(
                        "workspace parent {} exceeds the {MAX_CLEANUP_SCAN_ENTRIES} entry safety limit",
                        root.display()
                    );
                }
                let entry = entry?;
                let metadata = fs::symlink_metadata(entry.path())?;
                if !metadata.is_dir() || is_symlink_or_reparse_point(&metadata) {
                    continue;
                }
                let path = fs::canonicalize(entry.path())?;
                if path.parent() != Some(root.as_path()) || !directory_is_empty(&path)? {
                    continue;
                }
                workspace_path_entry(&mut entries, &display_workspace_path(&path));
            }
        }

        for (identity, entry) in &mut entries {
            for mapping in &scoped_mappings {
                let mapping_identity = normalize_path_for_match(&mapping.local_prefix);
                if path_is_same_or_child(identity, &mapping_identity) {
                    entry.mappings.push(WorkspacePathMappingSummary {
                        id: mapping.id,
                        remote_prefix: mapping.remote_prefix.clone(),
                        local_prefix: mapping.local_prefix.clone(),
                        inherited: identity != &mapping_identity,
                    });
                }
            }
            entry.directory_state = workspace_directory_state(Path::new(&entry.path))?;
            let protected = protected_paths
                .iter()
                .any(|protected| paths_overlap(identity, protected));
            let overlaps_other_project = project_path_identities
                .iter()
                .any(|project| project != identity && paths_overlap(identity, project));
            let removable_directory = entry.directory_state == WorkspaceDirectoryState::Empty;
            let removable_missing_project = entry.directory_state
                == WorkspaceDirectoryState::Missing
                && !entry.codex_project_names.is_empty();
            entry.cleanup_eligible = entry.active_count == 0
                && entry.archived_count == 0
                && entry.mappings.is_empty()
                && !protected
                && !overlaps_other_project
                && (removable_directory || removable_missing_project);
        }
        let candidates = entries
            .values()
            .filter(|entry| entry.cleanup_eligible)
            .map(|entry| WorkspaceCleanupCandidate {
                path: entry.path.clone(),
            })
            .collect();

        Ok(WorkspaceCleanupReport {
            scanned_roots: scan_roots
                .into_values()
                .map(|path| display_workspace_path(&path))
                .collect(),
            entries: entries.into_values().collect(),
            candidates,
        })
    }

    pub fn quarantine_empty_directories(
        &self,
        codex_home: &Path,
        remote_id: Uuid,
        namespace_id: Uuid,
        requested_paths: Vec<String>,
    ) -> Result<WorkspaceCleanupResult> {
        if requested_paths.is_empty() {
            bail!("at least one workspace cleanup path is required");
        }
        if requested_paths.len() > MAX_MAPPINGS {
            bail!("workspace cleanup exceeds the {MAX_MAPPINGS} path safety limit");
        }

        let repository_root = self.repository_root()?;
        recover_incomplete_workspace_cleanups(&repository_root, codex_home)?;
        let report = self.cleanup_report(codex_home, remote_id, namespace_id)?;
        let available = report
            .entries
            .into_iter()
            .filter(|entry| entry.cleanup_eligible)
            .map(|entry| (normalize_path_for_match(&entry.path), entry))
            .collect::<BTreeMap<_, _>>();
        let mut selected = Vec::<WorkspacePathEntry>::with_capacity(requested_paths.len());
        let mut seen = BTreeSet::new();
        for requested in requested_paths {
            let normalized = normalize_path_for_match(requested.trim());
            if !seen.insert(normalized.clone()) {
                bail!("workspace cleanup request contains duplicate paths");
            }
            let entry = available.get(&normalized).with_context(|| {
                format!(
                    "workspace cleanup path is no longer an unreferenced empty or missing Codex project: {}",
                    requested.trim()
                )
            })?;
            selected.push(entry.clone());
        }

        let operation_id = Uuid::now_v7();
        let quarantine_root = repository_root.join("quarantine").join("empty-workspaces");
        let mut planned_quarantines = Vec::new();
        for (index, entry) in selected.iter().enumerate() {
            if entry.directory_state != WorkspaceDirectoryState::Empty {
                continue;
            }
            let original = PathBuf::from(&entry.path);
            let name = original
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("workspace");
            let quarantine = quarantine_root.join(format!("{operation_id}-{index}-{name}"));
            planned_quarantines.push(QuarantinedWorkspaceDirectory {
                original_path: display_workspace_path(&original),
                quarantine_path: display_workspace_path(&quarantine),
            });
        }

        let selected_identities = selected
            .iter()
            .map(|entry| normalize_path_for_match(&entry.path))
            .collect::<BTreeSet<_>>();
        let global_state = load_codex_global_state(codex_home)?;
        let mut updated_state = None;
        let mut removed_codex_projects = 0;
        let mut removed_thread_assignments = 0;
        if let Some(state) = &global_state {
            let mut value = state.value.clone();
            let result = remove_codex_project_paths(&mut value, &selected_identities)?;
            removed_codex_projects = result.removed_projects;
            removed_thread_assignments = result.removed_assignments;
            if result.changed {
                updated_state = Some(serde_json::to_vec(&value)?);
            }
        }

        if planned_quarantines.is_empty() && updated_state.is_none() {
            bail!("workspace cleanup no longer has any directory or Codex project state to change");
        }

        let journal_dir = repository_root.join("journal");
        let backup_dir = repository_root
            .join("backups")
            .join(format!("workspace-cleanup-{operation_id}"));
        let journal_path = journal_dir.join(format!("workspace-cleanup-{operation_id}.json"));
        let global_state_backup = global_state
            .as_ref()
            .filter(|_| updated_state.is_some())
            .map(|_| backup_dir.join("codex-global-state.json"));
        let now = Utc::now().to_rfc3339();
        let mut journal = WorkspaceCleanupJournal {
            schema_version: WORKSPACE_CLEANUP_JOURNAL_SCHEMA_VERSION,
            operation_id,
            codex_home: codex_home.to_path_buf(),
            requested_paths: selected.iter().map(|entry| entry.path.clone()).collect(),
            status: WorkspaceCleanupStatus::Preparing,
            global_state_path: global_state.as_ref().map(|state| state.path.clone()),
            global_state_backup: global_state_backup.clone(),
            original_state_sha256: global_state
                .as_ref()
                .filter(|_| updated_state.is_some())
                .map(|state| sha256_bytes(&state.bytes)),
            updated_state_sha256: updated_state.as_ref().map(|bytes| sha256_bytes(bytes)),
            planned_quarantines: planned_quarantines.clone(),
            removed_codex_projects,
            removed_thread_assignments,
            created_at: now.clone(),
            updated_at: now,
            error: None,
        };
        write_workspace_cleanup_journal(&journal_path, &journal)?;

        let result = (|| -> Result<()> {
            if let (Some(state), Some(backup)) = (&global_state, &global_state_backup) {
                write_new_file(backup, &state.bytes)?;
            }
            journal.status = WorkspaceCleanupStatus::BackedUp;
            journal.updated_at = Utc::now().to_rfc3339();
            write_workspace_cleanup_journal(&journal_path, &journal)?;

            if let (Some(state), Some(bytes)) = (&global_state, &updated_state) {
                replace_file_if_hash_matches(
                    &state.path,
                    journal.original_state_sha256.as_deref().unwrap_or_default(),
                    bytes,
                )?;
            }
            journal.status = WorkspaceCleanupStatus::ProjectStateUpdated;
            journal.updated_at = Utc::now().to_rfc3339();
            write_workspace_cleanup_journal(&journal_path, &journal)?;

            fs::create_dir_all(&quarantine_root).with_context(|| {
                format!(
                    "failed to create workspace quarantine {}",
                    quarantine_root.display()
                )
            })?;
            for planned in &planned_quarantines {
                move_empty_directory(
                    Path::new(&planned.original_path),
                    Path::new(&planned.quarantine_path),
                )?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            journal.error = Some(format!("{error:#}"));
            if let Err(rollback_error) = rollback_workspace_cleanup(&journal_path, &mut journal) {
                return Err(error.context(format!(
                    "workspace cleanup rollback also failed: {rollback_error:#}"
                )));
            }
            return Err(error);
        }

        journal.status = WorkspaceCleanupStatus::Completed;
        journal.updated_at = Utc::now().to_rfc3339();
        write_workspace_cleanup_journal(&journal_path, &journal)?;
        Ok(WorkspaceCleanupResult {
            quarantined: planned_quarantines,
            removed_codex_projects,
            removed_thread_assignments,
            backup_path: global_state_backup.map(|path| display_workspace_path(&path)),
            journal_path: display_workspace_path(&journal_path),
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
        let mut file: WorkspaceMappingFile = serde_json::from_reader(BufReader::new(file))
            .context("failed to parse workspace mappings")?;
        validate_file(&file)?;
        deduplicate_legacy_verbatim_remote_rules(&mut file.mappings);
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

    fn repository_root(&self) -> Result<PathBuf> {
        self.path
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .context("workspace mapping path has no repository root")
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

fn insert_cleanup_scan_root(
    scan_roots: &mut BTreeMap<String, PathBuf>,
    workspace_path: &Path,
) -> Result<()> {
    let Some(parent) = workspace_path.parent() else {
        return Ok(());
    };
    if parent.as_os_str().is_empty() || parent.parent().is_none() || !parent.is_dir() {
        return Ok(());
    }
    let root = fs::canonicalize(parent)
        .with_context(|| format!("failed to normalize workspace parent {}", parent.display()))?;
    scan_roots
        .entry(normalize_path_for_match(&root.to_string_lossy()))
        .or_insert(root);
    Ok(())
}

fn workspace_path_entry<'a>(
    entries: &'a mut BTreeMap<String, WorkspacePathEntry>,
    path: &str,
) -> &'a mut WorkspacePathEntry {
    let display = display_workspace_path(Path::new(path.trim()));
    let identity = normalize_path_for_match(&display);
    entries
        .entry(identity)
        .or_insert_with(|| WorkspacePathEntry {
            path: display,
            active_count: 0,
            archived_count: 0,
            mappings: Vec::new(),
            codex_project_names: Vec::new(),
            directory_state: WorkspaceDirectoryState::Missing,
            cleanup_eligible: false,
        })
}

fn workspace_directory_state(path: &Path) -> Result<WorkspaceDirectoryState> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Ok(WorkspaceDirectoryState::Missing);
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect workspace path {}", path.display()));
        }
    };
    if !metadata.is_dir() || is_symlink_or_reparse_point(&metadata) {
        return Ok(WorkspaceDirectoryState::NotDirectory);
    }
    if directory_is_empty(path)? {
        Ok(WorkspaceDirectoryState::Empty)
    } else {
        Ok(WorkspaceDirectoryState::NonEmpty)
    }
}

fn load_codex_global_state(codex_home: &Path) -> Result<Option<CodexGlobalState>> {
    let path = codex_home.join(".codex-global-state.json");
    let metadata = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to inspect Codex global state {}", path.display())
            });
        }
    };
    if !metadata.is_file() {
        bail!(
            "Codex global state is not a regular file: {}",
            path.display()
        );
    }
    if metadata.len() > MAX_CODEX_GLOBAL_STATE_BYTES {
        bail!("Codex global state exceeds the {MAX_CODEX_GLOBAL_STATE_BYTES} byte safety limit");
    }
    let bytes = fs::read(&path)
        .with_context(|| format!("failed to read Codex global state {}", path.display()))?;
    let value: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse Codex global state {}", path.display()))?;
    let object = value
        .as_object()
        .context("Codex global state is not a JSON object")?;
    let mut project_paths = Vec::new();
    if let Some(projects) = object.get("local-projects") {
        let projects = projects
            .as_object()
            .context("Codex global state local-projects field is not an object")?;
        for (project_id, project) in projects {
            let project = project
                .as_object()
                .with_context(|| format!("Codex project {project_id} is not an object"))?;
            let name = project
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(project_id);
            let Some(root_paths) = project.get("rootPaths") else {
                continue;
            };
            let root_paths = root_paths
                .as_array()
                .with_context(|| format!("Codex project {project_id} rootPaths is not an array"))?;
            for root in root_paths {
                let root = root.as_str().with_context(|| {
                    format!("Codex project {project_id} contains a non-string root path")
                })?;
                if !root.trim().is_empty() {
                    project_paths.push(CodexProjectPath {
                        project_name: name.to_string(),
                        path: display_workspace_path(Path::new(root)),
                    });
                }
            }
        }
    }
    Ok(Some(CodexGlobalState {
        path,
        bytes,
        value,
        project_paths,
    }))
}

#[derive(Debug, Default, PartialEq, Eq)]
struct CodexProjectCleanupResult {
    changed: bool,
    removed_projects: usize,
    removed_assignments: usize,
}

fn remove_codex_project_paths(
    state: &mut Value,
    selected_paths: &BTreeSet<String>,
) -> Result<CodexProjectCleanupResult> {
    let state = state
        .as_object_mut()
        .context("Codex global state is not a JSON object")?;
    let mut result = CodexProjectCleanupResult::default();
    let mut removed_project_ids = BTreeSet::new();

    if let Some(projects) = state.get_mut("local-projects") {
        let projects = projects
            .as_object_mut()
            .context("Codex global state local-projects field is not an object")?;
        let project_ids = projects.keys().cloned().collect::<Vec<_>>();
        for project_id in project_ids {
            let project = projects
                .get_mut(&project_id)
                .and_then(Value::as_object_mut)
                .with_context(|| format!("Codex project {project_id} is not an object"))?;
            let Some(root_paths) = project.get_mut("rootPaths") else {
                continue;
            };
            let root_paths = root_paths
                .as_array_mut()
                .with_context(|| format!("Codex project {project_id} rootPaths is not an array"))?;
            let before = root_paths.len();
            root_paths.retain(|path| {
                path.as_str()
                    .is_none_or(|path| !selected_paths.contains(&normalize_path_for_match(path)))
            });
            result.changed |= root_paths.len() != before;
            if before > 0 && root_paths.is_empty() {
                removed_project_ids.insert(project_id);
            }
        }
        for project_id in &removed_project_ids {
            projects.remove(project_id);
        }
    }
    result.removed_projects = removed_project_ids.len();

    for key in ["active-workspace-roots", "electron-saved-workspace-roots"] {
        if let Some(paths) = state.get_mut(key) {
            let paths = paths
                .as_array_mut()
                .with_context(|| format!("Codex global state {key} field is not an array"))?;
            let before = paths.len();
            paths.retain(|path| {
                path.as_str()
                    .is_none_or(|path| !selected_paths.contains(&normalize_path_for_match(path)))
            });
            result.changed |= paths.len() != before;
        }
    }
    if let Some(order) = state.get_mut("project-order") {
        let order = order
            .as_array_mut()
            .context("Codex global state project-order field is not an array")?;
        let before = order.len();
        order.retain(|project_id| {
            project_id
                .as_str()
                .is_none_or(|project_id| !removed_project_ids.contains(project_id))
        });
        result.changed |= order.len() != before;
    }
    if state
        .get("selected-project")
        .and_then(Value::as_object)
        .and_then(|selected| selected.get("projectId"))
        .and_then(Value::as_str)
        .is_some_and(|project_id| removed_project_ids.contains(project_id))
    {
        state.remove("selected-project");
        result.changed = true;
    }
    if let Some(assignments) = state.get_mut("thread-project-assignments") {
        let assignments = assignments
            .as_object_mut()
            .context("Codex global state thread-project-assignments field is not an object")?;
        let before = assignments.len();
        assignments.retain(|_, assignment| {
            let project_removed = assignment
                .get("projectId")
                .and_then(Value::as_str)
                .is_some_and(|project_id| removed_project_ids.contains(project_id));
            let path_removed = assignment
                .get("cwd")
                .and_then(Value::as_str)
                .is_some_and(|path| {
                    selected_paths
                        .iter()
                        .any(|selected| path_is_same_or_child(path, selected))
                });
            !project_removed && !path_removed
        });
        result.removed_assignments = before - assignments.len();
        result.changed |= result.removed_assignments > 0;
    }
    if let Some(hints) = state.get_mut("thread-workspace-root-hints") {
        let hints = hints
            .as_object_mut()
            .context("Codex global state thread-workspace-root-hints field is not an object")?;
        let before = hints.len();
        hints.retain(|_, path| {
            path.as_str().is_none_or(|path| {
                !selected_paths
                    .iter()
                    .any(|selected| path_is_same_or_child(path, selected))
            })
        });
        result.changed |= hints.len() != before;
    }
    if let Some(writable_roots) = state.get_mut("thread-writable-roots") {
        let writable_roots = writable_roots
            .as_object_mut()
            .context("Codex global state thread-writable-roots field is not an object")?;
        for roots in writable_roots.values_mut() {
            let roots = roots
                .as_array_mut()
                .context("Codex thread writable roots entry is not an array")?;
            let before = roots.len();
            roots.retain(|path| {
                path.as_str().is_none_or(|path| {
                    !selected_paths
                        .iter()
                        .any(|selected| path_is_same_or_child(path, selected))
                })
            });
            result.changed |= roots.len() != before;
        }
    }
    Ok(result)
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

fn write_new_file(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .with_context(|| format!("failed to create backup file {}", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn replace_file_if_hash_matches(path: &Path, expected_sha256: &str, bytes: &[u8]) -> Result<()> {
    let live = fs::read(path)
        .with_context(|| format!("failed to read file before replacement {}", path.display()))?;
    let actual = sha256_bytes(&live);
    if actual != expected_sha256 {
        bail!(
            "file changed while workspace cleanup was preparing: {}",
            path.display()
        );
    }
    let temporary = path.with_file_name(format!(
        ".{}.workspace-cleanup-{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("codex-global-state"),
        Uuid::now_v7()
    ));
    write_new_file(&temporary, bytes)?;
    fs::remove_file(path)?;
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error).with_context(|| format!("failed to replace {}", path.display()));
    }
    let installed = fs::read(path)?;
    if sha256_bytes(&installed) != sha256_bytes(bytes) {
        bail!("workspace cleanup replacement failed validation");
    }
    Ok(())
}

fn write_workspace_cleanup_journal(path: &Path, journal: &WorkspaceCleanupJournal) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary =
        path.with_file_name(format!(".workspace-cleanup-journal-{}.tmp", Uuid::now_v7()));
    write_new_file(&temporary, &serde_json::to_vec_pretty(journal)?)?;
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(&temporary, path)?;
    Ok(())
}

fn recover_incomplete_workspace_cleanups(repository_root: &Path, codex_home: &Path) -> Result<()> {
    let journal_dir = repository_root.join("journal");
    let entries = match fs::read_dir(&journal_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("failed to inspect workspace cleanup journals"),
    };
    let target_home = normalize_path_for_match(&codex_home.to_string_lossy());
    for entry in entries {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with("workspace-cleanup-") || !name.ends_with(".json") {
            continue;
        }
        let mut journal: WorkspaceCleanupJournal =
            serde_json::from_reader(BufReader::new(File::open(&path)?)).with_context(|| {
                format!(
                    "failed to read workspace cleanup journal {}",
                    path.display()
                )
            })?;
        if journal.schema_version != WORKSPACE_CLEANUP_JOURNAL_SCHEMA_VERSION {
            bail!("unsupported workspace cleanup journal schema version");
        }
        if normalize_path_for_match(&journal.codex_home.to_string_lossy()) != target_home
            || matches!(
                journal.status,
                WorkspaceCleanupStatus::Completed | WorkspaceCleanupStatus::RolledBack
            )
        {
            continue;
        }
        rollback_workspace_cleanup(&path, &mut journal)?;
    }
    Ok(())
}

fn rollback_workspace_cleanup(
    journal_path: &Path,
    journal: &mut WorkspaceCleanupJournal,
) -> Result<()> {
    for planned in journal.planned_quarantines.iter().rev() {
        let original = Path::new(&planned.original_path);
        let quarantine = Path::new(&planned.quarantine_path);
        if quarantine.exists() && !original.exists() {
            if let Some(parent) = original.parent() {
                fs::create_dir_all(parent).with_context(|| {
                    format!(
                        "failed to recreate workspace parent during rollback {}",
                        parent.display()
                    )
                })?;
            }
            move_empty_directory(quarantine, original)?;
        } else if quarantine.exists() && original.exists() {
            bail!(
                "cannot roll back workspace cleanup because both paths exist: {} and {}",
                original.display(),
                quarantine.display()
            );
        }
    }
    if let (Some(live), Some(backup), Some(original_hash), Some(updated_hash)) = (
        journal.global_state_path.as_deref(),
        journal.global_state_backup.as_deref(),
        journal.original_state_sha256.as_deref(),
        journal.updated_state_sha256.as_deref(),
    ) {
        let backup_bytes = fs::read(backup).with_context(|| {
            format!(
                "failed to read workspace cleanup backup {}",
                backup.display()
            )
        })?;
        if sha256_bytes(&backup_bytes) != original_hash {
            bail!("workspace cleanup global-state backup failed hash validation");
        }
        match fs::read(live) {
            Ok(live_bytes) if sha256_bytes(&live_bytes) == original_hash => {}
            Ok(live_bytes) if sha256_bytes(&live_bytes) == updated_hash => {
                replace_file_if_hash_matches(live, updated_hash, &backup_bytes)?;
            }
            Ok(_) => bail!(
                "Codex global state changed after workspace cleanup; refusing rollback overwrite"
            ),
            Err(error) if error.kind() == ErrorKind::NotFound => {
                write_new_file(live, &backup_bytes)?;
            }
            Err(error) => return Err(error).context("failed to inspect Codex global state"),
        }
    }
    journal.status = WorkspaceCleanupStatus::RolledBack;
    journal.updated_at = Utc::now().to_rfc3339();
    write_workspace_cleanup_journal(journal_path, journal)?;
    Ok(())
}

fn paths_overlap(left: &str, right: &str) -> bool {
    path_is_same_or_child(left, right) || path_is_same_or_child(right, left)
}

fn directory_is_empty(path: &Path) -> Result<bool> {
    Ok(fs::read_dir(path)?.next().transpose()?.is_none())
}

fn display_workspace_path(path: &Path) -> String {
    let path = path.to_string_lossy();
    #[cfg(windows)]
    {
        if let Some(unc) = path.strip_prefix(r"\\?\UNC\") {
            return format!(r"\\{unc}");
        }
        if let Some(regular) = path.strip_prefix(r"\\?\") {
            return regular.to_string();
        }
    }
    path.into_owned()
}

fn is_symlink_or_reparse_point(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn move_empty_directory(source: &Path, destination: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source)
        .with_context(|| format!("failed to inspect empty workspace {}", source.display()))?;
    if !metadata.is_dir() || is_symlink_or_reparse_point(&metadata) {
        bail!("workspace cleanup accepts only regular non-symlink directories");
    }
    if !directory_is_empty(source)? {
        bail!(
            "workspace directory {} is no longer empty; cleanup was refused",
            source.display()
        );
    }
    match fs::rename(source, destination) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::CrossesDevices => {
            fs::create_dir(destination).with_context(|| {
                format!(
                    "failed to create quarantined workspace {}",
                    destination.display()
                )
            })?;
            if !directory_is_empty(source)? {
                let _ = fs::remove_dir(destination);
                bail!(
                    "workspace directory {} changed during cleanup; cleanup was refused",
                    source.display()
                );
            }
            if let Err(error) = fs::remove_dir(source) {
                let _ = fs::remove_dir(destination);
                return Err(error).with_context(|| {
                    format!("failed to quarantine empty workspace {}", source.display())
                });
            }
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to move empty workspace {} to {}",
                    source.display(),
                    destination.display()
                )
            });
        }
    }
    if !directory_is_empty(destination)? {
        let _ = fs::rename(destination, source);
        bail!("quarantined workspace changed during cleanup; the original was restored");
    }
    Ok(())
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
    sync_core::normalize_workspace_path_for_match(path)
}

fn normalize_path_for_legacy_match(path: &str) -> String {
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

fn deduplicate_legacy_verbatim_remote_rules(mappings: &mut Vec<WorkspaceMappingRule>) {
    type ScopePath = (Uuid, Uuid, String, String);

    let mut legacy_variants = BTreeMap::<ScopePath, BTreeSet<String>>::new();
    for mapping in mappings.iter() {
        legacy_variants
            .entry((
                mapping.remote_id,
                mapping.namespace_id,
                mapping.codex_home_key.clone(),
                normalize_path_for_match(&mapping.remote_prefix),
            ))
            .or_default()
            .insert(normalize_path_for_legacy_match(&mapping.remote_prefix));
    }
    let collapsible = legacy_variants
        .into_iter()
        .filter_map(|(key, variants)| (variants.len() > 1).then_some(key))
        .collect::<BTreeSet<_>>();
    let mut seen = BTreeSet::new();
    mappings.retain(|mapping| {
        let key = (
            mapping.remote_id,
            mapping.namespace_id,
            mapping.codex_home_key.clone(),
            normalize_path_for_match(&mapping.remote_prefix),
        );
        !collapsible.contains(&key) || seen.insert(key)
    });
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
    for mapping in &file.mappings {
        validate_rule(mapping)?;
        if !ids.insert(mapping.id) {
            bail!("workspace mapping file contains duplicate rule IDs");
        }
    }
    let mut effective_mappings = file.mappings.clone();
    deduplicate_legacy_verbatim_remote_rules(&mut effective_mappings);
    let mut scopes = BTreeMap::<(Uuid, Uuid, String), Vec<WorkspacePathMapping>>::new();
    for mapping in &effective_mappings {
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
    use rusqlite::Connection;
    use serde_json::json;
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn empty_project_repair_removes_only_unreferenced_project_definitions() {
        let directory = tempdir().unwrap();
        let codex_home = directory.path().join("codex-home");
        fs::create_dir_all(&codex_home).unwrap();
        let empty_root = directory.path().join("work/empty");
        let referenced_root = directory.path().join("work/referenced");
        let sessions = codex_home.join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(
            sessions.join("rollout-active-thread.jsonl"),
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"active-thread\",\"cwd\":\"{}\"}}}}\n",
                referenced_root.to_string_lossy().replace('\\', "\\\\")
            ),
        )
        .unwrap();
        let database = Connection::open(codex_home.join("state_5.sqlite")).unwrap();
        database
            .execute_batch(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, cwd TEXT, archived INTEGER);
                 INSERT INTO threads VALUES ('stale-thread', 'placeholder', 0);",
            )
            .unwrap();
        database
            .execute(
                "UPDATE threads SET cwd = ?1 WHERE id = 'stale-thread'",
                [empty_root.to_string_lossy().as_ref()],
            )
            .unwrap();
        drop(database);

        let state = json!({
            "local-projects": {
                "empty": { "id": "empty", "name": "empty", "rootPaths": [empty_root] },
                "referenced": { "id": "referenced", "name": "referenced", "rootPaths": [referenced_root] }
            },
            "project-order": ["empty", "referenced"],
            "selected-project": { "type": "local", "projectId": "empty" },
            "thread-project-assignments": {
                "stale-thread": { "projectId": "empty", "cwd": empty_root },
                "active-thread": { "projectId": "referenced", "cwd": referenced_root }
            }
        });
        fs::write(
            codex_home.join(".codex-global-state.json"),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();

        assert_eq!(remove_empty_codex_projects(&codex_home).unwrap(), 1);

        let updated: Value =
            serde_json::from_slice(&fs::read(codex_home.join(".codex-global-state.json")).unwrap())
                .unwrap();
        assert!(updated["local-projects"].get("empty").is_none());
        assert!(updated["local-projects"].get("referenced").is_some());
        assert_eq!(updated["project-order"], json!(["referenced"]));
        assert!(updated.get("selected-project").is_none());
        assert!(
            updated["thread-project-assignments"]
                .get("stale-thread")
                .is_none()
        );
        assert!(
            updated["thread-project-assignments"]
                .get("active-thread")
                .is_some()
        );
    }

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
    fn pull_plan_coalesces_windows_verbatim_and_regular_drive_paths() {
        let directory = tempdir().unwrap();
        let codex_home = directory.path().join("codex-home");
        fs::create_dir_all(&codex_home).unwrap();
        let store = WorkspaceMappingStore::new(directory.path());

        let plan = store
            .pull_plan(
                &codex_home,
                Uuid::now_v7(),
                Uuid::now_v7(),
                None,
                vec![
                    r"\\?\Z:\missing\cpa".to_string(),
                    "Z:/missing/cpa".to_string(),
                ],
            )
            .unwrap();

        assert_eq!(plan.unmapped_paths.len(), 1);
        assert_eq!(plan.unmapped_paths[0].suggested_subdirectory, "cpa");
    }

    #[test]
    fn legacy_verbatim_duplicate_rules_keep_the_first_mapping() {
        let directory = tempdir().unwrap();
        let codex_home = directory.path().join("codex-home");
        let first_target = directory.path().join("cpa");
        let duplicate_target = directory.path().join("cpa-2");
        fs::create_dir_all(&codex_home).unwrap();
        fs::create_dir_all(&first_target).unwrap();
        fs::create_dir_all(&duplicate_target).unwrap();
        let remote_id = Uuid::now_v7();
        let namespace_id = Uuid::now_v7();
        let codex_home_key = codex_home_key(&codex_home).unwrap();
        let store = WorkspaceMappingStore::new(directory.path());
        let file = WorkspaceMappingFile {
            schema_version: WORKSPACE_MAPPING_SCHEMA_VERSION,
            mappings: vec![
                WorkspaceMappingRule {
                    id: Uuid::now_v7(),
                    remote_id,
                    namespace_id,
                    codex_home_key: codex_home_key.clone(),
                    remote_prefix: r"\\?\C:\Users\jyh\Documents\Codex\cpa".to_string(),
                    local_prefix: first_target.to_string_lossy().into_owned(),
                    created_at: "2026-07-27T00:00:00Z".to_string(),
                    updated_at: "2026-07-27T00:00:00Z".to_string(),
                },
                WorkspaceMappingRule {
                    id: Uuid::now_v7(),
                    remote_id,
                    namespace_id,
                    codex_home_key: codex_home_key.clone(),
                    remote_prefix: "C:/Users/jyh/Documents/Codex/cpa".to_string(),
                    local_prefix: duplicate_target.to_string_lossy().into_owned(),
                    created_at: "2026-07-28T00:00:00Z".to_string(),
                    updated_at: "2026-07-28T00:00:00Z".to_string(),
                },
            ],
        };
        store.save(&file).unwrap();

        let state = store.state(&codex_home, remote_id, namespace_id).unwrap();
        assert_eq!(state.mappings.len(), 1);
        assert_eq!(
            state.mappings[0].local_prefix,
            first_target.to_string_lossy()
        );
        let mapper = store.mapper(&codex_home, remote_id, namespace_id).unwrap();
        assert_eq!(
            mapper.remote_to_local("C:/Users/jyh/Documents/Codex/cpa"),
            Some(first_target.to_string_lossy().replace('\\', "/"))
        );
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

    #[test]
    fn cleanup_finds_only_unmapped_unreferenced_empty_directories_and_quarantines_them() {
        let directory = tempdir().unwrap();
        let repository = directory.path().join("repository");
        let codex_home = directory.path().join("codex-home");
        let history = directory.path().join("history");
        let mapped = history.join("mapped");
        let referenced = history.join("referenced");
        let stale = history.join("stale");
        let nonempty = history.join("nonempty");
        for path in [
            &repository,
            &codex_home,
            &mapped,
            &referenced,
            &stale,
            &nonempty,
        ] {
            fs::create_dir_all(path).unwrap();
        }
        fs::write(nonempty.join("keep.txt"), b"keep").unwrap();
        let database = Connection::open(codex_home.join("state_5.sqlite")).unwrap();
        database
            .execute(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, cwd TEXT, archived INTEGER)",
                [],
            )
            .unwrap();
        database
            .execute(
                "INSERT INTO threads (id, cwd, archived) VALUES ('referenced', ?1, 1)",
                [referenced.to_string_lossy().as_ref()],
            )
            .unwrap();
        drop(database);

        let remote_id = Uuid::now_v7();
        let namespace_id = Uuid::now_v7();
        let store = WorkspaceMappingStore::new(&repository);
        store
            .create(
                &codex_home,
                remote_id,
                namespace_id,
                "D:/mapped".to_string(),
                mapped.to_string_lossy().into_owned(),
            )
            .unwrap();

        let report = store
            .cleanup_report(&codex_home, remote_id, namespace_id)
            .unwrap();
        let referenced_entry = report
            .entries
            .iter()
            .find(|entry| {
                normalize_path_for_match(&entry.path)
                    == normalize_path_for_match(&referenced.to_string_lossy())
            })
            .unwrap();
        assert_eq!(referenced_entry.active_count, 0);
        assert_eq!(referenced_entry.archived_count, 1);
        assert!(!referenced_entry.cleanup_eligible);
        assert_eq!(report.candidates.len(), 1);
        assert_eq!(
            normalize_path_for_match(&report.candidates[0].path),
            normalize_path_for_match(&stale.to_string_lossy())
        );

        let result = store
            .quarantine_empty_directories(
                &codex_home,
                remote_id,
                namespace_id,
                vec![report.candidates[0].path.clone()],
            )
            .unwrap();
        assert_eq!(result.quarantined.len(), 1);
        assert!(!stale.exists());
        assert!(Path::new(&result.quarantined[0].quarantine_path).is_dir());
        assert!(mapped.is_dir());
        assert!(referenced.is_dir());
        assert!(nonempty.is_dir());
        assert!(
            store
                .cleanup_report(&codex_home, remote_id, namespace_id)
                .unwrap()
                .candidates
                .is_empty()
        );
    }

    #[test]
    fn cleanup_revalidates_a_directory_that_changed_after_inspection() {
        let directory = tempdir().unwrap();
        let repository = directory.path().join("repository");
        let codex_home = directory.path().join("codex-home");
        let history = directory.path().join("history");
        let mapped = history.join("mapped");
        let stale = history.join("stale");
        for path in [&repository, &codex_home, &mapped, &stale] {
            fs::create_dir_all(path).unwrap();
        }
        let remote_id = Uuid::now_v7();
        let namespace_id = Uuid::now_v7();
        let store = WorkspaceMappingStore::new(&repository);
        store
            .create(
                &codex_home,
                remote_id,
                namespace_id,
                "D:/mapped".to_string(),
                mapped.to_string_lossy().into_owned(),
            )
            .unwrap();
        let report = store
            .cleanup_report(&codex_home, remote_id, namespace_id)
            .unwrap();
        assert_eq!(report.candidates.len(), 1);
        fs::write(stale.join("appeared.txt"), b"changed").unwrap();

        let error = store
            .quarantine_empty_directories(
                &codex_home,
                remote_id,
                namespace_id,
                vec![report.candidates[0].path.clone()],
            )
            .unwrap_err();
        assert!(error.to_string().contains("no longer"));
        assert!(stale.join("appeared.txt").is_file());
    }

    #[test]
    fn cleanup_report_marks_child_paths_as_inheriting_a_parent_mapping() {
        let directory = tempdir().unwrap();
        let repository = directory.path().join("repository");
        let codex_home = directory.path().join("codex-home");
        let local_root = directory.path().join("history/yaxin");
        let missing_child = local_root.join("data-platform");
        for path in [&repository, &codex_home, &local_root] {
            fs::create_dir_all(path).unwrap();
        }
        let database = Connection::open(codex_home.join("state_5.sqlite")).unwrap();
        database
            .execute(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, cwd TEXT, archived INTEGER)",
                [],
            )
            .unwrap();
        database
            .execute(
                "INSERT INTO threads (id, cwd, archived) VALUES ('active', ?1, 0)",
                [missing_child.to_string_lossy().as_ref()],
            )
            .unwrap();
        drop(database);

        let remote_id = Uuid::now_v7();
        let namespace_id = Uuid::now_v7();
        let store = WorkspaceMappingStore::new(&repository);
        store
            .create(
                &codex_home,
                remote_id,
                namespace_id,
                "D:/yaxin".to_string(),
                local_root.to_string_lossy().into_owned(),
            )
            .unwrap();

        let report = store
            .cleanup_report(&codex_home, remote_id, namespace_id)
            .unwrap();
        let root_entry = report
            .entries
            .iter()
            .find(|entry| {
                normalize_path_for_match(&entry.path)
                    == normalize_path_for_match(&local_root.to_string_lossy())
            })
            .unwrap();
        assert_eq!(root_entry.mappings.len(), 1);
        assert!(!root_entry.mappings[0].inherited);

        let child_entry = report
            .entries
            .iter()
            .find(|entry| {
                normalize_path_for_match(&entry.path)
                    == normalize_path_for_match(&missing_child.to_string_lossy())
            })
            .unwrap();
        assert_eq!(child_entry.active_count, 1);
        assert_eq!(
            child_entry.directory_state,
            WorkspaceDirectoryState::Missing
        );
        assert_eq!(child_entry.mappings.len(), 1);
        assert_eq!(child_entry.mappings[0].remote_prefix, "D:/yaxin");
        assert_eq!(
            normalize_path_for_match(&child_entry.mappings[0].local_prefix),
            normalize_path_for_match(&local_root.to_string_lossy())
        );
        assert!(child_entry.mappings[0].inherited);
        assert!(!child_entry.cleanup_eligible);
    }

    #[test]
    fn cleanup_aggregates_mappings_sessions_projects_and_removes_stale_codex_menu_state() {
        let directory = tempdir().unwrap();
        let repository = directory.path().join("repository");
        let codex_home = directory.path().join("codex-home");
        let history = directory.path().join("history");
        let referenced = history.join("cpa");
        let stale = history.join("cpa-3");
        let missing = history.join("do-c-2");
        for path in [&repository, &codex_home, &referenced, &stale] {
            fs::create_dir_all(path).unwrap();
        }
        let database = Connection::open(codex_home.join("state_5.sqlite")).unwrap();
        database
            .execute(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, cwd TEXT, archived INTEGER)",
                [],
            )
            .unwrap();
        database
            .execute(
                "INSERT INTO threads (id, cwd, archived) VALUES
                    ('active', ?1, 0),
                    ('archived', ?1, 1)",
                [referenced.to_string_lossy().as_ref()],
            )
            .unwrap();
        drop(database);
        let global_state_path = codex_home.join(".codex-global-state.json");
        fs::write(
            &global_state_path,
            serde_json::to_vec(&json!({
                "active-workspace-roots": [stale],
                "electron-saved-workspace-roots": [referenced, stale, missing],
                "local-projects": {
                    "project-cpa": { "id": "project-cpa", "name": "cpa", "rootPaths": [referenced] },
                    "project-stale": { "id": "project-stale", "name": "cpa-3", "rootPaths": [stale] },
                    "project-missing": { "id": "project-missing", "name": "do-c-2", "rootPaths": [missing] }
                },
                "project-order": ["project-cpa", "project-stale", "project-missing"],
                "selected-project": { "type": "local", "projectId": "project-stale" },
                "thread-project-assignments": {
                    "stale-thread": { "projectId": "project-stale", "cwd": stale },
                    "missing-thread": { "projectId": "project-missing", "cwd": missing }
                },
                "thread-workspace-root-hints": {
                    "stale-thread": stale,
                    "missing-thread": missing
                },
                "thread-writable-roots": {
                    "stale-thread": [stale],
                    "missing-thread": [missing]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let remote_id = Uuid::now_v7();
        let namespace_id = Uuid::now_v7();
        let store = WorkspaceMappingStore::new(&repository);
        store
            .create(
                &codex_home,
                remote_id,
                namespace_id,
                "D:/projects/cpa".to_string(),
                referenced.to_string_lossy().into_owned(),
            )
            .unwrap();

        let report = store
            .cleanup_report(&codex_home, remote_id, namespace_id)
            .unwrap();
        let referenced_entry = report
            .entries
            .iter()
            .find(|entry| path_is_same_or_child(&entry.path, &referenced.to_string_lossy()))
            .unwrap();
        assert_eq!(referenced_entry.active_count, 1);
        assert_eq!(referenced_entry.archived_count, 1);
        assert_eq!(referenced_entry.mappings.len(), 1);
        assert_eq!(referenced_entry.codex_project_names, vec!["cpa"]);
        assert!(!referenced_entry.cleanup_eligible);
        assert_eq!(report.candidates.len(), 2);
        assert!(report.candidates.iter().any(|candidate| {
            normalize_path_for_match(&candidate.path)
                == normalize_path_for_match(&stale.to_string_lossy())
        }));
        assert!(report.candidates.iter().any(|candidate| {
            normalize_path_for_match(&candidate.path)
                == normalize_path_for_match(&missing.to_string_lossy())
        }));

        let result = store
            .quarantine_empty_directories(
                &codex_home,
                remote_id,
                namespace_id,
                report
                    .candidates
                    .iter()
                    .map(|candidate| candidate.path.clone())
                    .collect(),
            )
            .unwrap();
        assert_eq!(result.quarantined.len(), 1);
        assert_eq!(result.removed_codex_projects, 2);
        assert_eq!(result.removed_thread_assignments, 2);
        assert!(
            result
                .backup_path
                .as_ref()
                .is_some_and(|path| Path::new(path).is_file())
        );
        assert!(Path::new(&result.journal_path).is_file());
        assert!(!stale.exists());
        assert!(!missing.exists());

        let state: Value = serde_json::from_slice(&fs::read(global_state_path).unwrap()).unwrap();
        let projects = state["local-projects"].as_object().unwrap();
        assert_eq!(projects.len(), 1);
        assert!(projects.contains_key("project-cpa"));
        assert_eq!(state["project-order"], json!(["project-cpa"]));
        assert_eq!(state["electron-saved-workspace-roots"], json!([referenced]));
        assert!(state.get("selected-project").is_none());
        assert!(
            state["thread-project-assignments"]
                .as_object()
                .unwrap()
                .is_empty()
        );
        assert!(
            state["thread-workspace-root-hints"]
                .as_object()
                .unwrap()
                .is_empty()
        );
        assert!(
            state["thread-writable-roots"]["stale-thread"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .cleanup_report(&codex_home, remote_id, namespace_id)
                .unwrap()
                .candidates
                .is_empty()
        );
    }

    #[test]
    fn project_state_cleanup_preserves_a_multi_root_project_with_an_unselected_root() {
        let mut state = json!({
            "local-projects": {
                "multi": { "rootPaths": ["F:/history/stale", "F:/history/keep"] }
            },
            "project-order": ["multi"],
            "thread-project-assignments": {},
            "electron-saved-workspace-roots": ["F:/history/stale", "F:/history/keep"]
        });
        let selected = BTreeSet::from([normalize_path_for_match("F:/history/stale")]);

        let result = remove_codex_project_paths(&mut state, &selected).unwrap();

        assert!(result.changed);
        assert_eq!(result.removed_projects, 0);
        assert_eq!(
            state["local-projects"]["multi"]["rootPaths"],
            json!(["F:/history/keep"])
        );
        assert_eq!(state["project-order"], json!(["multi"]));
    }

    #[test]
    fn incomplete_workspace_cleanup_restores_project_state_and_quarantined_directory() {
        let directory = tempdir().unwrap();
        let repository = directory.path().join("repository");
        let codex_home = directory.path().join("codex-home");
        let original = directory.path().join("history/stale");
        let quarantine = repository.join("quarantine/empty-workspaces/stale");
        fs::create_dir_all(&codex_home).unwrap();
        fs::create_dir_all(&quarantine).unwrap();
        let live = codex_home.join(".codex-global-state.json");
        let original_bytes = serde_json::to_vec(&json!({
            "local-projects": { "stale": { "rootPaths": [original] } }
        }))
        .unwrap();
        let updated_bytes = serde_json::to_vec(&json!({ "local-projects": {} })).unwrap();
        fs::write(&live, &updated_bytes).unwrap();
        let operation_id = Uuid::now_v7();
        let backup = repository
            .join("backups")
            .join(format!("workspace-cleanup-{operation_id}"))
            .join("codex-global-state.json");
        write_new_file(&backup, &original_bytes).unwrap();
        let journal_path = repository
            .join("journal")
            .join(format!("workspace-cleanup-{operation_id}.json"));
        let now = Utc::now().to_rfc3339();
        write_workspace_cleanup_journal(
            &journal_path,
            &WorkspaceCleanupJournal {
                schema_version: WORKSPACE_CLEANUP_JOURNAL_SCHEMA_VERSION,
                operation_id,
                codex_home: codex_home.clone(),
                requested_paths: vec![display_workspace_path(&original)],
                status: WorkspaceCleanupStatus::ProjectStateUpdated,
                global_state_path: Some(live.clone()),
                global_state_backup: Some(backup),
                original_state_sha256: Some(sha256_bytes(&original_bytes)),
                updated_state_sha256: Some(sha256_bytes(&updated_bytes)),
                planned_quarantines: vec![QuarantinedWorkspaceDirectory {
                    original_path: display_workspace_path(&original),
                    quarantine_path: display_workspace_path(&quarantine),
                }],
                removed_codex_projects: 1,
                removed_thread_assignments: 0,
                created_at: now.clone(),
                updated_at: now,
                error: None,
            },
        )
        .unwrap();

        recover_incomplete_workspace_cleanups(&repository, &codex_home).unwrap();

        assert_eq!(fs::read(live).unwrap(), original_bytes);
        assert!(original.is_dir());
        assert!(!quarantine.exists());
        let journal: WorkspaceCleanupJournal =
            serde_json::from_reader(File::open(journal_path).unwrap()).unwrap();
        assert_eq!(journal.status, WorkspaceCleanupStatus::RolledBack);
    }
}
