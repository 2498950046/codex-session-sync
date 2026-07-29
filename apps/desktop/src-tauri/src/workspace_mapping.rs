use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sync_core::{
    WorkspacePathMapper, WorkspacePathMapping, WorkspacePathUsage, codex_home_key,
    scan_codex_home_workspace_usage,
};
use uuid::Uuid;

const WORKSPACE_MAPPING_SCHEMA_VERSION: u32 = 1;
const MAX_MAPPINGS: usize = 128;
const MAX_CONFIG_BYTES: u64 = 256 * 1024;
const MAX_CLEANUP_SCAN_ENTRIES: usize = 4096;

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
pub struct WorkspaceCleanupReport {
    pub scanned_roots: Vec<String>,
    pub workspace_paths: Vec<WorkspacePathUsage>,
    pub candidates: Vec<WorkspaceCleanupCandidate>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct QuarantinedWorkspaceDirectory {
    pub original_path: String,
    pub quarantine_path: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceCleanupResult {
    pub quarantined: Vec<QuarantinedWorkspaceDirectory>,
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
        let mut scan_roots = BTreeMap::<String, PathBuf>::new();
        for mapping in file.mappings.iter().filter(|mapping| {
            mapping.remote_id == remote_id
                && mapping.namespace_id == namespace_id
                && mapping.codex_home_key == codex_home_key
        }) {
            let Some(parent) = Path::new(&mapping.local_prefix).parent() else {
                continue;
            };
            if parent.as_os_str().is_empty() || parent.parent().is_none() || !parent.is_dir() {
                continue;
            }
            let root = fs::canonicalize(parent).with_context(|| {
                format!("failed to normalize workspace parent {}", parent.display())
            })?;
            scan_roots
                .entry(normalize_path_for_match(&root.to_string_lossy()))
                .or_insert(root);
        }

        let repository_root = self.repository_root()?;
        let workspace_paths = scan_codex_home_workspace_usage(codex_home)?;
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

        let mut candidates = BTreeMap::<String, WorkspaceCleanupCandidate>::new();
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
                let normalized = normalize_path_for_match(&path.to_string_lossy());
                if protected_paths
                    .iter()
                    .any(|protected| paths_overlap(&normalized, protected))
                {
                    continue;
                }
                candidates.insert(
                    normalized,
                    WorkspaceCleanupCandidate {
                        path: display_workspace_path(&path),
                    },
                );
            }
        }

        Ok(WorkspaceCleanupReport {
            scanned_roots: scan_roots
                .into_values()
                .map(|path| display_workspace_path(&path))
                .collect(),
            workspace_paths,
            candidates: candidates.into_values().collect(),
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

        let report = self.cleanup_report(codex_home, remote_id, namespace_id)?;
        let available = report
            .candidates
            .into_iter()
            .map(|candidate| (normalize_path_for_match(&candidate.path), candidate.path))
            .collect::<BTreeMap<_, _>>();
        let mut selected = Vec::with_capacity(requested_paths.len());
        let mut seen = BTreeSet::new();
        for requested in requested_paths {
            let normalized = normalize_path_for_match(requested.trim());
            if !seen.insert(normalized.clone()) {
                bail!("workspace cleanup request contains duplicate paths");
            }
            let path = available.get(&normalized).with_context(|| {
                format!(
                    "workspace cleanup path is no longer an unreferenced empty directory: {}",
                    requested.trim()
                )
            })?;
            selected.push(PathBuf::from(path));
        }

        let quarantine_root = self
            .repository_root()?
            .join("quarantine")
            .join("empty-workspaces");
        fs::create_dir_all(&quarantine_root).with_context(|| {
            format!(
                "failed to create workspace quarantine {}",
                quarantine_root.display()
            )
        })?;
        let mut quarantined = Vec::<QuarantinedWorkspaceDirectory>::new();
        for original in selected {
            let name = original
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("workspace");
            let quarantine = quarantine_root.join(format!("{}-{name}", Uuid::now_v7()));
            if let Err(error) = move_empty_directory(&original, &quarantine) {
                for moved in quarantined.iter().rev() {
                    let _ = move_empty_directory(
                        Path::new(&moved.quarantine_path),
                        Path::new(&moved.original_path),
                    );
                }
                return Err(error);
            }
            quarantined.push(QuarantinedWorkspaceDirectory {
                original_path: display_workspace_path(&original),
                quarantine_path: display_workspace_path(&quarantine),
            });
        }
        Ok(WorkspaceCleanupResult { quarantined })
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
        assert_eq!(report.workspace_paths.len(), 1);
        assert_eq!(report.workspace_paths[0].active_count, 0);
        assert_eq!(report.workspace_paths[0].archived_count, 1);
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
}
