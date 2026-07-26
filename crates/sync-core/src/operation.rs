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

    pub fn non_cancellable(&self) -> Self {
        let reporter = self.reporter.as_ref().map(|reporter| {
            let reporter = reporter.clone();
            Arc::new(move |mut progress: OperationProgress| {
                progress.cancellable = false;
                reporter(progress);
            }) as ProgressReporter
        });
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            reporter,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[test]
    fn non_cancellable_control_ignores_cancellation_and_forces_progress_state() {
        let cancelled = Arc::new(AtomicBool::new(true));
        let reported = Arc::new(Mutex::new(Vec::new()));
        let captured = reported.clone();
        let control = OperationControl::new(cancelled, move |progress| {
            captured.lock().unwrap().push(progress);
        });
        assert!(control.check_cancelled().is_err());

        let non_cancellable = control.non_cancellable();
        assert!(non_cancellable.check_cancelled().is_ok());
        non_cancellable.report(OperationProgress::indeterminate("publish", "Publishing"));
        assert!(!reported.lock().unwrap()[0].cancellable);
    }
}
