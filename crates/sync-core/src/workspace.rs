use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::ops::Range;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::local::{
    install_prepared_repository_object, repository_object_path, validate_repository_object,
};
use crate::{LocalSnapshot, ObjectDescriptor, OperationControl, OperationProgress, ThreadBundle};

const MAX_SESSION_META_BYTES: u64 = 1024 * 1024;

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

    pub fn materialize_snapshot_objects(
        &self,
        snapshot: &LocalSnapshot,
        repository_root: &Path,
        control: &OperationControl,
    ) -> Result<LocalSnapshot> {
        self.transform_snapshot_objects(snapshot, repository_root, control, true)
    }

    pub fn canonicalize_snapshot_objects(
        &self,
        snapshot: &LocalSnapshot,
        repository_root: &Path,
        control: &OperationControl,
    ) -> Result<LocalSnapshot> {
        self.transform_snapshot_objects(snapshot, repository_root, control, false)
    }

    pub fn materialize_snapshot_objects_with_reference(
        &self,
        snapshot: &LocalSnapshot,
        canonical_reference: &[ThreadBundle],
        repository_root: &Path,
        control: &OperationControl,
    ) -> Result<LocalSnapshot> {
        let mut reference_paths = BTreeMap::new();
        for thread in canonical_reference {
            let Some(path) = thread_workspace_path(thread) else {
                continue;
            };
            if reference_paths
                .insert(thread.thread_id.as_str(), path)
                .is_some()
            {
                bail!("duplicate reference thread ID {}", thread.thread_id);
            }
        }

        let mut canonical = snapshot.clone();
        for thread in &mut canonical.threads {
            if let Some(path) = reference_paths.get(thread.thread_id.as_str()) {
                set_thread_workspace_path(thread, path);
            }
        }
        self.materialize_snapshot_objects(&canonical, repository_root, control)
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

    fn transform_snapshot_objects(
        &self,
        snapshot: &LocalSnapshot,
        repository_root: &Path,
        control: &OperationControl,
        remote_to_local: bool,
    ) -> Result<LocalSnapshot> {
        let mut transformed = if remote_to_local {
            self.materialize_snapshot(snapshot)
        } else {
            self.canonicalize_snapshot(snapshot)
        };
        for (index, (original, thread)) in snapshot
            .threads
            .iter()
            .zip(&mut transformed.threads)
            .enumerate()
        {
            control.check_cancelled()?;
            let original_path = thread_workspace_path(original);
            let target_path = thread_workspace_path(thread);
            if original_path == target_path {
                continue;
            }
            let target_path = target_path.with_context(|| {
                format!(
                    "thread {} workspace mapping removed its target path",
                    thread.thread_id
                )
            })?;
            let target_path = target_path.to_string();
            let thread_id = thread.thread_id.clone();
            control.report(OperationProgress {
                phase: if remote_to_local {
                    "materialize_workspace_paths".to_string()
                } else {
                    "canonicalize_workspace_paths".to_string()
                },
                message: thread.title.clone(),
                completed: index as u64,
                total: Some(snapshot.threads.len() as u64),
                unit: "threads".to_string(),
                cancellable: true,
            });
            rewrite_rollout_session_meta_cwd(
                &mut thread.rollout,
                &thread_id,
                &target_path,
                repository_root,
                control,
            )?;
        }
        Ok(transformed)
    }
}

fn thread_workspace_path(thread: &ThreadBundle) -> Option<&str> {
    thread.workspace.source_path.as_deref().or_else(|| {
        thread
            .related_records
            .tables
            .get("threads")?
            .first()?
            .get("cwd")?
            .as_str()
    })
}

fn set_thread_workspace_path(thread: &mut ThreadBundle, path: &str) {
    thread.workspace.source_path = Some(path.to_string());
    if let Some(rows) = thread.related_records.tables.get_mut("threads") {
        for row in rows {
            let Some(row) = row.as_object_mut() else {
                continue;
            };
            if row.contains_key("cwd") {
                row.insert("cwd".to_string(), Value::String(path.to_string()));
            }
        }
    }
}

fn rewrite_rollout_session_meta_cwd(
    object: &mut crate::ContentObject,
    thread_id: &str,
    target_cwd: &str,
    repository_root: &Path,
    control: &OperationControl,
) -> Result<()> {
    let source = repository_object_path(repository_root, &object.sha256)?;
    let input = File::open(&source)
        .with_context(|| format!("failed to open rollout object {}", source.display()))?;
    let mut reader = BufReader::new(input);
    let mut first_line = Vec::new();
    {
        let mut limited = reader.by_ref().take(MAX_SESSION_META_BYTES + 1);
        limited.read_until(b'\n', &mut first_line)?;
    }
    if first_line.is_empty() || first_line.len() as u64 > MAX_SESSION_META_BYTES {
        bail!("thread {thread_id} rollout session metadata is missing or oversized");
    }
    let rewritten_first_line = rewrite_session_meta_cwd(&first_line, thread_id, target_cwd)?;
    if rewritten_first_line == first_line {
        validate_repository_object(
            repository_root,
            &ObjectDescriptor {
                sha256: object.sha256.clone(),
                byte_length: object.byte_length,
            },
        )?;
        return Ok(());
    }

    fs::create_dir_all(repository_root.join("objects").join("tmp"))?;
    let temporary = repository_root
        .join("objects")
        .join("tmp")
        .join(format!("{}.workspace-object.tmp", Uuid::now_v7()));
    let transform_result = (|| -> Result<ObjectDescriptor> {
        let output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        let mut writer = BufWriter::new(output);
        let mut source_hasher = Sha256::new();
        let mut output_hasher = Sha256::new();
        let mut source_length = first_line.len() as u64;
        let mut output_length = rewritten_first_line.len() as u64;
        source_hasher.update(&first_line);
        output_hasher.update(&rewritten_first_line);
        writer.write_all(&rewritten_first_line)?;

        let mut buffer = [0_u8; 64 * 1024];
        loop {
            control.check_cancelled()?;
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            source_length = source_length
                .checked_add(count as u64)
                .context("source rollout length overflow")?;
            output_length = output_length
                .checked_add(count as u64)
                .context("rewritten rollout length overflow")?;
            source_hasher.update(&buffer[..count]);
            output_hasher.update(&buffer[..count]);
            writer.write_all(&buffer[..count])?;
        }
        writer.flush()?;
        writer.get_ref().sync_all()?;
        drop(writer);

        let source_sha256 = format!("sha256:{}", hex::encode(source_hasher.finalize()));
        if source_length != object.byte_length || source_sha256 != object.sha256 {
            bail!(
                "source rollout object changed while rewriting thread {thread_id}: expected {} ({} bytes), got {} ({} bytes)",
                object.sha256,
                object.byte_length,
                source_sha256,
                source_length
            );
        }
        Ok(ObjectDescriptor {
            sha256: format!("sha256:{}", hex::encode(output_hasher.finalize())),
            byte_length: output_length,
        })
    })();
    let descriptor = match transform_result {
        Ok(descriptor) => descriptor,
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
    };
    if let Err(error) = install_prepared_repository_object(repository_root, &temporary, &descriptor)
    {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    object.sha256 = descriptor.sha256;
    object.byte_length = descriptor.byte_length;
    object.source_path = None;
    Ok(())
}

fn rewrite_session_meta_cwd(line: &[u8], thread_id: &str, target_cwd: &str) -> Result<Vec<u8>> {
    let body_end = if line.ends_with(b"\r\n") {
        line.len() - 2
    } else if line.ends_with(b"\n") {
        line.len() - 1
    } else {
        line.len()
    };
    let body = &line[..body_end];
    let mut json_start = 0;
    skip_json_whitespace(body, &mut json_start);
    if body.get(json_start..json_start + 3) == Some(&[0xef, 0xbb, 0xbf]) {
        json_start += 3;
        skip_json_whitespace(body, &mut json_start);
    }
    let value: Value = serde_json::from_slice(&body[json_start..])
        .context("failed to parse rollout session metadata")?;
    if value.get("type").and_then(Value::as_str) != Some("session_meta") {
        bail!("first rollout record is not session_meta");
    }
    let payload = value
        .get("payload")
        .and_then(Value::as_object)
        .context("rollout session_meta payload is not an object")?;
    if payload.get("id").and_then(Value::as_str) != Some(thread_id) {
        bail!("rollout session_meta thread ID does not match {thread_id}");
    }
    if payload.get("cwd").and_then(Value::as_str) == Some(target_cwd) {
        return Ok(line.to_vec());
    }

    let top_object = body[json_start..]
        .iter()
        .position(|byte| *byte == b'{')
        .map(|offset| json_start + offset)
        .context("rollout session metadata is not a JSON object")?;
    let payload_range = find_object_field_value(body, top_object, "payload")?
        .context("rollout session metadata has no payload field")?;
    if body.get(payload_range.start) != Some(&b'{') {
        bail!("rollout session_meta payload is not a JSON object");
    }
    let cwd_range = find_object_field_value(body, payload_range.start, "cwd")?
        .context("rollout session_meta payload has no cwd field")?;
    let replacement = serde_json::to_vec(target_cwd)?;
    let mut rewritten = Vec::with_capacity(line.len() + replacement.len());
    rewritten.extend_from_slice(&body[..cwd_range.start]);
    rewritten.extend_from_slice(&replacement);
    rewritten.extend_from_slice(&body[cwd_range.end..]);
    rewritten.extend_from_slice(&line[body_end..]);
    Ok(rewritten)
}

fn find_object_field_value(
    input: &[u8],
    object_start: usize,
    target_field: &str,
) -> Result<Option<Range<usize>>> {
    if input.get(object_start) != Some(&b'{') {
        bail!("JSON object does not start with an opening brace");
    }
    let mut index = object_start + 1;
    loop {
        skip_json_whitespace(input, &mut index);
        match input.get(index) {
            Some(b'}') => return Ok(None),
            Some(b'"') => {}
            _ => bail!("invalid JSON object member"),
        }
        let key_start = index;
        let key_end = skip_json_string(input, index)?;
        let key: String = serde_json::from_slice(&input[key_start..key_end])?;
        index = key_end;
        skip_json_whitespace(input, &mut index);
        if input.get(index) != Some(&b':') {
            bail!("invalid JSON object member separator");
        }
        index += 1;
        skip_json_whitespace(input, &mut index);
        let value_start = index;
        let value_end = skip_json_value(input, index)?;
        if key == target_field {
            return Ok(Some(value_start..value_end));
        }
        index = value_end;
        skip_json_whitespace(input, &mut index);
        match input.get(index) {
            Some(b',') => index += 1,
            Some(b'}') => return Ok(None),
            _ => bail!("invalid JSON object terminator"),
        }
    }
}

fn skip_json_whitespace(input: &[u8], index: &mut usize) {
    while input.get(*index).is_some_and(u8::is_ascii_whitespace) {
        *index += 1;
    }
}

fn skip_json_string(input: &[u8], start: usize) -> Result<usize> {
    if input.get(start) != Some(&b'"') {
        bail!("JSON string does not start with a quote");
    }
    let mut index = start + 1;
    while let Some(byte) = input.get(index) {
        match byte {
            b'"' => return Ok(index + 1),
            b'\\' => index += 2,
            _ => index += 1,
        }
    }
    bail!("unterminated JSON string")
}

fn skip_json_value(input: &[u8], start: usize) -> Result<usize> {
    match input.get(start) {
        Some(b'"') => skip_json_string(input, start),
        Some(b'{') | Some(b'[') => {
            let mut stack = vec![input[start]];
            let mut index = start + 1;
            while let Some(byte) = input.get(index) {
                match byte {
                    b'"' => index = skip_json_string(input, index)?,
                    b'{' | b'[' => {
                        stack.push(*byte);
                        index += 1;
                    }
                    b'}' => {
                        if stack.pop() != Some(b'{') {
                            bail!("mismatched JSON object delimiter");
                        }
                        index += 1;
                        if stack.is_empty() {
                            return Ok(index);
                        }
                    }
                    b']' => {
                        if stack.pop() != Some(b'[') {
                            bail!("mismatched JSON array delimiter");
                        }
                        index += 1;
                        if stack.is_empty() {
                            return Ok(index);
                        }
                    }
                    _ => index += 1,
                }
            }
            bail!("unterminated JSON container")
        }
        Some(_) => {
            let mut index = start;
            while input.get(index).is_some_and(|byte| {
                !matches!(byte, b',' | b'}' | b']') && !byte.is_ascii_whitespace()
            }) {
                index += 1;
            }
            if index == start {
                bail!("empty JSON value");
            }
            Ok(index)
        }
        None => bail!("missing JSON value"),
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
    use std::io::Cursor;
    use std::path::PathBuf;

    use serde_json::json;
    use tempfile::tempdir;

    use super::*;
    use crate::{
        ContentObject, LOCAL_SNAPSHOT_SCHEMA_VERSION, RelatedRecords, THREAD_BUNDLE_SCHEMA_VERSION,
        WorkspaceRef, install_repository_object,
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

    #[test]
    fn session_meta_cwd_rewrite_preserves_other_bytes_and_round_trips() {
        let remote = b"  { \"payload\" : { \"id\" : \"thread\", \"cwd\" : \"D:\\\\projects\\\\demo\", \"other\" : [1, {\"cwd\":\"untouched\"}] }, \"type\" : \"session_meta\" }\r\n";
        let local = rewrite_session_meta_cwd(remote, "thread", "F:\\workspace\\demo").unwrap();
        assert_ne!(local, remote);
        assert!(String::from_utf8_lossy(&local).contains("F:\\\\workspace\\\\demo"));
        assert!(String::from_utf8_lossy(&local).contains("\"cwd\":\"untouched\""));
        let restored = rewrite_session_meta_cwd(&local, "thread", "D:\\projects\\demo").unwrap();
        assert_eq!(restored, remote);
    }

    #[test]
    fn snapshot_object_mapping_round_trip_restores_the_remote_hash() {
        let repository = tempdir().unwrap();
        let bytes = b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"thread\",\"cwd\":\"D:/projects/demo\",\"model_provider\":\"openai\"}}\n{\"type\":\"message\",\"payload\":{\"text\":\"unchanged\"}}\n";
        let descriptor = ObjectDescriptor {
            sha256: format!("sha256:{}", hex::encode(Sha256::digest(bytes))),
            byte_length: bytes.len() as u64,
        };
        install_repository_object(
            repository.path(),
            &descriptor,
            Cursor::new(bytes),
            &OperationControl::default(),
        )
        .unwrap();
        let mapper = WorkspacePathMapper::new(vec![WorkspacePathMapping {
            remote_prefix: "D:/projects".to_string(),
            local_prefix: "F:/workspace".to_string(),
        }])
        .unwrap();
        let mut remote = fixture_snapshot("D:/projects/demo");
        remote.threads[0].rollout.sha256 = descriptor.sha256.clone();
        remote.threads[0].rollout.byte_length = descriptor.byte_length;
        remote.threads[0].rollout.source_path = None;

        let local = mapper
            .materialize_snapshot_objects(&remote, repository.path(), &OperationControl::default())
            .unwrap();
        assert_eq!(
            local.threads[0].workspace.source_path.as_deref(),
            Some("F:/workspace/demo")
        );
        assert_ne!(local.threads[0].rollout.sha256, descriptor.sha256);
        let local_bytes = fs::read(
            repository_object_path(repository.path(), &local.threads[0].rollout.sha256).unwrap(),
        )
        .unwrap();
        assert!(String::from_utf8_lossy(&local_bytes).contains("F:/workspace/demo"));

        let restored = mapper
            .canonicalize_snapshot_objects(&local, repository.path(), &OperationControl::default())
            .unwrap();
        assert_eq!(restored, remote);
        assert_eq!(restored.threads[0].rollout.sha256, descriptor.sha256);
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
