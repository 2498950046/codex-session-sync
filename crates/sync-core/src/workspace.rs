use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{LocalSnapshot, ThreadBundle};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspacePathMapping {
    pub remote_prefix: String,
    pub local_prefix: String,
}

#[derive(Debug, Clone, Default)]
pub struct WorkspacePathMapper {
    mappings: Vec<WorkspacePathMapping>,
}

impl WorkspacePathMapper {
    pub fn new(mut mappings: Vec<WorkspacePathMapping>) -> Result<Self> {
        for mapping in &mut mappings {
            mapping.remote_prefix = normalize_prefix(&mapping.remote_prefix)?;
            mapping.local_prefix = normalize_prefix(&mapping.local_prefix)?;
        }
        reject_ambiguous_mappings(&mappings, true)?;
        reject_ambiguous_mappings(&mappings, false)?;
        Ok(Self { mappings })
    }

    pub fn is_empty(&self) -> bool {
        self.mappings.is_empty()
    }

    pub fn materialize_snapshot(&self, snapshot: &LocalSnapshot) -> LocalSnapshot {
        let mut snapshot = snapshot.clone();
        for thread in &mut snapshot.threads {
            self.materialize_thread(thread);
        }
        snapshot
    }

    pub fn canonicalize_snapshot(&self, snapshot: &LocalSnapshot) -> LocalSnapshot {
        let mut snapshot = snapshot.clone();
        for thread in &mut snapshot.threads {
            self.canonicalize_thread(thread);
        }
        snapshot
    }

    pub fn materialize_thread(&self, thread: &mut ThreadBundle) {
        map_thread_paths(thread, |path| self.remote_to_local(path));
    }

    pub fn canonicalize_thread(&self, thread: &mut ThreadBundle) {
        map_thread_paths(thread, |path| self.local_to_remote(path));
    }

    pub fn remote_to_local(&self, path: &str) -> Option<String> {
        self.map_path(path, true)
    }

    pub fn local_to_remote(&self, path: &str) -> Option<String> {
        self.map_path(path, false)
    }

    fn map_path(&self, path: &str, remote_to_local: bool) -> Option<String> {
        self.mappings
            .iter()
            .filter_map(|mapping| {
                let (from, to) = if remote_to_local {
                    (&mapping.remote_prefix, &mapping.local_prefix)
                } else {
                    (&mapping.local_prefix, &mapping.remote_prefix)
                };
                replace_prefix(path, from, to)
                    .map(|mapped| (normalize_for_match(from).chars().count(), mapped))
            })
            .max_by_key(|(prefix_length, _)| *prefix_length)
            .map(|(_, mapped)| mapped)
    }
}

fn map_thread_paths(thread: &mut ThreadBundle, mapper: impl Fn(&str) -> Option<String>) {
    if let Some(path) = thread.workspace.source_path.as_deref()
        && let Some(mapped) = mapper(path)
    {
        thread.workspace.source_path = Some(mapped);
    }
    if let Some(rows) = thread.related_records.tables.get_mut("threads") {
        for row in rows {
            if let Some(row) = row.as_object_mut()
                && let Some(path) = row.get("cwd").and_then(Value::as_str)
                && let Some(mapped) = mapper(path)
            {
                row.insert("cwd".to_string(), Value::String(mapped));
            }
        }
    }
}

fn normalize_prefix(prefix: &str) -> Result<String> {
    let prefix = prefix.trim();
    if prefix.is_empty() {
        bail!("workspace path mapping prefixes must not be empty");
    }
    let normalized = prefix.replace('\\', "/");
    let trimmed = normalized.trim_end_matches('/');
    if trimmed.is_empty() {
        bail!("workspace path mapping prefixes must not be filesystem roots");
    }
    Ok(trimmed.to_string())
}

fn reject_ambiguous_mappings(mappings: &[WorkspacePathMapping], remote_side: bool) -> Result<()> {
    for (index, left) in mappings.iter().enumerate() {
        let left = if remote_side {
            &left.remote_prefix
        } else {
            &left.local_prefix
        };
        for right in &mappings[index + 1..] {
            let right = if remote_side {
                &right.remote_prefix
            } else {
                &right.local_prefix
            };
            if paths_equal(left, right) {
                bail!("workspace path mappings contain a duplicate prefix: {left}");
            }
        }
    }
    Ok(())
}

fn replace_prefix(path: &str, from: &str, to: &str) -> Option<String> {
    let normalized_path = path.replace('\\', "/");
    let match_path = normalize_for_match(&normalized_path);
    let match_from = normalize_for_match(from);
    if match_path != match_from
        && !match_path
            .strip_prefix(&match_from)
            .is_some_and(|suffix| suffix.starts_with('/'))
    {
        return None;
    }
    let suffix = &normalized_path[from.len().min(normalized_path.len())..];
    let separator = if to.contains('\\') && !to.contains('/') {
        '\\'
    } else {
        '/'
    };
    let suffix = suffix
        .trim_start_matches(['/', '\\'])
        .replace(['/', '\\'], &separator.to_string());
    if suffix.is_empty() {
        Some(to.to_string())
    } else {
        Some(format!(
            "{}{separator}{suffix}",
            to.trim_end_matches(['/', '\\'])
        ))
    }
}

fn paths_equal(left: &str, right: &str) -> bool {
    normalize_for_match(left) == normalize_for_match(right)
}

fn normalize_for_match(path: &str) -> String {
    let normalized = path.replace('\\', "/").trim_end_matches('/').to_string();
    if looks_like_windows_path(&normalized) {
        normalized.to_ascii_lowercase()
    } else {
        normalized
    }
}

fn looks_like_windows_path(path: &str) -> bool {
    path.starts_with("//") || path.as_bytes().get(1).is_some_and(|value| *value == b':')
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use serde_json::json;

    use super::*;
    use crate::{
        ContentObject, LOCAL_SNAPSHOT_SCHEMA_VERSION, RelatedRecords, THREAD_BUNDLE_SCHEMA_VERSION,
        WorkspaceRef,
    };

    #[test]
    fn mapping_is_bidirectional_and_uses_component_boundaries() {
        let mapper = WorkspacePathMapper::new(vec![WorkspacePathMapping {
            remote_prefix: "D:\\projects".to_string(),
            local_prefix: "F:\\workspace".to_string(),
        }])
        .unwrap();
        assert_eq!(
            mapper.remote_to_local("d:/projects/demo"),
            Some("F:/workspace/demo".to_string())
        );
        assert_eq!(
            mapper.local_to_remote("F:\\workspace\\demo"),
            Some("D:/projects/demo".to_string())
        );
        assert_eq!(mapper.remote_to_local("D:/projects-old/demo"), None);
    }

    #[test]
    fn snapshot_round_trip_preserves_remote_semantics() {
        let mapper = WorkspacePathMapper::new(vec![WorkspacePathMapping {
            remote_prefix: "D:/projects".to_string(),
            local_prefix: "F:/workspace".to_string(),
        }])
        .unwrap();
        let snapshot = fixture_snapshot("D:/projects/demo");
        let local = mapper.materialize_snapshot(&snapshot);
        assert_eq!(
            local.threads[0].workspace.source_path.as_deref(),
            Some("F:/workspace/demo")
        );
        assert_eq!(
            local.threads[0].related_records.tables["threads"][0]["cwd"],
            json!("F:/workspace/demo")
        );
        assert_eq!(mapper.canonicalize_snapshot(&local), snapshot);
    }

    #[test]
    fn duplicate_reverse_prefix_is_rejected() {
        let error = WorkspacePathMapper::new(vec![
            WorkspacePathMapping {
                remote_prefix: "D:/one".to_string(),
                local_prefix: "F:/shared".to_string(),
            },
            WorkspacePathMapping {
                remote_prefix: "D:/two".to_string(),
                local_prefix: "f:/shared".to_string(),
            },
        ])
        .unwrap_err();
        assert!(error.to_string().contains("duplicate prefix"));
    }

    fn fixture_snapshot(cwd: &str) -> LocalSnapshot {
        LocalSnapshot {
            schema_version: LOCAL_SNAPSHOT_SCHEMA_VERSION,
            snapshot_id: "snapshot".to_string(),
            created_at: "2026-07-28T00:00:00Z".to_string(),
            threads: vec![ThreadBundle {
                schema_version: THREAD_BUNDLE_SCHEMA_VERSION,
                thread_id: "thread".to_string(),
                title: "Thread".to_string(),
                archived: false,
                created_at_ms: Some(1_700_000_000_000),
                updated_at_ms: Some(1_700_000_100_000),
                model_provider: Some("openai".to_string()),
                workspace: WorkspaceRef {
                    logical_id: None,
                    source_path: Some(cwd.to_string()),
                },
                rollout: ContentObject {
                    sha256:
                        "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                            .to_string(),
                    byte_length: 0,
                    media_type: "application/x-ndjson".to_string(),
                    logical_path: Some("sessions/rollout-thread.jsonl".to_string()),
                    source_path: Some(PathBuf::from("rollout-thread.jsonl")),
                },
                related_records: RelatedRecords {
                    source_database: None,
                    tables: BTreeMap::from([(
                        "threads".to_string(),
                        vec![json!({"id": "thread", "cwd": cwd})],
                    )]),
                },
                attachments: Vec::new(),
            }],
            warning_count: 0,
        }
    }
}
