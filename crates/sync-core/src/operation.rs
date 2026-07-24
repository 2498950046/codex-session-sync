use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OperationProgress {
    pub phase: String,
    pub message: String,
    pub completed: u64,
    pub total: Option<u64>,
    pub unit: String,
    pub cancellable: bool,
}

impl OperationProgress {
    pub fn indeterminate(phase: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            phase: phase.into(),
            message: message.into(),
            completed: 0,
            total: None,
            unit: "items".to_string(),
            cancellable: true,
        }
    }
}

type ProgressReporter = Arc<dyn Fn(OperationProgress) + Send + Sync>;

#[derive(Clone, Default)]
pub struct OperationControl {
    cancelled: Arc<AtomicBool>,
    reporter: Option<ProgressReporter>,
}

impl OperationControl {
    pub fn new(
        cancelled: Arc<AtomicBool>,
        reporter: impl Fn(OperationProgress) + Send + Sync + 'static,
    ) -> Self {
        Self {
            cancelled,
            reporter: Some(Arc::new(reporter)),
        }
    }

    pub fn cancel_handle(&self) -> Arc<AtomicBool> {
        self.cancelled.clone()
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    pub fn check_cancelled(&self) -> Result<()> {
        if self.is_cancelled() {
            bail!("operation cancelled");
        }
        Ok(())
    }

    pub fn report(&self, progress: OperationProgress) {
        if let Some(reporter) = &self.reporter {
            reporter(progress);
        }
    }
}
