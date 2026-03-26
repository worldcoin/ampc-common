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
    // registered means the job was requested but not sent to the workers yet
    registered: HashSet<WorkerId>,
    // pending means it was sent to the workers but no response has been received
    pending: HashSet<WorkerId>,
    results: HashMap<WorkerId, WorkerRsp>,
    response_tx: oneshot::Sender<WorkpoolRes>,
}

impl PendingJob {
    fn send_results(self) {
        let results: Vec<_> = self.results.into_values().collect();
        let _ = self.response_tx.send(Ok(results));
    }
}

/// Job tracker for scatter-gather and broadcast operations
pub struct JobTracker {
    jobs: DashMap<JobId, PendingJob>,
}

impl JobTracker {
    pub fn new() -> Self {
        Self {
            jobs: DashMap::new(),
        }
    }

    pub fn register_job(
        &self,
        job_id: JobId,
        job_type: JobType,
        response_tx: oneshot::Sender<WorkpoolRes>,
    ) {
        let partitions_registered = match job_type {
            JobType::ScatterGather { worker_ids } => worker_ids,
            JobType::Broadcast { num_workers } => (0..num_workers).collect(),
        };

        let pending = PendingJob {
            registered: partitions_registered,
            pending: HashSet::new(),
            results: HashMap::new(),
            response_tx,
        };

        self.jobs.insert(job_id, pending);
    }

    pub fn set_to_pending(&self, job_id: JobId, worker_id: WorkerId) -> Result<()> {
        let mut entry = self
            .jobs
            .get_mut(&job_id)
            .ok_or_else(|| eyre!("Job {} not found", job_id))?;
        let jobs = entry.value_mut();

        if !jobs.registered.remove(&worker_id) {
            return Err(eyre!(
                "Worker {} for job {} was not registered",
                worker_id,
                job_id
            ));
        }

        jobs.pending.insert(worker_id);
        Ok(())
    }

    pub fn record_response(&self, response: WorkerRsp) -> Result<()> {
        let job_id = response.job_id;
        let worker_id = response.worker_id;

        let mut entry = self
            .jobs
            .get_mut(&job_id)
            .ok_or_else(|| eyre!("Job {} not found", job_id))?;
        let jobs = entry.value_mut();

        if !jobs.pending.remove(&worker_id) {
            return Err(eyre!(
                "Worker {} for job {} was not pending or already completed",
                worker_id,
                job_id
            ));
        }

        jobs.results.insert(worker_id, response);

        if jobs.pending.is_empty() {
            drop(entry);
            self.complete_job(job_id);
        }
        Ok(())
    }

    /// Compares the worker's reported pending jobs against jobs the leader is expecting
    /// a response from this specific worker. Jobs that were sent to this worker but are
    /// not in the worker's pending set are considered lost (dropped in transit or worker
    /// restarted and lost state).
    pub fn validate_pending_jobs(&self, worker_id: WorkerId, pending_jobs: Vec<JobId>) {
        let got_pending_jobs: HashSet<JobId> = pending_jobs.into_iter().collect();
        let dropped_jobs: Vec<JobId> = self
            .jobs
            .iter()
            .filter(|r| {
                let pending = r.value();
                // Job was sent to this worker AND worker doesn't have it
                pending.pending.contains(&worker_id) && !got_pending_jobs.contains(r.key())
            })
            .map(|r| *r.key())
            .collect();

        for job_id in dropped_jobs {
            let error_response = WorkerRsp {
                job_id,
                worker_id,
                payload: Err(WorkpoolError::JobsLost { worker_id, job_id }),
            };

            if let Err(e) = self.record_response(error_response) {
                tracing::error!("record_response: {}", e);
            }
        }
    }

    /// validation should occur before this function is called.
    fn complete_job(&self, job_id: JobId) {
        let Some((_, job)) = self.jobs.remove(&job_id) else {
            tracing::warn!("job id not found: {}", job_id);
            return;
        };

        job.send_results();
    }

    /// Cancel a job and return the list of workers which are still pending
    pub fn cancel_job(&self, job_id: JobId) -> Result<Vec<WorkerId>> {
        let (_, job) = self
            .jobs
            .remove(&job_id)
            .ok_or_else(|| eyre!("Job {} not found", job_id))?;

        let mut worker_ids: Vec<WorkerId> = job.pending.iter().copied().collect();
        worker_ids.sort();
        Ok(worker_ids)
    }

    pub fn pending_count(&self) -> usize {
        self.jobs.len()
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
    use crate::network::workpool::payload::Payload;
    use crate::network::workpool::WorkpoolError;
    use bytes::Bytes;
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
                payload: Ok(vec![10].into()),
            })
            .unwrap();
        tracker
            .record_response(WorkerRsp {
                job_id,
                worker_id: 2,
                payload: Ok(vec![30].into()),
            })
            .unwrap();
        tracker
            .record_response(WorkerRsp {
                job_id,
                worker_id: 1,
                payload: Ok(vec![20].into()),
            })
            .unwrap();

        // Check assembled result (order is not guaranteed)
        let values = rx.blocking_recv().unwrap().unwrap();
        assert_eq!(values.len(), 3);
        let results: HashMap<WorkerId, _> = values
            .into_iter()
            .map(|r| (r.worker_id, r.payload))
            .collect();
        assert!(matches!(&results[&0], Ok(Payload::Bytes(b)) if b == &Bytes::from(vec![10])));
        assert!(matches!(&results[&1], Ok(Payload::Bytes(b)) if b == &Bytes::from(vec![20])));
        assert!(matches!(&results[&2], Ok(Payload::Bytes(b)) if b == &Bytes::from(vec![30])));
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
                    payload: Ok(Bytes::new().into()),
                })
                .unwrap();
        }

        // Check assembled result
        let results = rx.blocking_recv().unwrap().unwrap();
        assert_eq!(results.len(), 4);
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
                payload: Ok(vec![10].into()),
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
                payload: Ok(vec![30].into()),
            })
            .unwrap();

        // Check results by worker_id (order is not guaranteed)
        let result = rx.try_recv().unwrap().unwrap();
        let results: HashMap<WorkerId, _> = result
            .into_iter()
            .map(|r| (r.worker_id, r.payload))
            .collect();
        assert!(results[&0].is_ok());
        assert!(results[&1].is_err());
        assert!(results[&2].is_ok());
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
                payload: Ok(vec![10].into()),
            })
            .unwrap();

        // Try to record same partition again
        let result = tracker.record_response(WorkerRsp {
            job_id,
            worker_id: 0,
            payload: Ok(vec![20].into()),
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
                payload: Ok(vec![10].into()),
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
                payload: Ok(vec![50].into()),
            })
            .unwrap();
        tracker
            .record_response(WorkerRsp {
                job_id,
                worker_id: 0,
                payload: Ok(vec![10].into()),
            })
            .unwrap();
        tracker
            .record_response(WorkerRsp {
                job_id,
                worker_id: 2,
                payload: Ok(vec![30].into()),
            })
            .unwrap();

        // Check results (order is not guaranteed)
        let result = rx.blocking_recv().unwrap().unwrap();
        assert_eq!(result.len(), 3);
        let results: HashMap<WorkerId, _> = result
            .into_iter()
            .map(|r| (r.worker_id, r.payload))
            .collect();
        assert!(matches!(&results[&0], Ok(Payload::Bytes(b)) if b == &Bytes::from(vec![10])));
        assert!(matches!(&results[&2], Ok(Payload::Bytes(b)) if b == &Bytes::from(vec![30])));
        assert!(matches!(&results[&5], Ok(Payload::Bytes(b)) if b == &Bytes::from(vec![50])));
    }
}
