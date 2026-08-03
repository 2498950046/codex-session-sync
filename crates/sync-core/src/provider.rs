use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::local::{install_prepared_repository_object, repository_object_path};
use crate::storage_v3::{ContentRef, ContentStore, FilesystemContentStore, canonical_json};
use crate::{LocalSnapshot, ObjectDescriptor, OperationControl, OperationProgress, ThreadBundle};

pub const REMOTE_PROVIDER_PLACEHOLDER: &str = "__codex_session_sync_local_provider__";

pub fn detect_configured_provider(codex_home: &Path) -> Result<String> {
    let path = codex_home.join("config.toml");
    if !path.is_file() {
        return Ok("openai".to_string());
    }
    let text = fs::read_to_string(&path)
        .with_context(|| format!("failed to read Codex provider config {}", path.display()))?;
    let config: toml::Value = text
        .parse()
        .with_context(|| format!("failed to parse Codex provider config {}", path.display()))?;
    let provider = config
        .get("model_provider")
        .and_then(toml::Value::as_str)
        .unwrap_or("openai");
    Ok(validate_provider_id(provider)?.to_string())
}

pub fn validate_provider_id(provider: &str) -> Result<&str> {
    let provider = provider.trim();
    if provider.is_empty() || provider.len() > 128 {
        bail!("provider ID must contain between 1 and 128 characters");
    }
    if !provider
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        bail!("provider ID contains unsupported characters");
    }
    Ok(provider)
}

pub fn canonicalize_snapshot_provider_objects(
    snapshot: &LocalSnapshot,
    repository_root: &Path,
    control: &OperationControl,
) -> Result<LocalSnapshot> {
    transform_snapshot_provider_objects(snapshot, None, repository_root, control)
}

pub fn materialize_snapshot_provider_objects(
    snapshot: &LocalSnapshot,
    provider: &str,
    repository_root: &Path,
    control: &OperationControl,
) -> Result<LocalSnapshot> {
    let provider = validate_provider_id(provider)?;
    transform_snapshot_provider_objects(snapshot, Some(provider), repository_root, control)
}

fn transform_snapshot_provider_objects(
    snapshot: &LocalSnapshot,
    provider: Option<&str>,
    repository_root: &Path,
    control: &OperationControl,
) -> Result<LocalSnapshot> {
    let mut transformed = snapshot.clone();
    for (index, thread) in transformed.threads.iter_mut().enumerate() {
        control.check_cancelled()?;
        control.report(OperationProgress {
            phase: if provider.is_some() {
                "materialize_provider".to_string()
            } else {
                "canonicalize_provider".to_string()
            },
            message: thread.title.clone(),
            completed: index as u64,
            total: Some(snapshot.threads.len() as u64),
            unit: "threads".to_string(),
            cancellable: true,
        });
        set_thread_provider(thread, provider);
        rewrite_rollout_provider(
            &mut thread.rollout,
            &thread.thread_id,
            provider.unwrap_or(REMOTE_PROVIDER_PLACEHOLDER),
            repository_root,
            control,
        )?;
    }
    Ok(transformed)
}

fn set_thread_provider(thread: &mut ThreadBundle, provider: Option<&str>) {
    thread.model_provider = provider.map(str::to_string);
    if provider.is_none() {
        for rows in thread.related_records.tables.values_mut() {
            for row in rows {
                remove_provider_fields(row);
            }
        }
        return;
    }
    if let Some(rows) = thread.related_records.tables.get_mut("threads") {
        for row in rows {
            let Some(row) = row.as_object_mut() else {
                continue;
            };
            match provider {
                Some(provider) => {
                    row.insert(
                        "model_provider".to_string(),
                        Value::String(provider.to_string()),
                    );
                }
                None => {
                    row.remove("model_provider");
                }
            }
        }
    }
}

fn remove_provider_fields(value: &mut Value) {
    match value {
        Value::Object(object) => {
            object.remove("model_provider");
            for value in object.values_mut() {
                remove_provider_fields(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                remove_provider_fields(value);
            }
        }
        _ => {}
    }
}

fn rewrite_rollout_provider(
    object: &mut crate::ContentObject,
    thread_id: &str,
    target_provider: &str,
    repository_root: &Path,
    control: &OperationControl,
) -> Result<()> {
    let (source, temporary_source) = materialized_source(object, repository_root, control)?;
    let temporary_output = repository_root
        .join("objects")
        .join("tmp")
        .join(format!("{}.provider-object.tmp", Uuid::now_v7()));
    let result = transform_rollout_file(
        &source,
        &temporary_output,
        thread_id,
        target_provider,
        control,
    );
    if let Some(path) = temporary_source {
        let _ = fs::remove_file(path);
    }
    let descriptor = match result {
        Ok(descriptor) => descriptor,
        Err(error) => {
            let _ = fs::remove_file(&temporary_output);
            return Err(error);
        }
    };
    if descriptor.sha256 == object.sha256 && descriptor.byte_length == object.byte_length {
        let _ = fs::remove_file(&temporary_output);
        return Ok(());
    }
    if let Err(error) =
        install_prepared_repository_object(repository_root, &temporary_output, &descriptor)
    {
        let _ = fs::remove_file(&temporary_output);
        return Err(error);
    }
    object.sha256 = descriptor.sha256;
    object.byte_length = descriptor.byte_length;
    object.source_path = None;
    let content = FilesystemContentStore::open(repository_root.to_path_buf())?.ingest(
        &repository_object_path(repository_root, &object.sha256)?,
        control,
    )?;
    object.storage = Some(content.storage);
    Ok(())
}

fn materialized_source(
    object: &crate::ContentObject,
    repository_root: &Path,
    control: &OperationControl,
) -> Result<(PathBuf, Option<PathBuf>)> {
    let whole = repository_object_path(repository_root, &object.sha256)?;
    if whole.is_file() {
        return Ok((whole, None));
    }
    let storage = object
        .storage
        .clone()
        .context("rollout has no physical storage reference")?;
    let temporary = repository_root
        .join("objects")
        .join("tmp")
        .join(format!("{}.provider-source.tmp", Uuid::now_v7()));
    FilesystemContentStore::open(repository_root.to_path_buf())?.materialize(
        &ContentRef {
            logical_sha256: object.sha256.clone(),
            byte_length: object.byte_length,
            storage,
            media_type: Some(object.media_type.clone()),
            logical_path: object.logical_path.clone(),
        },
        &temporary,
        control,
    )?;
    Ok((temporary.clone(), Some(temporary)))
}

pub(crate) fn transform_rollout_file(
    source: &Path,
    destination: &Path,
    thread_id: &str,
    target_provider: &str,
    control: &OperationControl,
) -> Result<ObjectDescriptor> {
    let input = File::open(source)
        .with_context(|| format!("failed to open rollout object {}", source.display()))?;
    let output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)?;
    let mut reader = BufReader::new(input);
    let mut writer = BufWriter::new(output);
    let mut output_hasher = Sha256::new();
    let mut output_length = 0_u64;
    let mut found_session_meta = false;
    let mut line = Vec::new();
    loop {
        control.check_cancelled()?;
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        let transformed = transform_jsonl_line(&line, thread_id, target_provider)?;
        found_session_meta |= transformed.is_some();
        let bytes = transformed.as_deref().unwrap_or(&line);
        writer.write_all(bytes)?;
        output_hasher.update(bytes);
        output_length = output_length
            .checked_add(bytes.len() as u64)
            .context("provider-neutral rollout length overflow")?;
    }
    if !found_session_meta {
        bail!("thread {thread_id} rollout has no session_meta record");
    }
    writer.flush()?;
    writer.get_ref().sync_all()?;
    drop(writer);
    Ok(ObjectDescriptor {
        sha256: format!("sha256:{}", hex::encode(output_hasher.finalize())),
        byte_length: output_length,
    })
}

fn transform_jsonl_line(
    line: &[u8],
    thread_id: &str,
    target_provider: &str,
) -> Result<Option<Vec<u8>>> {
    let (body, ending) = if let Some(body) = line.strip_suffix(b"\r\n") {
        (body, b"\r\n".as_slice())
    } else if let Some(body) = line.strip_suffix(b"\n") {
        (body, b"\n".as_slice())
    } else {
        (line, b"".as_slice())
    };
    let Ok(mut value) = serde_json::from_slice::<Value>(body) else {
        return Ok(None);
    };
    if value.get("type").and_then(Value::as_str) != Some("session_meta") {
        return Ok(None);
    }
    let payload = value
        .get_mut("payload")
        .and_then(Value::as_object_mut)
        .context("rollout session_meta payload is not an object")?;
    if let Some(id) = payload.get("id").and_then(Value::as_str)
        && id != thread_id
    {
        bail!("rollout session_meta thread ID does not match {thread_id}");
    }
    payload.insert(
        "model_provider".to_string(),
        Value::String(target_provider.to_string()),
    );
    let mut output = canonical_json(&value)?;
    output.extend_from_slice(ending);
    Ok(Some(output))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_only_changes_have_identical_canonical_bytes() {
        let openai = b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"thread\",\"model_provider\":\"openai\",\"cwd\":\"C:/work\"}}\n{\"type\":\"message\",\"payload\":{\"text\":\"same\"}}\n";
        let custom = b"{\"payload\":{\"cwd\":\"C:/work\",\"model_provider\":\"custom\",\"id\":\"thread\"},\"type\":\"session_meta\"}\n{\"type\":\"message\",\"payload\":{\"text\":\"same\"}}\n";
        let first = transform_jsonl_line(
            openai
                .split_inclusive(|byte| *byte == b'\n')
                .next()
                .unwrap(),
            "thread",
            REMOTE_PROVIDER_PLACEHOLDER,
        )
        .unwrap()
        .unwrap();
        let second = transform_jsonl_line(
            custom
                .split_inclusive(|byte| *byte == b'\n')
                .next()
                .unwrap(),
            "thread",
            REMOTE_PROVIDER_PLACEHOLDER,
        )
        .unwrap()
        .unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn non_session_meta_bytes_are_unchanged() {
        let line = b"  {not valid json}\r\n";
        assert_eq!(
            transform_jsonl_line(line, "thread", "custom").unwrap(),
            None
        );
    }
}
