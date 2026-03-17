use dashmap::DashMap;
use eyre::{eyre, Result};
use std::collections::{HashMap, HashSet};
use tokio::sync::oneshot;

use super::WorkerRsp;
use crate::network::workpool::leader::WorkpoolRes;
use crate::network::workpool::{JobId, WorkerId, WorkpoolError};

/// Type of job for different coordination patterns
#[derive(Debug, Clone)]
pub enum JobType {
    /// Scatter work to multiple workers, gather and reassemble results
    ScatterGather { worker_ids: HashSet<WorkerId> },

    /// Broadcast same message to all workers, wait for all acks
    Broadcast { num_workers: WorkerId },
}

/// Internal state for a pending job
struct PendingJob {
    partitions_pending: HashSet<WorkerId>,
    results: HashMap<WorkerId, WorkerRsp>,
    response_tx: oneshot::Sender<WorkpoolRes>,
}

/// Job tracker for scatter-gather and broadcast operations
pub struct JobTracker {
    pending_jobs: DashMap<JobId, PendingJob>,
}

impl JobTracker {
    pub fn new() -> Self {
        Self {
            pending_jobs: DashMap::new(),
        }
    }

    pub fn register_job(
        &self,
        job_id: JobId,
        job_type: JobType,
        response_tx: oneshot::Sender<WorkpoolRes>,
    ) {
        let partitions_pending = match job_type {
            JobType::ScatterGather { worker_ids } => worker_ids,
            JobType::Broadcast { num_workers } => (0..num_workers).collect(),
        };

        let pending = PendingJob {
            partitions_pending,
            results: HashMap::new(),
            response_tx,
        };

        self.pending_jobs.insert(job_id, pending);
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

        pending.results.insert(worker_id, response);

        if pending.partitions_pending.is_empty() {
            drop(entry);
            self.complete_job(job_id);
        }
        Ok(())
    }

    /// Checks for dropped jobs and marks them as completed with errors.
    ///
    /// If a worker reports receiving job N, any pending job with ID < N is considered dropped.
    /// If a worker reports receiving NO jobs (None), all pending jobs are considered dropped.
    /// Dropped partitions are marked as completed with an error, and if all partitions for
    /// a job are complete, the job result is sent.
    pub fn check_for_dropped_jobs(&self, worker_id: WorkerId, last_rxd_job: Option<JobId>) {
        // Collect dropped jobs to avoid holding iterator locks during mutation
        let dropped_jobs: Vec<JobId> = self
            .pending_jobs
            .iter()
            .filter_map(|entry| {
                let job_id = *entry.key();
                let pending = entry.value();

                // Job was dropped if:
                // - It has a lower ID than the last received job (or no job was received)
                // - This worker is still pending for this job (hasn't responded)
                if last_rxd_job
                    .map(|last_rxd| job_id < last_rxd)
                    .unwrap_or(true)
                    && pending.partitions_pending.contains(&worker_id)
                {
                    Some(job_id)
                } else {
                    None
                }
            })
            .collect();

        // Record error responses for each dropped partition
        for job_id in dropped_jobs {
            let error_response = WorkerRsp {
                job_id,
                worker_id,
                payload: Err(WorkpoolError::JobsLost {
                    worker_id,
                    expected: job_id,
                    actual: last_rxd_job,
                }),
            };

            if let Err(e) = self.record_response(error_response) {
                tracing::error!("Failed to record dropped job response: {}", e);
            }
        }
    }

    /// validation should occur before this function is called.
    fn complete_job(&self, job_id: JobId) {
        let (_, pending) = self
            .pending_jobs
            .remove(&job_id)
            .ok_or_else(|| eyre!("Job {} not found", job_id))
            .expect("fatal error in complete_job");

        // Collect results and sort by worker_id for deterministic ordering
        let mut results: Vec<_> = pending.results.into_iter().collect();
        results.sort_by_key(|(worker_id, _)| *worker_id);
        let results: Vec<_> = results.into_iter().map(|(_, rsp)| rsp).collect();

        let _ = pending.response_tx.send(Ok(results));
    }

    /// Cancel a job and return the list of workers which are still pending
    pub fn cancel_job(&self, job_id: JobId) -> Result<Vec<WorkerId>> {
        let (_, pending) = self
            .pending_jobs
            .remove(&job_id)
            .ok_or_else(|| eyre!("Job {} not found", job_id))?;

        let mut worker_ids: Vec<WorkerId> = pending.partitions_pending.iter().copied().collect();
        worker_ids.sort();
        Ok(worker_ids)
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
    use crate::network::workpool::WorkpoolError;
    use tracing_test::traced_test;

    #[test]
    fn test_scatter_gather_basic() {
        let tracker = JobTracker::new();

        // Register a scatter-gather job with 3 workers
        let (tx, rx) = oneshot::channel();
        let worker_ids = [0, 1, 2].into_iter().collect();
        let job_id = 0;
        tracker.register_job(job_id, JobType::ScatterGather { worker_ids }, tx);

        // Record responses
        tracker
            .record_response(WorkerRsp {
                job_id,
                worker_id: 0,
                payload: Ok(vec![10]),
            })
            .unwrap();
        tracker
            .record_response(WorkerRsp {
                job_id,
                worker_id: 2,
                payload: Ok(vec![30]),
            })
            .unwrap();
        tracker
            .record_response(WorkerRsp {
                job_id,
                worker_id: 1,
                payload: Ok(vec![20]),
            })
            .unwrap();

        // Check assembled result
        let values = rx.blocking_recv().unwrap().unwrap();
        assert_eq!(values.len(), 3);
        assert_eq!(values[0].payload, Ok(vec![10]));
        assert_eq!(values[1].payload, Ok(vec![20]));
        assert_eq!(values[2].payload, Ok(vec![30]));
    }

    #[test]
    fn test_broadcast_basic() {
        let tracker = JobTracker::new();

        let job_id = 0;
        let (tx, rx) = oneshot::channel();
        tracker.register_job(job_id, JobType::Broadcast { num_workers: 4 }, tx);

        // Record acks from all workers
        for worker_id in 0..4 {
            tracker
                .record_response(WorkerRsp {
                    job_id,
                    worker_id,
                    payload: Ok(vec![]),
                })
                .unwrap();
        }

        // Check assembled result
        let values = rx.blocking_recv().unwrap().unwrap();
        assert_eq!(values.len(), 4);
    }

    #[test]
    #[traced_test]
    fn test_error_propagation() {
        let tracker = JobTracker::new();

        let (tx, mut rx) = oneshot::channel();
        let worker_ids = [0, 1, 2].into_iter().collect();
        let job_id = 0;
        tracker.register_job(job_id, JobType::ScatterGather { worker_ids }, tx);

        // One partition fails
        tracker
            .record_response(WorkerRsp {
                job_id,
                worker_id: 0,
                payload: Ok(vec![10]),
            })
            .unwrap();
        tracker
            .record_response(WorkerRsp {
                job_id,
                worker_id: 1,
                payload: Err(WorkpoolError::SendFailed),
            })
            .unwrap();
        tracker
            .record_response(WorkerRsp {
                job_id,
                worker_id: 2,
                payload: Ok(vec![30]),
            })
            .unwrap();

        let result = rx.try_recv().unwrap().unwrap();
        assert!(result[0].payload.is_ok());
        assert!(result[1].payload.is_err());
    }

    #[test]
    fn test_duplicate_partition() {
        let tracker = JobTracker::new();

        let (tx, _rx) = oneshot::channel();
        let worker_ids = [0, 1].into_iter().collect();
        let job_id = 0;
        tracker.register_job(job_id, JobType::ScatterGather { worker_ids }, tx);

        tracker
            .record_response(WorkerRsp {
                job_id,
                worker_id: 0,
                payload: Ok(vec![10]),
            })
            .unwrap();

        // Try to record same partition again
        let result = tracker.record_response(WorkerRsp {
            job_id,
            worker_id: 0,
            payload: Ok(vec![20]),
        });
        assert!(result.is_err());
    }

    #[test]
    fn test_cancel_job() {
        let tracker = JobTracker::new();

        let (tx, rx) = oneshot::channel();
        let worker_ids = [0, 1, 2].into_iter().collect();
        let job_id = 0;
        tracker.register_job(job_id, JobType::ScatterGather { worker_ids }, tx);

        tracker
            .record_response(WorkerRsp {
                job_id,
                worker_id: 0,
                payload: Ok(vec![10]),
            })
            .unwrap();

        // Cancel the job
        let worker_ids = tracker.cancel_job(job_id).unwrap();
        assert_eq!(worker_ids, vec![1, 2]);

        // Should receive error (channel dropped)
        let result = rx.blocking_recv();
        assert!(result.is_err());
    }

    #[test]
    fn test_scatter_gather_sparse_workers() {
        let tracker = JobTracker::new();

        // Register a scatter-gather job with non-contiguous worker IDs
        let (tx, rx) = oneshot::channel();
        let worker_ids = [0, 2, 5].into_iter().collect();
        let job_id = 0;
        tracker.register_job(job_id, JobType::ScatterGather { worker_ids }, tx);

        // Record responses
        tracker
            .record_response(WorkerRsp {
                job_id,
                worker_id: 5,
                payload: Ok(vec![50]),
            })
            .unwrap();
        tracker
            .record_response(WorkerRsp {
                job_id,
                worker_id: 0,
                payload: Ok(vec![10]),
            })
            .unwrap();
        tracker
            .record_response(WorkerRsp {
                job_id,
                worker_id: 2,
                payload: Ok(vec![30]),
            })
            .unwrap();

        // Check results are sorted by worker_id
        let result = rx.blocking_recv().unwrap().unwrap();
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].worker_id, 0);
        assert_eq!(result[0].payload, Ok(vec![10]));
        assert_eq!(result[1].worker_id, 2);
        assert_eq!(result[1].payload, Ok(vec![30]));
        assert_eq!(result[2].worker_id, 5);
        assert_eq!(result[2].payload, Ok(vec![50]));
    }
}
