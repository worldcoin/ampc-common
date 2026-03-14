use dashmap::DashMap;
use eyre::{eyre, Result};
use std::{
    collections::HashSet,
    sync::atomic::{AtomicU32, Ordering},
};
use tokio::sync::oneshot;

use super::WorkerRsp;

/// Type of job for different coordination patterns
#[derive(Debug, Clone)]
pub enum JobType {
    /// Scatter work to multiple workers, gather and reassemble results
    ScatterGather { num_partitions: u16 },

    /// Broadcast same message to all workers, wait for all acks
    Broadcast { num_workers: u16 },
}

/// Internal state for a pending job
struct PendingJob {
    job_type: JobType,
    partitions_pending: HashSet<u16>,
    results: Vec<Option<Result<WorkerRsp>>>,
    response_tx: oneshot::Sender<Vec<WorkerRsp>>,
}

/// Job tracker for scatter-gather and broadcast operations
/// note that although concurrent access is not needed, the code
/// is more ergonomic with the Atomic and DashMap, and performance
/// impact should be negligible.
pub struct JobTracker {
    next_job_id: AtomicU32,
    pending_jobs: DashMap<u32, PendingJob>,
}

impl JobTracker {
    pub fn new() -> Self {
        Self {
            next_job_id: AtomicU32::new(0),
            pending_jobs: DashMap::new(),
        }
    }

    pub fn register_job(
        &self,
        job_type: JobType,
        response_tx: oneshot::Sender<Vec<WorkerRsp>>,
    ) -> u32 {
        // fetch_add wraps around on overflow
        let job_id = self.next_job_id.fetch_add(1, Ordering::SeqCst);

        let (num_partitions, partitions_pending) = match &job_type {
            JobType::ScatterGather { num_partitions } => {
                (*num_partitions, (0..*num_partitions).collect())
            }
            JobType::Broadcast { num_workers } => (*num_workers, (0..*num_workers).collect()),
        };

        let mut results = Vec::new();
        results.resize_with(num_partitions as usize, || None);

        let pending = PendingJob {
            job_type,
            partitions_pending,
            results,
            response_tx,
        };

        self.pending_jobs.insert(job_id, pending);

        job_id
    }

    pub fn record_response(&self, response: WorkerRsp) -> Result<()> {
        let job_id = response.job_id;
        let worker_id = response.worker_id;

        let mut entry = self
            .pending_jobs
            .get_mut(&job_id)
            .ok_or_else(|| eyre!("Job {} not found", job_id))?;

        let pending = entry.value_mut();

        if !pending.partitions_pending.remove(&worker_id) {
            return Err(eyre!(
                "Worker {} for job {} was not pending or already completed",
                worker_id,
                job_id
            ));
        }

        // Store result
        if (worker_id as usize) >= pending.results.len() {
            return Err(eyre!(
                "Worker ID {} out of bounds for job {}",
                worker_id,
                job_id
            ));
        }
        pending.results[worker_id as usize] = Some(Ok(response));

        // Check if job is complete
        if pending.partitions_pending.is_empty() {
            drop(entry); // Drop the mutable borrow before removing
            self.complete_job(job_id)?;
        }

        Ok(())
    }

    pub fn record_error(&self, job_id: u32, worker_id: u16, error: String) -> Result<()> {
        let mut entry = self
            .pending_jobs
            .get_mut(&job_id)
            .ok_or_else(|| eyre!("Job {} not found", job_id))?;

        let pending = entry.value_mut();

        // Remove from pending set
        if !pending.partitions_pending.remove(&worker_id) {
            return Err(eyre!(
                "Worker {} for job {} was not pending or already completed",
                worker_id,
                job_id
            ));
        }

        // Store error with dummy WorkerRsp (we'll handle it in complete_job)
        if (worker_id as usize) >= pending.results.len() {
            return Err(eyre!(
                "Worker ID {} out of bounds for job {}",
                worker_id,
                job_id
            ));
        }
        pending.results[worker_id as usize] = Some(Err(eyre!(error)));

        // Check if job is complete
        if pending.partitions_pending.is_empty() {
            drop(entry); // Drop the mutable borrow before removing
            self.complete_job(job_id)?;
        }

        Ok(())
    }

    /// Mark job as complete and send assembled response
    fn complete_job(&self, job_id: u32) -> Result<()> {
        let (_, pending) = self
            .pending_jobs
            .remove(&job_id)
            .ok_or_else(|| eyre!("Job {} not found", job_id))?;

        // Assemble results - same logic for both ScatterGather and Broadcast
        let mut results = Vec::with_capacity(pending.results.len());
        for (idx, result_opt) in pending.results.into_iter().enumerate() {
            let result = result_opt.ok_or_else(|| eyre!("Missing result for partition {}", idx))?;
            match result {
                Ok(value) => results.push(value),
                Err(e) => {
                    // On error, we can't send the partial results, just log
                    tracing::error!("Job {} partition {} failed: {}", job_id, idx, e);
                    return Err(e);
                }
            }
        }

        let _ = pending.response_tx.send(results);

        Ok(())
    }

    pub fn cancel_job(&self, job_id: u32, error: String) -> Result<()> {
        let (_, _pending) = self
            .pending_jobs
            .remove(&job_id)
            .ok_or_else(|| eyre!("Job {} not found", job_id))?;

        tracing::error!("Job {} cancelled: {}", job_id, error);
        Ok(())
    }

    pub fn pending_count(&self) -> usize {
        self.pending_jobs.len()
    }
}

impl Default for JobTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scatter_gather_basic() {
        let tracker = JobTracker::new();

        // Register a scatter-gather job with 3 partitions
        let (tx, rx) = oneshot::channel();
        let job_id = tracker.register_job(JobType::ScatterGather { num_partitions: 3 }, tx);
        assert_eq!(job_id, 0);

        // Record responses
        tracker
            .record_response(WorkerRsp {
                job_id,
                worker_id: 0,
                payload: vec![10],
            })
            .unwrap();
        tracker
            .record_response(WorkerRsp {
                job_id,
                worker_id: 2,
                payload: vec![30],
            })
            .unwrap();
        tracker
            .record_response(WorkerRsp {
                job_id,
                worker_id: 1,
                payload: vec![20],
            })
            .unwrap();

        // Check assembled result
        let values = rx.blocking_recv().unwrap();
        assert_eq!(values.len(), 3);
        assert_eq!(values[0].payload, vec![10]);
        assert_eq!(values[1].payload, vec![20]);
        assert_eq!(values[2].payload, vec![30]);
    }

    #[test]
    fn test_broadcast_basic() {
        let tracker = JobTracker::new();

        // Register a broadcast job to 4 workers
        let (tx, rx) = oneshot::channel();
        let job_id = tracker.register_job(JobType::Broadcast { num_workers: 4 }, tx);

        // Record acks from all workers
        for worker_id in 0..4 {
            tracker
                .record_response(WorkerRsp {
                    job_id,
                    worker_id,
                    payload: vec![],
                })
                .unwrap();
        }

        // Check assembled result
        let values = rx.blocking_recv().unwrap();
        assert_eq!(values.len(), 4);
    }

    #[test]
    fn test_error_propagation() {
        let tracker = JobTracker::new();

        let (tx, rx) = oneshot::channel();
        let job_id = tracker.register_job(JobType::ScatterGather { num_partitions: 3 }, tx);

        // One partition fails
        tracker
            .record_response(WorkerRsp {
                job_id,
                worker_id: 0,
                payload: vec![10],
            })
            .unwrap();
        tracker
            .record_error(job_id, 1, "Worker error".to_string())
            .unwrap();
        tracker
            .record_response(WorkerRsp {
                job_id,
                worker_id: 2,
                payload: vec![30],
            })
            .unwrap();

        // Should get error (channel will be dropped without sending)
        let result = rx.blocking_recv();
        assert!(result.is_err());
    }

    #[test]
    fn test_duplicate_partition() {
        let tracker = JobTracker::new();

        let (tx, _rx) = oneshot::channel();
        let job_id = tracker.register_job(JobType::ScatterGather { num_partitions: 2 }, tx);

        tracker
            .record_response(WorkerRsp {
                job_id,
                worker_id: 0,
                payload: vec![10],
            })
            .unwrap();

        // Try to record same partition again
        let result = tracker.record_response(WorkerRsp {
            job_id,
            worker_id: 0,
            payload: vec![20],
        });
        assert!(result.is_err());
    }

    #[test]
    fn test_cancel_job() {
        let tracker = JobTracker::new();

        let (tx, rx) = oneshot::channel();
        let job_id = tracker.register_job(JobType::ScatterGather { num_partitions: 3 }, tx);

        tracker
            .record_response(WorkerRsp {
                job_id,
                worker_id: 0,
                payload: vec![10],
            })
            .unwrap();

        // Cancel the job
        tracker.cancel_job(job_id, "Timeout".to_string()).unwrap();

        // Should receive error (channel dropped)
        let result = rx.blocking_recv();
        assert!(result.is_err());
    }
}
