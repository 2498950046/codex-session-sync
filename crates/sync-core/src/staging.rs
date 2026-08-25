//! Pure workspace / baseline / staging planning.
//!
//! A staged set is deliberately a list of *changes*, not a partial revision.
//! Callers must compose it onto a complete remote tree before publishing.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::{ThreadBundle, ThreadRefV4, semantic_thread_hash};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Modified,
    ArchiveChanged,
    Deleted,
    Unchanged,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ChangeCandidate {
    pub thread_id: String,
    pub kind: ChangeKind,
    /// The desired archive state for an archive transition.  It is absent for
    /// additions, content edits, deletions and unchanged threads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived: Option<bool>,
    /// Present for every non-delete local change, and deliberately retained
    /// for deletes so the UI can show the prior title/project after the local
    /// rollout has been physically removed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local: Option<ThreadBundle>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<ThreadBundle>,
    pub local_fingerprint: Option<String>,
    pub baseline_fingerprint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ChangePlan {
    /// Tracking's integrated Head at planning time. `None` means first Push.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_revision_id: Option<String>,
    pub candidates: Vec<ChangeCandidate>,
}

impl ChangePlan {
    pub fn changed(&self) -> impl Iterator<Item = &ChangeCandidate> {
        self.candidates
            .iter()
            .filter(|candidate| candidate.kind != ChangeKind::Unchanged)
    }

    pub fn candidate(&self, thread_id: &str) -> Option<&ChangeCandidate> {
        self.candidates
            .iter()
            .find(|candidate| candidate.thread_id == thread_id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct StagedChangeSet {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_revision_id: Option<String>,
    #[serde(default)]
    pub selected_thread_ids: BTreeSet<String>,
}

impl StagedChangeSet {
    pub fn from_selected<I>(plan: &ChangePlan, selected_thread_ids: I) -> Result<Self>
    where
        I: IntoIterator<Item = String>,
    {
        let selected_thread_ids = selected_thread_ids.into_iter().collect::<BTreeSet<_>>();
        for id in &selected_thread_ids {
            let candidate = plan.candidate(id).ok_or_else(|| {
                anyhow::anyhow!("selected thread is not in the change plan: {id}")
            })?;
            if candidate.kind == ChangeKind::Unchanged {
                bail!("unchanged thread cannot be staged: {id}");
            }
        }
        Ok(Self {
            base_revision_id: plan.base_revision_id.clone(),
            selected_thread_ids,
        })
    }
}

/// Build a semantic change plan.  Both inputs must already have had their
/// rollout content normalized; no file IO occurs here.
pub fn build_change_plan(
    base_revision_id: Option<String>,
    baseline: &[ThreadBundle],
    workspace: &[ThreadBundle],
) -> Result<ChangePlan> {
    let baseline = thread_map(baseline, "baseline")?;
    let workspace = thread_map(workspace, "workspace")?;
    let ids = baseline
        .keys()
        .chain(workspace.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut candidates = Vec::with_capacity(ids.len());

    for thread_id in ids {
        let local = workspace.get(&thread_id).cloned();
        let base = baseline.get(&thread_id).cloned();
        let local_fingerprint = local.as_ref().map(semantic_thread_hash).transpose()?;
        let baseline_fingerprint = base.as_ref().map(semantic_thread_hash).transpose()?;
        let (kind, archived) = match (&base, &local) {
            (None, Some(_)) => (ChangeKind::Added, None),
            (Some(_), None) => (ChangeKind::Deleted, None),
            (Some(base), Some(local)) => {
                if local_fingerprint == baseline_fingerprint {
                    (ChangeKind::Unchanged, None)
                } else if archive_independent_fingerprint(base)?
                    == archive_independent_fingerprint(local)?
                {
                    (ChangeKind::ArchiveChanged, Some(local.archived))
                } else {
                    (ChangeKind::Modified, None)
                }
            }
            (None, None) => unreachable!("union of maps cannot contain an absent thread"),
        };
        candidates.push(ChangeCandidate {
            thread_id,
            kind,
            archived,
            local,
            baseline: base,
            local_fingerprint,
            baseline_fingerprint,
        });
    }
    Ok(ChangePlan {
        base_revision_id,
        candidates,
    })
}

/// Apply the explicitly staged local changes to a complete remote tree.
/// Unstaged changes never influence the output.  This is the guard that
/// prevents a partial Push from turning B/C into remote deletions when only A
/// was checked in the staging dialog.
pub fn compose_staged_thread_set(
    remote: &[ThreadBundle],
    plan: &ChangePlan,
    staged: &StagedChangeSet,
) -> Result<Vec<ThreadBundle>> {
    if staged.base_revision_id != plan.base_revision_id {
        bail!("staged change set was created from a different baseline");
    }
    let mut result = thread_map(remote, "remote")?;
    for thread_id in &staged.selected_thread_ids {
        let candidate = plan.candidate(thread_id).ok_or_else(|| {
            anyhow::anyhow!("staged thread is not in the change plan: {thread_id}")
        })?;
        match candidate.kind {
            ChangeKind::Deleted => {
                result.remove(thread_id);
            }
            ChangeKind::Added | ChangeKind::Modified | ChangeKind::ArchiveChanged => {
                let local = candidate.local.clone().ok_or_else(|| {
                    anyhow::anyhow!("staged local thread is missing: {thread_id}")
                })?;
                result.insert(thread_id.clone(), local);
            }
            ChangeKind::Unchanged => bail!("unchanged thread cannot be staged: {thread_id}"),
        }
    }
    Ok(result.into_values().collect())
}

/// Compose a complete Revision Root's lightweight references without loading
/// descriptors for unselected remote threads. `selected_refs` contains only
/// staged non-delete changes that were materialized locally.
pub fn compose_staged_thread_refs(
    remote: &[ThreadRefV4],
    plan: &ChangePlan,
    staged: &StagedChangeSet,
    selected_refs: &[ThreadRefV4],
) -> Result<Vec<ThreadRefV4>> {
    if staged.base_revision_id != plan.base_revision_id {
        bail!("staged change set was created from a different baseline");
    }
    let mut result = BTreeMap::new();
    for reference in remote {
        if result
            .insert(reference.thread_id.clone(), reference.clone())
            .is_some()
        {
            bail!(
                "remote contains duplicate thread ID {}",
                reference.thread_id
            );
        }
    }
    let selected_refs = selected_refs
        .iter()
        .map(|reference| (reference.thread_id.clone(), reference))
        .collect::<BTreeMap<_, _>>();
    for thread_id in &staged.selected_thread_ids {
        let candidate = plan.candidate(thread_id).ok_or_else(|| {
            anyhow::anyhow!("staged thread is not in the change plan: {thread_id}")
        })?;
        match candidate.kind {
            ChangeKind::Deleted => {
                if selected_refs.contains_key(thread_id) {
                    bail!("a deleted thread must not have a staged descriptor: {thread_id}");
                }
                result.remove(thread_id);
            }
            ChangeKind::Added | ChangeKind::Modified | ChangeKind::ArchiveChanged => {
                let reference = selected_refs
                    .get(thread_id)
                    .ok_or_else(|| anyhow::anyhow!("staged descriptor is missing: {thread_id}"))?;
                result.insert(thread_id.clone(), (*reference).clone());
            }
            ChangeKind::Unchanged => bail!("unchanged thread cannot be staged: {thread_id}"),
        }
    }
    Ok(result.into_values().collect())
}

fn thread_map(threads: &[ThreadBundle], label: &str) -> Result<BTreeMap<String, ThreadBundle>> {
    let mut result = BTreeMap::new();
    for thread in threads {
        if thread.thread_id.trim().is_empty() {
            bail!("{label} contains an empty thread ID");
        }
        if result
            .insert(thread.thread_id.clone(), thread.clone())
            .is_some()
        {
            bail!("{label} contains duplicate thread ID {}", thread.thread_id);
        }
    }
    Ok(result)
}

fn archive_independent_fingerprint(thread: &ThreadBundle) -> Result<String> {
    let mut thread = thread.clone();
    thread.archived = false;
    semantic_thread_hash(&thread)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::{ContentObject, RelatedRecords, WorkspaceRef};

    fn thread(id: &str, content: &str, archived: bool) -> ThreadBundle {
        ThreadBundle {
            schema_version: 1,
            thread_id: id.to_string(),
            title: id.to_string(),
            archived,
            created_at_ms: None,
            updated_at_ms: None,
            model_provider: None,
            workspace: WorkspaceRef::default(),
            rollout: ContentObject {
                sha256: format!("sha256:{}", &content.repeat(64)[..64]),
                byte_length: 1,
                media_type: "application/jsonl".to_string(),
                logical_path: Some(format!("rollout-{id}.jsonl")),
                source_path: Some(PathBuf::from(id)),
                storage: None,
            },
            related_records: RelatedRecords::default(),
            attachments: Vec::new(),
        }
    }

    #[test]
    fn classifies_add_modify_archive_delete_and_unchanged() {
        let base = vec![
            thread("same", "a", false),
            thread("modified", "a", false),
            thread("archive", "a", false),
            thread("deleted", "a", false),
        ];
        let workspace = vec![
            thread("same", "a", false),
            thread("modified", "b", false),
            thread("archive", "a", true),
            thread("added", "a", false),
        ];
        let plan = build_change_plan(Some("sha256:base".to_string()), &base, &workspace).unwrap();
        assert_eq!(plan.candidate("same").unwrap().kind, ChangeKind::Unchanged);
        assert_eq!(
            plan.candidate("modified").unwrap().kind,
            ChangeKind::Modified
        );
        assert_eq!(
            plan.candidate("archive").unwrap().kind,
            ChangeKind::ArchiveChanged
        );
        assert_eq!(plan.candidate("deleted").unwrap().kind, ChangeKind::Deleted);
        assert_eq!(plan.candidate("added").unwrap().kind, ChangeKind::Added);
    }

    #[test]
    fn partial_stage_preserves_unselected_remote_threads() {
        let base = vec![
            thread("a", "a", false),
            thread("b", "a", false),
            thread("c", "a", false),
        ];
        let workspace = vec![thread("a", "b", false), thread("b", "b", false)];
        let remote = base.clone();
        let plan = build_change_plan(Some("sha256:base".to_string()), &base, &workspace).unwrap();
        let staged = StagedChangeSet::from_selected(&plan, vec!["a".to_string()]).unwrap();
        let composed = compose_staged_thread_set(&remote, &plan, &staged).unwrap();
        assert_eq!(
            semantic_thread_hash(composed.iter().find(|t| t.thread_id == "a").unwrap()).unwrap(),
            semantic_thread_hash(&workspace[0]).unwrap()
        );
        assert_eq!(
            semantic_thread_hash(composed.iter().find(|t| t.thread_id == "b").unwrap()).unwrap(),
            semantic_thread_hash(&base[1]).unwrap()
        );
        assert!(composed.iter().any(|thread| thread.thread_id == "c"));
    }

    #[test]
    fn deleted_thread_is_removed_only_when_explicitly_staged() {
        let base = vec![thread("deleted", "a", false), thread("kept", "a", false)];
        let plan = build_change_plan(
            Some("sha256:base".to_string()),
            &base,
            &[thread("kept", "a", false)],
        )
        .unwrap();
        let empty = StagedChangeSet::from_selected(&plan, Vec::new()).unwrap();
        assert_eq!(
            compose_staged_thread_set(&base, &plan, &empty)
                .unwrap()
                .len(),
            2
        );
        let staged = StagedChangeSet::from_selected(&plan, vec!["deleted".to_string()]).unwrap();
        let composed = compose_staged_thread_set(&base, &plan, &staged).unwrap();
        assert_eq!(
            composed
                .iter()
                .map(|t| t.thread_id.as_str())
                .collect::<Vec<_>>(),
            vec!["kept"]
        );
    }

    #[test]
    fn partial_stage_reuses_unselected_remote_references() {
        let base = vec![thread("a", "a", false), thread("b", "a", false)];
        let workspace = vec![thread("a", "b", false), thread("b", "a", false)];
        let plan = build_change_plan(Some("sha256:base".to_string()), &base, &workspace).unwrap();
        let staged = StagedChangeSet::from_selected(&plan, vec!["a".to_string()]).unwrap();
        let remote = vec![
            ThreadRefV4 {
                thread_id: "a".to_string(),
                descriptor_sha256: format!("sha256:{}", "a".repeat(64)),
            },
            ThreadRefV4 {
                thread_id: "b".to_string(),
                descriptor_sha256: format!("sha256:{}", "b".repeat(64)),
            },
        ];
        let selected = vec![ThreadRefV4 {
            thread_id: "a".to_string(),
            descriptor_sha256: format!("sha256:{}", "c".repeat(64)),
        }];
        let composed = compose_staged_thread_refs(&remote, &plan, &staged, &selected).unwrap();
        assert_eq!(composed[0].descriptor_sha256, selected[0].descriptor_sha256);
        assert_eq!(composed[1].descriptor_sha256, remote[1].descriptor_sha256);
    }
}
