//! Long-running async task monitoring.

use eyre::Result;
use std::{
    ops::{Deref, DerefMut},
    panic,
};
use tokio::task::{JoinError, JoinSet};

/// A long-running async task monitor which checks all its tasks for panics or
/// hangs when dropped. Designed for ongoing tasks which run until the program
/// exits.
#[derive(Debug, Default)]
pub struct TaskMonitor {
    pub tasks: JoinSet<Result<()>>,
}

impl Deref for TaskMonitor {
    type Target = JoinSet<Result<()>>;

    fn deref(&self) -> &Self::Target {
        &self.tasks
    }
}

impl DerefMut for TaskMonitor {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.tasks
    }
}

impl Drop for TaskMonitor {
    fn drop(&mut self) {
        self.tasks.abort_all();
    }
}

impl TaskMonitor {
    /// Create a new task monitor.
    pub fn new() -> Self {
        tracing::info!("Preparing task monitor");
        Self::default()
    }

    /// Panics if any of the monitored tasks have finished normally, were
    /// cancelled, or panicked.
    pub fn check_tasks(&mut self) {
        if let Some(finished_task) = self.tasks.try_join_next() {
            Self::panic_with_task_status(finished_task);
        }
    }

    /// Checks for panics, cancellations, or early finishes, then aborts all tasks.
    pub fn abort_all(&mut self) {
        self.check_tasks();
        self.tasks.abort_all();
    }

    /// Panics if any of the tasks have finished with a panic or hang.
    pub fn check_tasks_finished(&mut self) {
        while let Some(finished_task) = self.tasks.try_join_next() {
            Self::resume_panic(finished_task);
        }

        if !self.tasks.is_empty() {
            panic!(
                "{} monitored tasks hung even when aborted",
                self.tasks.len()
            );
        }
    }

    /// If `result` is a task panic, resume that panic.
    /// If `result` is an `eyre::Report`, panic with that error.
    #[track_caller]
    pub fn resume_panic(result: Result<Result<()>, JoinError>) {
        match result {
            Err(join_err) => {
                if !join_err.is_cancelled() {
                    panic::resume_unwind(join_err.into_panic());
                }
            }
            Ok(Err(report_err)) => panic!("{:?}", report_err),
            Ok(Ok(())) => { /* Task finished with Ok or was cancelled */ }
        }
    }

    /// Panics with a message containing the task exit status.
    #[track_caller]
    pub fn panic_with_task_status(result: Result<Result<()>, JoinError>) {
        result
            .expect("Monitored task was panicked or cancelled")
            .expect("Monitored task returned an error");

        panic!("Monitored task unexpectedly finished without an error");
    }
}
