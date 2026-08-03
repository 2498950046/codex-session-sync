use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use serde::Serialize;
use serde_json::Value;
use sync_core::{OperationControl, OperationProgress};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Running,
    Cancelling,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobSnapshot {
    pub job_id: String,
    pub kind: String,
    pub state: JobState,
    pub progress: OperationProgress,
    pub cancellable: bool,
    pub result_ready: bool,
    pub error: Option<String>,
}

#[derive(Clone)]
pub struct JobManager {
    jobs: Arc<Mutex<HashMap<String, JobEntry>>>,
    active_codex_homes: Arc<Mutex<HashSet<String>>>,
    repositories: Arc<Mutex<HashMap<String, RepositoryLeaseState>>>,
}

#[derive(Debug, Default)]
struct RepositoryLeaseState {
    readers: usize,
    writer: bool,
}

struct JobEntry {
    cancel: Arc<AtomicBool>,
    snapshot: JobSnapshot,
    result: Option<Value>,
}

#[derive(Debug)]
pub(crate) struct CodexHomeWriteLease {
    key: String,
    active_codex_homes: Arc<Mutex<HashSet<String>>>,
}

impl Drop for CodexHomeWriteLease {
    fn drop(&mut self) {
        if let Ok(mut active) = self.active_codex_homes.lock() {
            active.remove(&self.key);
        }
    }
}

#[derive(Debug)]
pub(crate) struct RepositoryLease {
    key: String,
    exclusive: bool,
    repositories: Arc<Mutex<HashMap<String, RepositoryLeaseState>>>,
}

impl Drop for RepositoryLease {
    fn drop(&mut self) {
        let Ok(mut repositories) = self.repositories.lock() else {
            return;
        };
        let mut remove = false;
        if let Some(state) = repositories.get_mut(&self.key) {
            if self.exclusive {
                state.writer = false;
            } else {
                state.readers = state.readers.saturating_sub(1);
            }
            remove = !state.writer && state.readers == 0;
        }
        if remove {
            repositories.remove(&self.key);
        }
    }
}

impl Default for JobManager {
    fn default() -> Self {
        Self {
            jobs: Arc::new(Mutex::new(HashMap::new())),
            active_codex_homes: Arc::new(Mutex::new(HashSet::new())),
            repositories: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl JobManager {
    pub(crate) fn try_acquire_codex_home(
        &self,
        codex_home: &Path,
    ) -> Result<CodexHomeWriteLease, String> {
        let key = normalized_codex_home(codex_home)?;
        let mut active = self
            .active_codex_homes
            .lock()
            .map_err(|_| "Codex Home write-lock registry is unavailable".to_string())?;
        if !active.insert(key.clone()) {
            return Err(format!(
                "Codex Home {} already has an active write operation",
                codex_home.display()
            ));
        }
        drop(active);
        Ok(CodexHomeWriteLease {
            key,
            active_codex_homes: self.active_codex_homes.clone(),
        })
    }

    pub(crate) fn try_acquire_repository_shared(
        &self,
        repository: &Path,
    ) -> Result<RepositoryLease, String> {
        self.try_acquire_repository(repository, false)
    }

    pub(crate) fn try_acquire_repository_exclusive(
        &self,
        repository: &Path,
    ) -> Result<RepositoryLease, String> {
        self.try_acquire_repository(repository, true)
    }

    fn try_acquire_repository(
        &self,
        repository: &Path,
        exclusive: bool,
    ) -> Result<RepositoryLease, String> {
        let key = normalized_path(repository)?;
        let mut repositories = self
            .repositories
            .lock()
            .map_err(|_| "repository lease registry is unavailable".to_string())?;
        let state = repositories.entry(key.clone()).or_default();
        if state.writer || (exclusive && state.readers > 0) {
            return Err(format!(
                "repository {} already has an incompatible active operation",
                repository.display()
            ));
        }
        if exclusive {
            state.writer = true;
        } else {
            state.readers = state
                .readers
                .checked_add(1)
                .ok_or_else(|| "repository reader count overflow".to_string())?;
        }
        drop(repositories);
        Ok(RepositoryLease {
            key,
            exclusive,
            repositories: self.repositories.clone(),
        })
    }

    pub(crate) fn start_home_repository_shared<R, F>(
        &self,
        codex_home: &Path,
        repository: &Path,
        kind: impl Into<String>,
        cancellable: bool,
        operation: F,
    ) -> Result<JobSnapshot, String>
    where
        R: Serialize + Send + 'static,
        F: FnOnce(OperationControl) -> anyhow::Result<R> + Send + 'static,
    {
        let home_lease = self.try_acquire_codex_home(codex_home)?;
        let repository_lease = self.try_acquire_repository_shared(repository)?;
        Ok(self.start(kind, cancellable, move |control| {
            let _home_lease = home_lease;
            let _repository_lease = repository_lease;
            operation(control)
        }))
    }

    pub(crate) fn start_home_repository_exclusive<R, F>(
        &self,
        codex_home: &Path,
        repository: &Path,
        kind: impl Into<String>,
        cancellable: bool,
        operation: F,
    ) -> Result<JobSnapshot, String>
    where
        R: Serialize + Send + 'static,
        F: FnOnce(OperationControl) -> anyhow::Result<R> + Send + 'static,
    {
        let home_lease = self.try_acquire_codex_home(codex_home)?;
        let repository_lease = self.try_acquire_repository_exclusive(repository)?;
        Ok(self.start(kind, cancellable, move |control| {
            let _home_lease = home_lease;
            let _repository_lease = repository_lease;
            operation(control)
        }))
    }

    pub(crate) fn start_repository_shared<R, F>(
        &self,
        repository: &Path,
        kind: impl Into<String>,
        cancellable: bool,
        operation: F,
    ) -> Result<JobSnapshot, String>
    where
        R: Serialize + Send + 'static,
        F: FnOnce(OperationControl) -> anyhow::Result<R> + Send + 'static,
    {
        let repository_lease = self.try_acquire_repository_shared(repository)?;
        Ok(self.start(kind, cancellable, move |control| {
            let _repository_lease = repository_lease;
            operation(control)
        }))
    }

    pub fn start<R, F>(
        &self,
        kind: impl Into<String>,
        cancellable: bool,
        operation: F,
    ) -> JobSnapshot
    where
        R: Serialize + Send + 'static,
        F: FnOnce(OperationControl) -> anyhow::Result<R> + Send + 'static,
    {
        let job_id = Uuid::now_v7().to_string();
        let kind = kind.into();
        let cancel = Arc::new(AtomicBool::new(false));
        let initial = JobSnapshot {
            job_id: job_id.clone(),
            kind,
            state: JobState::Running,
            progress: OperationProgress::indeterminate("queued", "任务正在准备"),
            cancellable,
            result_ready: false,
            error: None,
        };
        self.jobs.lock().expect("job manager lock poisoned").insert(
            job_id.clone(),
            JobEntry {
                cancel: cancel.clone(),
                snapshot: initial.clone(),
                result: None,
            },
        );

        let jobs = self.jobs.clone();
        let reporter_jobs = jobs.clone();
        let reporter_job_id = job_id.clone();
        let control = OperationControl::new(cancel.clone(), move |progress| {
            if let Some(entry) = reporter_jobs
                .lock()
                .expect("job manager lock poisoned")
                .get_mut(&reporter_job_id)
            {
                entry.snapshot.cancellable = progress.cancellable;
                entry.snapshot.progress = progress;
            }
        });
        tauri::async_runtime::spawn_blocking(move || {
            let result = operation(control.clone());
            let mut guard = jobs.lock().expect("job manager lock poisoned");
            let Some(entry) = guard.get_mut(&job_id) else {
                return;
            };
            match result {
                Ok(value) => match serde_json::to_value(value) {
                    Ok(value) => {
                        entry.snapshot.state = JobState::Completed;
                        entry.snapshot.progress = OperationProgress {
                            phase: "completed".to_string(),
                            message: "任务已完成".to_string(),
                            completed: 1,
                            total: Some(1),
                            unit: "tasks".to_string(),
                            cancellable: false,
                        };
                        entry.snapshot.cancellable = false;
                        entry.snapshot.result_ready = true;
                        entry.result = Some(value);
                    }
                    Err(error) => {
                        entry.snapshot.state = JobState::Failed;
                        entry.snapshot.error = Some(error.to_string());
                        entry.snapshot.cancellable = false;
                    }
                },
                Err(error) => {
                    entry.snapshot.state = if control.is_cancelled() {
                        JobState::Cancelled
                    } else {
                        JobState::Failed
                    };
                    entry.snapshot.error = Some(error.to_string());
                    entry.snapshot.cancellable = false;
                }
            }
        });
        initial
    }

    pub fn get(&self, job_id: &str) -> Option<JobSnapshot> {
        self.jobs
            .lock()
            .expect("job manager lock poisoned")
            .get(job_id)
            .map(|entry| entry.snapshot.clone())
    }

    pub fn cancel(&self, job_id: &str) -> Result<JobSnapshot, String> {
        let mut jobs = self
            .jobs
            .lock()
            .map_err(|_| "任务管理器不可用".to_string())?;
        let entry = jobs
            .get_mut(job_id)
            .ok_or_else(|| "找不到任务".to_string())?;
        if !entry.snapshot.cancellable {
            return Err("当前任务不能中断".to_string());
        }
        entry.cancel.store(true, Ordering::Relaxed);
        entry.snapshot.state = JobState::Cancelling;
        entry.snapshot.progress.message = "已请求取消，正在到达安全停止点".to_string();
        Ok(entry.snapshot.clone())
    }

    pub fn take_result(&self, job_id: &str) -> Result<Value, String> {
        let mut jobs = self
            .jobs
            .lock()
            .map_err(|_| "任务管理器不可用".to_string())?;
        let completed = jobs
            .get(job_id)
            .ok_or_else(|| "找不到任务".to_string())?
            .snapshot
            .state
            .clone();
        if !matches!(completed, JobState::Completed) {
            return Err("任务尚未完成".to_string());
        }
        jobs.remove(job_id)
            .expect("existing job disappeared")
            .result
            .ok_or_else(|| "任务结果已领取".to_string())
    }
}

fn normalized_codex_home(path: &Path) -> Result<String, String> {
    normalized_existing_path(path, "Codex Home")
}

fn normalized_existing_path(path: &Path, kind: &str) -> Result<String, String> {
    let resolved = std::fs::canonicalize(path)
        .map_err(|error| format!("failed to normalize {kind} {}: {error}", path.display()))?;
    let normalized = resolved.to_string_lossy().replace('\\', "/");
    #[cfg(windows)]
    let normalized = normalized.to_lowercase();
    Ok(normalized)
}

fn normalized_path(path: &Path) -> Result<String, String> {
    if path.exists() {
        return normalized_existing_path(path, "repository");
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| format!("failed to resolve repository path: {error}"))?
            .join(path)
    };
    let normalized = absolute.to_string_lossy().replace('\\', "/");
    #[cfg(windows)]
    let normalized = normalized.to_lowercase();
    Ok(normalized.trim_end_matches('/').to_string())
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn normalized_codex_home_write_leases_are_exclusive_and_release_on_drop() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("codex-home");
        std::fs::create_dir_all(&home).unwrap();
        let equivalent_home = home.join(".");
        let jobs = JobManager::default();

        let first = jobs.try_acquire_codex_home(&home).unwrap();
        let error = jobs.try_acquire_codex_home(&equivalent_home).unwrap_err();
        assert!(error.contains("already has an active write operation"));

        drop(first);
        assert!(jobs.try_acquire_codex_home(&equivalent_home).is_ok());
    }

    #[test]
    fn different_codex_homes_can_hold_write_leases_concurrently() {
        let directory = tempdir().unwrap();
        let first_home = directory.path().join("first-home");
        let second_home = directory.path().join("second-home");
        std::fs::create_dir_all(&first_home).unwrap();
        std::fs::create_dir_all(&second_home).unwrap();
        let jobs = JobManager::default();

        let first = jobs.try_acquire_codex_home(&first_home).unwrap();
        let second = jobs.try_acquire_codex_home(&second_home).unwrap();

        drop((first, second));
    }

    #[test]
    fn repository_shared_and_exclusive_leases_enforce_read_write_rules() {
        let directory = tempdir().unwrap();
        let jobs = JobManager::default();
        let first = jobs
            .try_acquire_repository_shared(directory.path())
            .unwrap();
        let second = jobs
            .try_acquire_repository_shared(directory.path())
            .unwrap();
        assert!(
            jobs.try_acquire_repository_exclusive(directory.path())
                .is_err()
        );
        drop((first, second));

        let writer = jobs
            .try_acquire_repository_exclusive(directory.path())
            .unwrap();
        assert!(
            jobs.try_acquire_repository_shared(directory.path())
                .is_err()
        );
        assert!(
            jobs.try_acquire_repository_exclusive(directory.path())
                .is_err()
        );
        drop(writer);
        assert!(jobs.try_acquire_repository_shared(directory.path()).is_ok());
    }
}
