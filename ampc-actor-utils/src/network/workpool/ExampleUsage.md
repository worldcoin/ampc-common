# WorkPool Example Usage

For basic setup, broadcast, scatter-gather, and custom payload serialization examples, see the integration tests in `tests/workpool_integration.rs`.

This document covers additional patterns not shown in the tests.

## Pipelining Multiple Jobs

Submit multiple jobs concurrently to maximize throughput:

```rust
use ampc_actor_utils::network::workpool::leader::LeaderHandle;
use ampc_actor_utils::network::workpool::WorkpoolError;

async fn pipelined_operations(leader: &LeaderHandle) -> Result<(), WorkpoolError> {
    // Submit multiple jobs without waiting (returns JobHandles)
    let job1 = leader.broadcast(vec![1, 2, 3])?;
    let job2 = leader.broadcast(vec![4, 5, 6])?;
    let job3 = leader.broadcast(vec![7, 8, 9])?;

    // All jobs execute concurrently
    let (r1, r2, r3) = tokio::join!(job1, job2, job3);

    // Check results
    let responses1 = r1?;
    let responses2 = r2?;
    let responses3 = r3?;

    println!("Completed 3 pipelined jobs");
    Ok(())
}
```

## Async Worker Processing

Workers can perform async I/O or spawn tasks for long-running computations:

```rust
use ampc_actor_utils::network::workpool::worker::{WorkerHandle, Job};

async fn worker_loop(mut worker: WorkerHandle) {
    while let Some(job) = worker.recv().await {
        // Spawn task to handle job asynchronously
        tokio::spawn(async move {
            handle_async_job(job).await;
        });
    }
}

async fn handle_async_job(mut job: Job) {
    let payload = job.take_payload();

    // Perform async operation (e.g., database query, HTTP request)
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    job.send_result(payload);
}
```

## Error Handling

### Handling Mixed Results

Jobs may complete with a mix of successes and errors from different workers:

```rust
use ampc_actor_utils::network::workpool::leader::LeaderHandle;
use ampc_actor_utils::network::workpool::WorkpoolError;

async fn robust_broadcast(leader: &LeaderHandle, payload: Vec<u8>) {
    let handle = match leader.broadcast(payload) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("Failed to submit job: {}", e);
            return;
        }
    };

    match handle.await {
        Ok(responses) => {
            for rsp in responses {
                match &rsp.payload {
                    Ok(data) => {
                        println!("Worker {} succeeded with {} bytes", rsp.worker_id, data.len());
                    }
                    Err(WorkpoolError::JobsLost { worker_id, expected, actual }) => {
                        eprintln!("Worker {} dropped job: expected {}, got {:?}",
                                 worker_id, expected, actual);
                    }
                    Err(e) => {
                        eprintln!("Worker {} error: {}", rsp.worker_id, e);
                    }
                }
            }
        }
        Err(WorkpoolError::SendFailed) => {
            eprintln!("Lost connection to worker tasks");
        }
        Err(e) => {
            eprintln!("System error: {}", e);
        }
    }
}
```

## Job Cancellation

### Timeout-Based Cancellation

```rust
use tokio::time::{timeout, Duration};
use ampc_actor_utils::network::workpool::leader::LeaderHandle;
use ampc_actor_utils::network::workpool::WorkpoolError;

async fn timeout_job(leader: &LeaderHandle) -> Result<(), WorkpoolError> {
    let handle = leader.broadcast(vec![1, 2, 3])?;

    match timeout(Duration::from_secs(5), handle).await {
        Ok(Ok(responses)) => {
            println!("Job completed with {} responses", responses.len());
        }
        Ok(Err(e)) => {
            eprintln!("Job failed: {}", e);
        }
        Err(_) => {
            eprintln!("Job timed out after 5 seconds");
            // Job is cancelled when handle is dropped
        }
    }

    Ok(())
}
```

### User-Driven Cancellation

```rust
use tokio::sync::oneshot;
use ampc_actor_utils::network::workpool::leader::LeaderHandle;
use ampc_actor_utils::network::workpool::WorkpoolError;

async fn cancellable_job(
    leader: &LeaderHandle,
    cancel_signal: oneshot::Receiver<()>,
) -> Result<(), WorkpoolError> {
    let handle = leader.broadcast(vec![1, 2, 3])?;

    tokio::select! {
        result = handle => {
            match result {
                Ok(responses) => println!("Completed with {} responses", responses.len()),
                Err(e) => eprintln!("Job failed: {}", e),
            }
        }
        _ = cancel_signal => {
            println!("User cancelled");
            // handle dropped here, job cancelled
        }
    }

    Ok(())
}
```

### Cancellation Notes

- Cancellation removes the job from the leader's JobTracker
- Workers may still complete the job; their responses are dropped
- `Cancel` messages are sent to workers (best-effort) but not enforced
- For long-running jobs requiring immediate worker-side cancellation, use application-level protocols

## Handling Dropped Jobs

After reconnection, the system detects jobs lost during network disruption:

```rust
use ampc_actor_utils::network::workpool::leader::LeaderHandle;
use ampc_actor_utils::network::workpool::WorkpoolError;

async fn resilient_computation(leader: &LeaderHandle) {
    let handle = match leader.broadcast(vec![1, 2, 3, 4, 5]) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("Failed to submit job: {}", e);
            return;
        }
    };

    match handle.await {
        Ok(responses) => {
            let mut successes = 0;
            let mut dropped = 0;

            for rsp in responses {
                match &rsp.payload {
                    Ok(_) => successes += 1,
                    Err(WorkpoolError::JobsLost { .. }) => {
                        dropped += 1;
                        eprintln!("Worker {} dropped the job", rsp.worker_id);
                    }
                    Err(e) => eprintln!("Worker {} error: {}", rsp.worker_id, e),
                }
            }

            println!("Results: {} successes, {} dropped", successes, dropped);

            if dropped > 0 {
                // Implement retry logic here
            }
        }
        Err(e) => eprintln!("System error: {}", e),
    }
}
```

### How Reconciliation Works

1. Leader queries worker's last received job ID after reconnection
2. Pending jobs with ID < last received are marked as dropped
3. Dropped jobs complete with `WorkpoolError::JobsLost` in their payload
4. Application receives mixed results and can implement retry logic
