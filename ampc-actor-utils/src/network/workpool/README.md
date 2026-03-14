# WorkPool Design Documentation

## Table of Contents
1. [Overview](#overview)
2. [Architecture](#architecture)
3. [Connection Establishment](#connection-establishment)
4. [Message Protocol](#message-protocol)
5. [Data Flow](#data-flow)
6. [Job Patterns](#job-patterns)
7. [Job Tracking](#job-tracking)
8. [Reconciliation & Error Handling](#reconciliation--error-handling)
9. [Performance Characteristics](#performance-characteristics)

---

## Overview

The WorkPool is a distributed computing system designed for coordinating work between a single leader node and multiple worker nodes. It provides two primary coordination patterns:

- **Broadcast**: Send the same message to all workers and wait for acknowledgments
- **Scatter-Gather**: Send different messages to different workers and collect results in order

The system is designed for high throughput with support for pipelining, automatic reconnection, and job state reconciliation after network disruptions.

---

## Architecture

### Component Hierarchy

```
┌─────────────────────────────────────────────────────────────┐
│                      LeaderHandle                           │
│  (User-facing API for broadcast/scatter-gather operations)  │
└───────────────────────────┬─────────────────────────────────┘
                            │
                    ┌───────▼────────┐
                    │  leader_task   │
                    │  (Job routing) │
                    └───┬────────┬───┘
                        │        │
            ┌───────────▼───┐   ┌▼────────────┐
            │  JobTracker   │   │ worker_mgr  │ (per worker)
            │ (Coord jobs)  │   │ (Connection)│
            └───────────────┘   └─────────────┘
                                      │
                    ┌─────────────────┼─────────────────┐
                    │                 │                 │
            ┌───────▼───────┐ ┌───────▼───────┐ ┌─────▼─────┐
            │   Worker 0    │ │   Worker 1    │ │  Worker N │
            └───────────────┘ └───────────────┘ └───────────┘
```

### Key Components

#### Leader Side
- **LeaderHandle**: Public API for submitting broadcast/scatter-gather jobs
- **leader_task**: Coordinates job submission and response collection
- **JobTracker**: Tracks pending jobs and assembles results when complete
- **worker_mgr**: Per-worker connection manager with automatic reconnection

#### Worker Side
- **WorkerHandle**: Public API for receiving jobs
- **worker_task**: Manages connection to leader and job state tracking
- **Job**: Encapsulates work request with automatic response channel

---

## Connection Establishment

### Leader Connection Establishment

```rust
// Leader setup process
build_leader(args: LeaderArgs) -> LeaderHandle
    1. Parse leader address and worker addresses
    2. Initialize TLS or TCP listener on leader address
    3. Create TLS or TCP connector for outbound connections
    4. Spawn accept_loop for incoming worker connections
    5. For each worker:
        a. Create per-worker command channel (cmd_tx/cmd_rx)
        b. Spawn worker_mgr task
    6. Spawn leader_task with job coordination
    7. Return LeaderHandle
```

**Per-Worker Manager (worker_mgr)**:
```
Loop forever:
    1. Connect to worker using TCP connect() utility
       - Uses ConnectionId, Identity, Peer abstraction
       - Automatic retry with 1-second backoff on failure

    2. Perform job reconciliation (detect_dropped_jobs)
       - Query worker's last received job_id
       - Compare with leader's last sent job_id
       - Detect lost jobs during disconnection

    3. Split connection into read/write halves
       - Write half: handle_outbound_traffic (sends requests)
       - Read half: handle_inbound_traffic (receives responses)

    4. Run until connection closes or shutdown
       - Track last_sent_job_id for reconciliation
       - Convert network responses to WorkerRsp

    5. On disconnect: log, wait 1 second, reconnect
```

### Worker Connection Establishment

```rust
// Worker setup process
build_worker_handle(args: WorkerArgs) -> WorkerHandle
    1. Parse worker and leader addresses
    2. Initialize TLS or TCP listener on worker address
    3. Create TLS or TCP connector for outbound connections
    4. Spawn accept_loop for incoming leader connections
    5. Spawn worker_task
    6. Return WorkerHandle with job receiver channel
```

**Worker Task (worker_task)**:
```
Loop forever:
    1. Connect to leader using TCP connect() utility
       - Uses ConnectionId(0) - worker has one connection
       - Automatic retry with 1-second backoff on failure

    2. Create response channel (rsp_tx/rsp_rx)
       - Workers send responses via this channel

    3. Split connection into read/write halves
       - Write half: handle_outbound_traffic (sends responses)
       - Read half: handle_inbound_traffic (receives requests)

    4. Track job state:
       - last_received_job_id: Last job received from leader
       - last_responded_job_id: Last response sent to leader
       - Used for reconciliation after reconnection

    5. On disconnect: log, wait 1 second, reconnect
```

### Connection Properties

- **Bidirectional**: Both sides can accept and initiate connections
- **Resilient**: Automatic reconnection with exponential backoff
- **Stateful**: Tracks job IDs for reconciliation after network disruptions
- **TLS Support**: Optional TLS encryption for all connections

---

## Message Protocol

### NetworkValue Enum

The protocol uses a compact binary format with a descriptor byte prefix:

```rust
enum NetworkValue {
    Request {
        job_id: u32,
        worker_id: u16,
        payload: Vec<u8>,
    },
    Response {
        job_id: u32,
        worker_id: u16,
        payload: Vec<u8>,
    },
    QueryJobState {
        worker_id: u16,
    },
    JobStateResponse {
        worker_id: u16,
        last_received_job_id: Option<u32>,
        last_responded_job_id: Option<u32>,
    },
}
```

### Wire Format

#### Request/Response (Descriptor: 0x01/0x02)
```
┌──────────┬──────────┬───────────┬─────────────┬─────────┐
│Descriptor│ job_id   │ worker_id │ payload_len │ payload │
│  1 byte  │ 4 bytes  │  2 bytes  │  4 bytes    │ N bytes │
└──────────┴──────────┴───────────┴─────────────┴─────────┘
Total: 11 + N bytes
```

#### QueryJobState (Descriptor: 0x03)
```
┌──────────┬───────────┐
│Descriptor│ worker_id │
│  1 byte  │  2 bytes  │
└──────────┴───────────┘
Total: 3 bytes
```

#### JobStateResponse (Descriptor: 0x04)
```
┌──────────┬───────────┬─────────────────┬─────────────────┐
│Descriptor│ worker_id │ last_received   │ last_responded  │
│  1 byte  │  2 bytes  │ 1+4 bytes       │ 1+4 bytes       │
└──────────┴───────────┴─────────────────┴─────────────────┘
Total: 13 bytes

Option<u32> encoding:
  - First byte: 0 = None, 1 = Some
  - Next 4 bytes: u32 value (if Some)
```

### Message Batching

**Outbound Traffic Handler** (`handle_outbound_traffic`):
- Receives messages from unbounded channel
- Attempts to batch multiple messages into single write
- Buffer capacity: 64 MB maximum
- Batching logic:
  ```rust
  1. Receive first message (blocking)
  2. Serialize to buffer
  3. Try to receive more messages (non-blocking)
  4. Add to buffer if under 64 MB limit
  5. Write all batched messages
  6. Flush
  ```

**Inbound Traffic Handler** (`handle_inbound_traffic`):
- Reads descriptor byte first
- Determines message length based on descriptor
- Reads remaining bytes in one or more reads
- Deserializes and forwards to appropriate channel
- Auto-resizes buffer if payload exceeds 64 KB default

---

## Data Flow

### Leader → Worker (Request)

```
User Code
    │
    ▼
LeaderHandle::broadcast(payload)
  or scatter_gather(msgs)
    │
    ▼
Job { Broadcast/ScatterGather, rsp: oneshot }
    │
    ▼
leader_task (tokio::select loop)
    │
    ▼
send_to_workpool(job, worker_cmd_ch, job_tracker)
    │
    ├─→ JobTracker::register_job(job_type, rsp) → job_id
    │
    └─→ For each worker:
            worker_cmd_ch[i].send(NetworkValue::Request {
                job_id,
                worker_id,
                payload
            })
            │
            ▼
        worker_mgr (per-worker task)
            │
            ▼
        handle_outbound_traffic
            │
            ▼
        TCP/TLS Write
            │
            ▼
        ═══════════════════════════════
                 NETWORK
        ═══════════════════════════════
            │
            ▼
        TCP/TLS Read
            │
            ▼
        handle_inbound_traffic (worker side)
            │
            ▼
        convert_to_job(NetworkValue::Request)
            │
            ├─→ Update last_received_job_id
            │
            └─→ Create Job { msg, rsp: rsp_tx }
                    │
                    ▼
                job_tx.send(Job)
                    │
                    ▼
                WorkerHandle::recv() → Some(Job)
                    │
                    ▼
                User Code
```

### Worker → Leader (Response)

```
User Code
    │
    ▼
Job::send_result(payload)
    │
    ▼
rsp_tx.send(NetworkValue::Response {
    job_id,
    worker_id,
    payload
})
    │
    ▼
handle_outbound_traffic (worker side)
    │
    ├─→ Update last_responded_job_id
    │
    ▼
TCP/TLS Write
    │
    ▼
═══════════════════════════════
         NETWORK
═══════════════════════════════
    │
    ▼
TCP/TLS Read
    │
    ▼
handle_inbound_traffic (leader side)
    │
    ▼
convert_to_worker_rsp(NetworkValue::Response)
    │
    ▼
worker_rsp_tx.send(WorkerRsp {
    job_id,
    worker_id,
    payload
})
    │
    ▼
leader_task (tokio::select loop)
    │
    ▼
handle_worker_response(rsp, job_tracker)
    │
    ▼
JobTracker::record_response(rsp)
    │
    ├─→ Remove worker_id from partitions_pending
    ├─→ Store result in results[worker_id]
    │
    └─→ If partitions_pending.is_empty():
            │
            ▼
        complete_job(job_id)
            │
            ├─→ Assemble Vec<WorkerRsp> from results
            ├─→ Check for errors
            │
            └─→ response_tx.send(assembled_results)
                    │
                    ▼
                LeaderHandle::broadcast/scatter_gather
                    │
                    ▼
                User Code (receives Vec<WorkerRsp>)
```

---

## Job Patterns

### Broadcast Pattern

**Use Case**: Send the same message to all workers

```rust
// User API
let responses = leader.broadcast(payload).await?;
```

**Flow**:
1. Leader creates `Job::Broadcast { payload, rsp }`
2. JobTracker registers job with `JobType::Broadcast { num_workers }`
3. Leader sends same payload to all workers with same job_id
4. Each worker processes independently and sends response
5. JobTracker collects responses from all workers
6. When all workers respond, assembled `Vec<WorkerRsp>` returned

**Characteristics**:
- Same job_id for all workers
- Different worker_ids (0..num_workers)
- Completes when all workers respond
- Results returned in worker_id order (0, 1, 2, ...)

### Scatter-Gather Pattern

**Use Case**: Send different messages to different workers, collect ordered results

```rust
// User API
let msgs = vec![
    WorkerRsp { worker_id: 0, payload: vec![...], job_id: 0 },
    WorkerRsp { worker_id: 1, payload: vec![...], job_id: 0 },
    // ... more workers
];
let responses = leader.scatter_gather(msgs).await?;
```

**Flow**:
1. Leader creates `Job::ScatterGather { msgs, rsp }`
2. JobTracker registers job with `JobType::ScatterGather { num_partitions }`
3. Leader sends different payload to each specified worker
4. Each worker processes its unique payload and sends response
5. JobTracker collects responses from all partitions
6. When all partitions respond, assembled `Vec<WorkerRsp>` returned

**Characteristics**:
- Same job_id for all workers
- Different worker_ids as specified in msgs
- Different payloads per worker
- Completes when all specified workers respond
- Results returned in worker_id order

### Pipelining

**Key Feature**: The system supports pipelining multiple jobs in flight simultaneously.

```rust
// Multiple jobs can be submitted without waiting
let job1 = leader.broadcast(payload1);
let job2 = leader.scatter_gather(msgs2);
let job3 = leader.broadcast(payload3);

// Jobs execute concurrently
let (r1, r2, r3) = tokio::join!(job1, job2, job3);
```

**How Pipelining Works**:

1. **Job ID Sequencing**:
   - JobTracker uses atomic counter for job IDs
   - Each job gets unique, monotonically increasing ID
   - Jobs tracked independently in DashMap

2. **Concurrent Transmission**:
   - Multiple jobs can be sent to workers without waiting
   - Each worker processes jobs in order received
   - Responses can return out of order

3. **Independent Completion**:
   - JobTracker tracks each job separately
   - Job completes when all its workers respond
   - No blocking between different job IDs

4. **Benefits**:
   - Hides network latency
   - Maximizes worker utilization
   - Improves overall throughput
   - Allows overlapping computation and communication

**Example Timeline**:
```
Time →
Job 0: [Submit]──[Worker 0]──[Worker 1]──[Complete]
Job 1:      [Submit]──[Worker 0]──[Worker 1]──[Complete]
Job 2:           [Submit]──[Worker 0]──[Worker 1]──[Complete]
                     ▲          ▲          ▲
                All jobs running concurrently on workers
```

---

## Job Tracking

### JobTracker Design

The JobTracker is the core component for coordinating multi-worker operations.

```rust
pub struct JobTracker {
    next_job_id: AtomicU32,              // Atomic counter for job IDs
    pending_jobs: DashMap<u32, PendingJob>,  // Concurrent map of active jobs
}

struct PendingJob {
    job_type: JobType,                   // Broadcast or ScatterGather
    partitions_pending: HashSet<u16>,    // Set of worker_ids awaiting response
    results: Vec<Option<Result<WorkerRsp>>>,  // Results indexed by worker_id
    response_tx: oneshot::Sender<Vec<WorkerRsp>>,  // Channel to caller
}

pub enum JobType {
    ScatterGather { num_partitions: u16 },
    Broadcast { num_workers: u16 },
}
```

### Job Lifecycle

```
┌─────────────────────────────────────────────────────────────┐
│                    1. Job Registration                      │
└─────────────────────────────────────────────────────────────┘
    register_job(job_type, response_tx) → job_id
    - Atomically increment next_job_id
    - Initialize partitions_pending = {0..num_workers}
    - Initialize results = [None, None, ..., None]
    - Store PendingJob in DashMap
    - Return job_id

┌─────────────────────────────────────────────────────────────┐
│                  2. Response Collection                     │
└─────────────────────────────────────────────────────────────┘
    record_response(WorkerRsp { job_id, worker_id, payload })
    - Lookup PendingJob by job_id
    - Check worker_id is in partitions_pending
    - Remove worker_id from partitions_pending
    - Store result in results[worker_id]
    - If partitions_pending.is_empty():
        → complete_job(job_id)

┌─────────────────────────────────────────────────────────────┐
│                    3. Job Completion                        │
└─────────────────────────────────────────────────────────────┘
    complete_job(job_id)
    - Remove PendingJob from DashMap
    - Unwrap all results from Vec<Option<Result<WorkerRsp>>>
    - Check for errors in any result
    - If any error: return early (response_tx dropped)
    - Assemble Vec<WorkerRsp> in order
    - Send via response_tx to caller

┌─────────────────────────────────────────────────────────────┐
│                    4. Job Cancellation                      │
└─────────────────────────────────────────────────────────────┘
    cancel_job(job_id, error)
    - Remove PendingJob from DashMap
    - Drop response_tx (caller receives RecvError)
    - Log error message
```

### Concurrency Safety

- **DashMap**: Lock-free concurrent HashMap for pending_jobs
- **AtomicU32**: Lock-free job ID generation
- **No Global Lock**: Each job tracked independently
- **Linearizable**: Job completion is atomic remove + send

### Error Handling in JobTracker

1. **Duplicate Response**:
   - `record_response` checks `partitions_pending.remove()`
   - Returns error if worker_id not in set
   - Prevents double-counting

2. **Out-of-Bounds Worker ID**:
   - Validates `worker_id < results.len()`
   - Returns error if invalid
   - Protects against index panics

3. **Worker Error**:
   - `record_error(job_id, worker_id, error)` stores error in results
   - Job still waits for all workers
   - Error propagated during completion

4. **Missing Results**:
   - `complete_job` checks all results are Some
   - Returns error if any partition missing
   - Should never happen if logic correct

5. **Job Not Found**:
   - All methods return error if job_id not in map
   - Happens if job already completed/cancelled
   - Caller should handle gracefully

---

## Reconciliation & Error Handling

### Job State Reconciliation

After a network disconnection, leader and worker may have inconsistent views of which jobs were sent/received. The system uses a reconciliation protocol to detect lost jobs.

#### Leader-Side Reconciliation (detect_dropped_jobs)

```rust
async fn detect_dropped_jobs(
    conn: &mut T,
    worker_id: u16,
    expected_last_sent: Option<u32>,
) -> io::Result<()>
```

**Process**:
1. Leader reconnects to worker
2. Leader sends `QueryJobState { worker_id }`
3. Leader reads `JobStateResponse { worker_id, last_received_job_id, ... }`
4. Compare `expected_last_sent` with `last_received_job_id`
5. If mismatch:
   - Log `NetworkFailure::JobsLost` error
   - TODO: Cancel all pending jobs for this worker

**Scenarios**:

| Leader Last Sent | Worker Last Received | Outcome |
|------------------|----------------------|---------|
| None | None | ✓ First connection |
| Some(100) | Some(100) | ✓ No jobs lost |
| Some(100) | Some(95) | ✗ Jobs 96-100 lost |
| Some(100) | None | ✗ All jobs lost |

#### Worker-Side State Tracking

Workers maintain two counters:
```rust
last_received_job_id: Option<u32>   // Last Request received
last_responded_job_id: Option<u32>  // Last Response sent
```

Updated during message processing:
- `NetworkValue::Request` → update `last_received_job_id`
- `NetworkValue::Response` → update `last_responded_job_id`
- `NetworkValue::QueryJobState` → send both values

### Connection Failure Handling

#### Worker Manager Reconnection Loop

```rust
loop {
    // 1. Attempt connection
    let conn = connect(...).await;
    if conn.is_err() {
        sleep(1 second);
        continue;  // Retry connection
    }

    // 2. Reconcile job state
    if detect_dropped_jobs(...).is_err() {
        // TODO: Cancel pending jobs
    }

    // 3. Run traffic handlers
    tokio::select! {
        _ = handle_outbound_traffic(...) => { /* closed */ },
        _ = handle_inbound_traffic(...) => { /* closed */ },
        _ = shutdown_ct.cancelled() => { break; },
    }

    // 4. Reconnect
    sleep(1 second);
}
```

**Behavior**:
- Infinite reconnection attempts
- 1-second backoff between attempts
- Preserves command channel across reconnects
- Messages queue in channel during disconnect

#### Worker Reconnection Loop

```rust
loop {
    // 1. Attempt connection to leader
    let conn = connect(...).await;
    if conn.is_err() {
        sleep(1 second);
        continue;
    }

    // 2. Create new response channel
    let (rsp_tx, rsp_rx) = unbounded_channel();

    // 3. Run traffic handlers
    tokio::select! {
        _ = handle_outbound_traffic(...) => { /* closed */ },
        _ = handle_inbound_traffic(...) => { /* closed */ },
        _ = shutdown_ct.cancelled() => { break; },
    }

    // 4. Reconnect
    sleep(1 second);
}
```

**Behavior**:
- Job state (last_received/last_responded) preserved across reconnects
- Response channel recreated each connection
- Responds to QueryJobState with current state

### Error Types

#### NetworkFailure (recoverable)
```rust
pub enum NetworkFailure {
    JobsLost { worker_id, expected, actual },  // Detected via reconciliation
    WorkerDisconnected { worker_id },          // Connection lost
    JobTimeout(job_id),                        // Job exceeded deadline
    ChannelClosed,                             // Internal channel error
}
```

#### LeaderError (fatal)
```rust
pub enum LeaderError {
    ConnectionError(String),   // Failed to bind/listen
    ParseError(String),        // Invalid address
    ChannelClosed,             // Internal error
    ChannelRecvError,          // Oneshot receive failed
    IoError,                   // I/O failure
}
```

#### WorkerError (fatal)
```rust
pub enum WorkerError {
    ConnectionError(String),   // Failed to bind/listen
    ParseError(String),        // Invalid address
    ChannelClosed,             // Internal error
    IoError,                   // I/O failure
}
```

### Future Error Handling TODO

From the code comments, these error handling paths are incomplete:

1. **Reconciliation Failures** (leader_task.rs:194):
   ```rust
   if let Err(e) = detect_dropped_jobs(...) {
       // TODO: emit error for every job being tracked
   }
   ```
   Should cancel all pending jobs involving the affected worker.

2. **Job Loss Notification** (leader_task.rs:280):
   ```rust
   todo!("send error via channel");
   ```
   Should propagate `NetworkFailure::JobsLost` to application layer.

---

## Performance Characteristics

### Throughput

**Message Batching**:
- Up to 64 MB per batch
- Amortizes syscall overhead
- Multiple messages per write/flush

**Pipelining**:
- Unlimited in-flight jobs
- No head-of-line blocking
- Concurrent job execution

**Lock-Free JobTracker**:
- DashMap for concurrent access
- AtomicU32 for job ID generation
- No global synchronization

### Latency

**Single Job Latency**:
```
L = network_rtt + worker_processing + serialization + jobtracker_overhead

Where:
  network_rtt: 2 × one-way network latency
  worker_processing: application-specific
  serialization: ~microseconds for small payloads
  jobtracker_overhead: ~microseconds (DashMap lookup + insert)
```

**Pipelined Throughput**:
```
Throughput ≈ num_workers / max(network_rtt, worker_processing)

Assumes:
  - Workers process jobs in parallel
  - Network does not saturate
  - JobTracker does not bottleneck
```

### Memory

**Per Job**:
```
Job overhead = PendingJob size ≈ 100 bytes + (num_workers × 100 bytes)

Includes:
  - HashSet<u16> for partitions_pending
  - Vec<Option<Result<WorkerRsp>>> for results
  - oneshot::Sender
```

**Total Memory**:
```
Total = base_overhead + (active_jobs × job_overhead) + message_buffers

Where:
  base_overhead: ~10 KB (tasks, channels)
  message_buffers: 64 KB per connection
```

### Scalability

**Number of Workers**:
- Tested: Unknown (no benchmarks in code)
- Theoretical: Limited by file descriptor limits (~1000s)
- Per worker: ~100 KB memory overhead

**Number of Concurrent Jobs**:
- Limited only by memory
- DashMap scales to millions of entries
- AtomicU32 job IDs wrap at 4 billion

**Message Size**:
- Max message: 64 MB (buffer capacity)
- Recommended: < 1 MB for low latency
- Large messages: batching disabled at 64 MB

### Comparison with Alternatives

| Feature | WorkPool | gRPC Streaming | NATS | MPI |
|---------|----------|----------------|------|-----|
| Pipelining | ✓ | ✓ | ✓ | ✓ |
| Automatic Reconnect | ✓ | ✓ | ✓ | ✗ |
| Job Reconciliation | ✓ | ✗ | ✗ | ✗ |
| Scatter-Gather | ✓ | Manual | Manual | ✓ |
| Broadcast | ✓ | Manual | ✓ | ✓ |
| Type Safety | Payload only | Protobuf | None | Language-specific |
| Overhead | Low | Medium | Low | Low |

---

## Example Usage

### Leader Example

```rust
use ampc_actor_utils::network::workpool::leader::{build_leader, LeaderArgs};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = LeaderArgs {
        leader_id: Identity("leader-1".to_string()),
        leader_address: "127.0.0.1:8000".to_string(),
        worker_addresses: vec![
            "127.0.0.1:8001".to_string(),
            "127.0.0.1:8002".to_string(),
        ],
        tls: None,  // or Some(TlsConfig { ... })
    };

    let shutdown = CancellationToken::new();
    let leader = build_leader(args, shutdown.clone()).await?;

    // Broadcast example
    let responses = leader.broadcast(vec![1, 2, 3]).await?;
    println!("Broadcast responses: {} workers", responses.len());

    // Scatter-gather example
    let msgs = vec![
        WorkerRsp { job_id: 0, worker_id: 0, payload: vec![10] },
        WorkerRsp { job_id: 0, worker_id: 1, payload: vec![20] },
    ];
    let responses = leader.scatter_gather(msgs).await?;
    println!("Scatter-gather responses: {:?}", responses);

    // Pipelining example
    let job1 = leader.broadcast(vec![1]);
    let job2 = leader.broadcast(vec![2]);
    let job3 = leader.broadcast(vec![3]);
    let (r1, r2, r3) = tokio::join!(job1, job2, job3);
    println!("Pipelined jobs complete");

    shutdown.cancel();
    Ok(())
}
```

### Worker Example

```rust
use ampc_actor_utils::network::workpool::worker::{build_worker_handle, WorkerArgs};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = WorkerArgs {
        worker_id: Identity("worker-1".to_string()),
        worker_address: "127.0.0.1:8001".to_string(),
        leader_id: Identity("leader-1".to_string()),
        leader_address: "127.0.0.1:8000".to_string(),
        tls: None,
    };

    let shutdown = CancellationToken::new();
    let mut worker = build_worker_handle(args, shutdown.clone()).await?;

    // Process jobs
    while let Some(mut job) = worker.recv().await {
        let payload = job.take_payload();

        // Do work...
        let result = process(payload);

        // Send response
        job.send_result(result);
    }

    Ok(())
}

fn process(payload: Vec<u8>) -> Vec<u8> {
    // Application-specific processing
    payload
}
```

---

## Conclusion

The WorkPool system provides a robust, high-performance framework for coordinating distributed work between a leader and multiple workers. Key strengths include:

- **Simplicity**: Clean API with two coordination patterns (broadcast, scatter-gather)
- **Reliability**: Automatic reconnection and job state reconciliation
- **Performance**: Pipelining, batching, and lock-free job tracking
- **Flexibility**: Supports both TCP and TLS, arbitrary payload types

The system is well-suited for compute-intensive distributed applications requiring low-latency coordination and high throughput.
