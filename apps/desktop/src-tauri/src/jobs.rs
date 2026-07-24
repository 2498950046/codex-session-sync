use std::collections::HashMap;
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
}

struct JobEntry {
    cancel: Arc<AtomicBool>,
    snapshot: JobSnapshot,
    result: Option<Value>,
}

impl Default for JobManager {
    fn default() -> Self {
        Self {
            jobs: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl JobManager {
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
