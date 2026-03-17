# WorkPool Example Usage

This guide demonstrates how to effectively use the WorkPool system for common distributed computing patterns.

## Basic Setup

### Starting a Leader

```rust
use ampc_actor_utils::network::workpool::leader::{build_leader, LeaderArgs};
use ampc_actor_utils::execution::player::Identity;
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing/logging
    tracing_subscriber::fmt::init();

    // Configure leader with worker addresses
    let args = LeaderArgs {
        leader_id: Identity("leader-node".to_string()),
        leader_address: "0.0.0.0:8000".to_string(),
        worker_addresses: vec![
            "192.168.1.10:9000".to_string(),
            "192.168.1.11:9000".to_string(),
            "192.168.1.12:9000".to_string(),
        ],
        tls: None,  // Use TLS in production
    };

    let shutdown = CancellationToken::new();
    let leader = build_leader(args, shutdown.clone()).await?;

    println!("Leader started with {} workers", leader.num_workers());

    // ... do work ...

    // Graceful shutdown
    shutdown.cancel();
    Ok(())
}
```

### Starting a Worker

```rust
use ampc_actor_utils::network::workpool::worker::{build_worker_handle, WorkerArgs, Job};
use ampc_actor_utils::execution::player::Identity;
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let args = WorkerArgs {
        worker_id: Identity("worker-1".to_string()),
        worker_address: "0.0.0.0:9000".to_string(),
        leader_id: Identity("leader-node".to_string()),
        leader_address: "192.168.1.5:8000".to_string(),
        tls: None,
    };

    let shutdown = CancellationToken::new();
    let mut worker = build_worker_handle(args, shutdown.clone()).await?;

    println!("Worker started, waiting for jobs...");

    // Process jobs in a loop
    while let Some(job) = worker.recv().await {
        handle_job(job);
    }

    Ok(())
}

fn handle_job(mut job: Job) {
    // Extract payload (zero-copy move)
    let payload = job.take_payload();

    // Process the work
    let result = process_data(payload);

    // Send result back to leader
    job.send_result(result);
}

fn process_data(data: Vec<u8>) -> Vec<u8> {
    // Your application logic here
    data
}
```

## Pattern 1: Broadcast

**Use Case**: Send the same command/data to all workers and collect acknowledgments.

### Leader Side

```rust
use serde::{Serialize, Deserialize};
use ampc_actor_utils::network::workpool::leader::LeaderHandle;
use ampc_actor_utils::network::workpool::WorkpoolError;

#[derive(Serialize, Deserialize)]
struct Command {
    operation: String,
    timestamp: u64,
}

async fn broadcast_command(leader: &LeaderHandle) -> Result<(), WorkpoolError> {
    // Serialize command
    let cmd = Command {
        operation: "initialize".to_string(),
        timestamp: 1234567890,
    };
    let payload = bincode::serialize(&cmd).unwrap();

    // Broadcast to all workers
    let responses = leader.broadcast(payload).await?;

    // Process responses (may contain errors from individual workers)
    println!("Received {} responses", responses.len());
    for rsp in responses {
        match &rsp.payload {
            Ok(data) => {
                println!("Worker {} responded with {} bytes", rsp.worker_id, data.len());
            }
            Err(e) => {
                println!("Worker {} returned error: {}", rsp.worker_id, e);
            }
        }
    }

    Ok(())
}
```

### Worker Side

```rust
use serde::{Serialize, Deserialize};
use ampc_actor_utils::network::workpool::worker::Job;

#[derive(Serialize, Deserialize)]
struct Command {
    operation: String,
    timestamp: u64,
}

fn handle_broadcast_job(mut job: Job) {
    let payload = job.take_payload();

    // Deserialize command
    let cmd: Command = bincode::deserialize(&payload).unwrap();
    println!("Received command: {}", cmd.operation);

    // Execute command
    execute_command(&cmd);

    // Send acknowledgment
    let ack = bincode::serialize(&"OK").unwrap();
    job.send_result(ack);
}

fn execute_command(cmd: &Command) {
    // Application-specific logic
    println!("Executing: {} at {}", cmd.operation, cmd.timestamp);
}
```

## Pattern 2: Scatter-Gather

**Use Case**: Distribute different data chunks to workers and collect ordered results.

### Leader Side

```rust
use ampc_actor_utils::network::workpool::leader::{LeaderHandle, WorkerJob};
use ampc_actor_utils::network::workpool::WorkpoolError;

async fn scatter_gather_computation(
    leader: &LeaderHandle,
    data: Vec<f64>,
) -> Result<Vec<f64>, WorkpoolError> {
    let num_workers = leader.num_workers();
    let chunk_size = (data.len() + num_workers - 1) / num_workers;

    // note: each chunk of data should in practice be an individual job.
    let mut msgs = Vec::new();
    for (worker_id, chunk) in data.chunks(chunk_size).enumerate() {
        let payload = bincode::serialize(chunk).unwrap();
        msgs.push(WorkerJob {
            worker_id: worker_id as u16,
            payload,
        });
    }

    // Scatter to workers, gather results
    let responses = leader.scatter_gather(msgs).await?;

    // Deserialize and combine results (handle potential errors)
    let mut results = Vec::new();
    for rsp in responses {
        match &rsp.payload {
            Ok(data) => {
                let chunk_result: Vec<f64> = bincode::deserialize(data).unwrap();
                results.extend(chunk_result);
            }
            Err(e) => {
                eprintln!("Worker {} failed: {}", rsp.worker_id, e);
                return Err(WorkpoolError::InvalidInput(format!("Worker {} failed", rsp.worker_id)));
            }
        }
    }

    Ok(results)
}
```

### Worker Side

```rust
use ampc_actor_utils::network::workpool::worker::Job;

fn handle_scatter_gather_job(mut job: Job) {
    let payload = job.take_payload();

    // Deserialize chunk
    let chunk: Vec<f64> = bincode::deserialize(&payload).unwrap();

    // Process chunk (e.g., square all values)
    let results: Vec<f64> = chunk.iter().map(|x| x * x).collect();

    // Send results back
    let response_payload = bincode::serialize(&results).unwrap();
    job.send_result(response_payload);
}
```

## Pattern 3: Pipelining Multiple Jobs

**Use Case**: Submit multiple jobs concurrently to maximize throughput.

```rust
use ampc_actor_utils::network::workpool::leader::LeaderHandle;
use ampc_actor_utils::network::workpool::WorkpoolError;

async fn pipelined_operations(leader: &LeaderHandle) -> Result<(), WorkpoolError> {
    // Submit multiple jobs without waiting
    let job1 = leader.broadcast(vec![1, 2, 3]);
    let job2 = leader.broadcast(vec![4, 5, 6]);
    let job3 = leader.broadcast(vec![7, 8, 9]);

    // All jobs execute concurrently
    let (r1, r2, r3) = tokio::join!(job1, job2, job3);

    // Check results
    let responses1 = r1?;
    let responses2 = r2?;
    let responses3 = r3?;

    println!("Completed 3 pipelined jobs");
    println!("Job 1: {} workers", responses1.len());
    println!("Job 2: {} workers", responses2.len());
    println!("Job 3: {} workers", responses3.len());

    Ok(())
}
```

## Pattern 4: Async Worker Processing

**Use Case**: Workers perform async I/O or long-running computations.

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
    let result = async_processing(payload).await;

    // Send result
    job.send_result(result);
}

async fn async_processing(data: Vec<u8>) -> Vec<u8> {
    // Simulate async I/O
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    data
}
```

## Error Handling

### Handling Network Failures

```rust
use ampc_actor_utils::network::workpool::leader::LeaderHandle;
use ampc_actor_utils::network::workpool::WorkpoolError;

async fn robust_broadcast(leader: &LeaderHandle, payload: Vec<u8>) {
    match leader.broadcast(payload.clone()).await {
        Ok(responses) => {
            println!("Received {} responses", responses.len());

            // Check individual worker responses
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
        Err(WorkpoolError::InvalidInput(msg)) => {
            eprintln!("Invalid input: {}", msg);
        }
        Err(WorkpoolError::SendFailed) => {
            eprintln!("Lost connection to worker tasks - system may be shutting down");
        }
        Err(e) => {
            eprintln!("System error: {}", e);
        }
    }
}
```

### Worker Error Handling

```rust
use ampc_actor_utils::network::workpool::worker::Job;

fn safe_job_handler(mut job: Job) {
    let payload = job.take_payload();

    match process_with_error_handling(payload) {
        Ok(result) => {
            job.send_result(result);
        }
        Err(e) => {
            // Encode error as response
            let error_msg = format!("ERROR: {}", e);
            job.send_result(error_msg.into_bytes());
        }
    }
}

fn process_with_error_handling(data: Vec<u8>) -> Result<Vec<u8>, String> {
    // Validate input
    if data.is_empty() {
        return Err("Empty payload".to_string());
    }

    // Process data
    Ok(data)
}
```

## Handling Dropped Jobs

When workers reconnect after a network disruption, the system automatically detects and handles dropped jobs.

### How It Works

```rust
use ampc_actor_utils::network::workpool::leader::LeaderHandle;
use ampc_actor_utils::network::workpool::WorkpoolError;

async fn resilient_computation(leader: &LeaderHandle) {
    let payload = vec![1, 2, 3, 4, 5];

    match leader.broadcast(payload).await {
        Ok(responses) => {
            // Responses may contain both successes and dropped job errors
            let mut successes = 0;
            let mut dropped = 0;

            for rsp in responses {
                match &rsp.payload {
                    Ok(_) => successes += 1,
                    Err(WorkpoolError::JobsLost { .. }) => {
                        dropped += 1;
                        eprintln!("Worker {} dropped the job", rsp.worker_id);
                    }
                    Err(e) => {
                        eprintln!("Worker {} error: {}", rsp.worker_id, e);
                    }
                }
            }

            println!("Results: {} successes, {} dropped", successes, dropped);

            // Decide how to handle partial results
            if dropped > 0 {
                println!("Some jobs were dropped - retrying...");
                // Implement retry logic here
            }
        }
        Err(e) => {
            eprintln!("System error: {}", e);
        }
    }
}
```

### Automatic Reconciliation

After reconnection:
1. Leader queries worker's last received job ID
2. Any pending jobs with ID < last received are marked as dropped
3. Dropped jobs complete with `WorkpoolError::JobsLost` in their payload
4. Application receives mixed results and can implement retry logic

### Best Practices

1. **Check Individual Responses**: Always check each `WorkerRsp.payload` for errors
2. **Implement Retry Logic**: Handle dropped jobs by retrying or using backup strategies
3. **Monitor Dropped Jobs**: Track `JobsLost` errors to detect network issues
4. **Partial Results**: Decide if partial success is acceptable for your use case
```
