# Worker Pool Networking Design: Job ID Tracking & Architecture Split

## Context

This design supports the horizontal scaling architecture described in `scaling_design.md`, where iris-mpc computation is split between:
- **Core nodes** (one per MPC party): Own HNSW graph, orchestrate MPC protocol, handle SQS/SNS
- **Worker nodes** (fleet per party): Hold sharded iris database, execute dot product computations

Workers connect to their party's core node. The core node acts as a **leader** that:
1. Receives batches of work: `Vec<QuerySpec>` with flattened `Vec<u32>` target serial IDs
2. Partitions targets by worker shard (`serial_id % num_workers`)
3. Scatters work units to workers
4. Gathers responses and reassembles results in original order

Key insight: Even a single SQS batch generates **many** `compute_dot_products` calls during HNSW graph search. Request pipelining is essential to avoid wasting latency.

## Architecture Split: ampc-common vs iris-mpc

### Repositories
- **ampc-common**: Generic networking infrastructure (not iris-specific)
  - TCP connection management
  - Framing protocol
  - Job ID tracking and request-response correlation
  - Generic scatter-gather primitives

- **iris-mpc**: Iris-specific business logic
  - `IrisWorkerPool` trait and implementations
  - Iris message types (CacheQueries, ComputeDotProducts, etc.)
  - Shard partitioning logic
  - Result reassembly with iris-specific semantics

### Why This Split?

The existing `ampc-common` networking is designed for **peer-to-peer 3MPC** (symmetric party-to-party). The worker pool needs **leader-worker** patterns (asymmetric, scatter-gather). However, the core networking primitives (framing, job tracking, connection management) are reusable.

**Split principle**: `ampc-common` provides generic request-response infrastructure with job tracking. `iris-mpc` implements iris-specific logic on top of those primitives.

## Design: Job ID Tracking

### Why Job IDs?

1. **Request pipelining**: Graph search issues multiple overlapping `compute_dot_products` calls
2. **Scatter-gather**: Each call fans out to multiple workers, needs correlation for reassembly
3. **Connection resilience**: Track in-flight jobs if connection drops (critical for Phase 4 error handling)
4. **Decouples work from connections**: Can have multiple outstanding requests per worker

### Message Structure (ampc-common)

Define generic request-response envelope:

```rust
/// Generic request envelope with job tracking
#[derive(Serialize, Deserialize)]
pub struct JobRequest<T> {
    pub job_id: u64,          // Monotonic ID assigned by leader
    pub partition_id: u16,    // Which partition of this job (for scatter-gather)
    pub payload: T,           // Application-specific request type
}

/// Generic response envelope
#[derive(Serialize, Deserialize)]
pub struct JobResponse<T> {
    pub job_id: u64,
    pub partition_id: u16,
    pub payload: Result<T, String>,  // Application-specific response or error
}
```

### Leader State Machine (ampc-common)

Generic scatter-gather infrastructure:

```rust
/// Generic job tracker for scatter-gather operations
pub struct JobTracker<Req, Resp> {
    next_job_id: AtomicU64,
    pending_jobs: DashMap<u64, PendingJob<Resp>>,
    _phantom: PhantomData<Req>,
}

struct PendingJob<Resp> {
    job_type: JobType,
    partitions_pending: HashSet<u16>,
    results: Vec<Option<Result<Resp, String>>>,  // indexed by partition_id
    response_tx: oneshot::Sender<Result<AssembledResponse<Resp>>>,
}

pub enum JobType {
    /// Scatter work to multiple workers, gather and reassemble results
    ScatterGather {
        num_partitions: u16,
    },

    /// Broadcast same message to all workers, wait for all acks
    Broadcast {
        num_workers: u16,
    },
}

pub enum AssembledResponse<T> {
    /// Scatter-gather: results from each partition in order
    Gathered(Vec<T>),

    /// Broadcast: all workers acked
    BroadcastComplete,
}
```

### Connection Manager (ampc-common)

Manages TCP connections with framing and job correlation:

```rust
/// Manages connections to a pool of worker nodes
pub struct WorkerConnectionPool<Req, Resp> {
    workers: Vec<WorkerConnection<Req, Resp>>,
    job_tracker: Arc<JobTracker<Req, Resp>>,
}

struct WorkerConnection<Req, Resp> {
    worker_id: u16,
    addr: SocketAddr,
    tx: mpsc::Sender<JobRequest<Req>>,
    // Background task handles: sending, receiving, framing
}

impl<Req, Resp> WorkerConnectionPool<Req, Resp>
where
    Req: Serialize + Send + 'static,
    Resp: DeserializeOwned + Send + 'static,
{
    /// Create pool and connect to all workers
    pub async fn new(worker_addrs: Vec<SocketAddr>) -> Result<Self>;

    /// Broadcast to all workers, wait for all acks
    pub async fn broadcast(
        &self,
        payload: Req,
    ) -> Result<()>;

    /// Scatter-gather: send different payloads to different workers,
    /// gather results in partition order
    pub async fn scatter_gather(
        &self,
        partitions: Vec<(u16, Req)>,  // (worker_id, payload)
    ) -> Result<Vec<Resp>>;
}
```

### Worker Message Handler (ampc-common)

Workers receive framed messages, process, and echo job IDs:

```rust
#[async_trait]
pub trait RequestHandler<Req, Resp>: Send + Sync {
    /// Application-specific request processing
    /// Worker just processes and returns response - no job tracking needed
    async fn handle_request(&self, request: Req) -> Result<Resp, String>;
}

/// Worker-side message handler with static dispatch
pub struct WorkerMessageHandler<H, Req, Resp>
where
    H: RequestHandler<Req, Resp>,
{
    tcp_listener: TcpListener,
    handler: H,
}

impl<H, Req, Resp> WorkerMessageHandler<H, Req, Resp>
where
    H: RequestHandler<Req, Resp>,
    Req: DeserializeOwned + Send + 'static,
    Resp: Serialize + Send + 'static,
{
    /// Process incoming request, echo job_id/partition_id in response
    async fn handle_message(&self, req: JobRequest<Req>) -> JobResponse<Resp> {
        let result = self.handler.handle_request(req.payload).await;

        JobResponse {
            job_id: req.job_id,
            partition_id: req.partition_id,
            payload: result,
        }
    }
}
```

**Key insight**: Workers are **stateless** for job tracking. They just process requests and echo IDs back. All scatter-gather complexity lives in the leader's `JobTracker`.

## Iris-Specific Layer (iris-mpc)

### Message Types

Define iris-specific protocol on top of generic `JobRequest`/`JobResponse`:

```rust
// In iris-mpc-cpu crate

/// Request types sent from core node to workers
#[derive(Serialize, Deserialize)]
pub enum WorkerRequest {
    CacheQueries {
        queries: Vec<(QueryId, GaloisRingSharedIris)>,
    },

    FetchIrises {
        serial_ids: Vec<u32>,
    },

    ComputeDotProducts {
        queries: Vec<QuerySpec>,
        target_serial_ids: Vec<u32>,  // Only targets for this worker's shard
    },

    InsertIrises {
        inserts: Vec<(QueryId, u32, i16)>,  // (query_id, serial_id, version_id)
    },

    EvictQueries {
        query_ids: Vec<QueryId>,
    },
}

/// Response types from workers to core node
#[derive(Serialize, Deserialize)]
pub enum WorkerResponse {
    CacheComplete,

    FetchedIrises {
        irises: Vec<(u32, GaloisRingSharedIris)>,
    },

    DotProducts {
        results: Vec<RingElement<u16>>,  // Flattened results in query order
    },

    InsertComplete {
        shard_checksum: ShardChecksum,
    },

    EvictComplete,
}
```

### RemoteIrisWorkerPool

Implements `IrisWorkerPool` trait using `ampc-common` networking:

```rust
// In iris-mpc-cpu crate

pub struct RemoteIrisWorkerPool {
    num_workers: u16,
    // Generic connection pool from ampc-common
    connections: WorkerConnectionPool<WorkerRequest, WorkerResponse>,
}

#[async_trait]
impl IrisWorkerPool for RemoteIrisWorkerPool {
    async fn cache_queries(
        &self,
        queries: Vec<(QueryId, GaloisRingSharedIris)>,
    ) -> Result<()> {
        // Broadcast to all workers
        self.connections.broadcast(
            WorkerRequest::CacheQueries { queries }
        ).await
    }

    async fn fetch_irises(
        &self,
        serial_ids: Vec<u32>,
    ) -> Result<Vec<(u32, GaloisRingSharedIris)>> {
        // Partition by worker shard
        let partitions = self.partition_by_shard(serial_ids);

        // Scatter-gather
        let requests: Vec<_> = partitions.into_iter()
            .map(|(worker_id, shard_serial_ids)| {
                (worker_id, WorkerRequest::FetchIrises {
                    serial_ids: shard_serial_ids
                })
            })
            .collect();

        let responses = self.connections.scatter_gather(requests).await?;

        // Flatten results from all workers
        let mut all_irises = Vec::new();
        for resp in responses {
            match resp {
                WorkerResponse::FetchedIrises { irises } => {
                    all_irises.extend(irises);
                }
                _ => return Err(anyhow!("Unexpected response type")),
            }
        }
        Ok(all_irises)
    }

    async fn compute_dot_products(
        &self,
        queries: Vec<QuerySpec>,
        target_serial_ids: Vec<u32>,
    ) -> Result<Vec<RingElement<u16>>> {
        // Partition targets by worker shard, maintaining original order
        let partitions = self.partition_targets_for_scatter(
            &queries,
            target_serial_ids,
        );

        // Scatter to workers
        let requests: Vec<_> = partitions.iter()
            .map(|(worker_id, worker_targets)| {
                (*worker_id, WorkerRequest::ComputeDotProducts {
                    queries: queries.clone(),
                    target_serial_ids: worker_targets.clone(),
                })
            })
            .collect();

        let responses = self.connections.scatter_gather(requests).await?;

        // Reassemble results in original target order
        self.reassemble_dot_products(responses, &partitions)
    }

    async fn insert_irises(
        &self,
        inserts: Vec<(QueryId, u32, i16)>,
    ) -> Result<StoreChecksum> {
        // Partition by worker shard
        let partitions = self.partition_inserts_by_shard(inserts);

        let requests: Vec<_> = partitions.into_iter()
            .map(|(worker_id, worker_inserts)| {
                (worker_id, WorkerRequest::InsertIrises {
                    inserts: worker_inserts
                })
            })
            .collect();

        let responses = self.connections.scatter_gather(requests).await?;

        // Combine shard checksums into global checksum
        let mut shard_checksums = Vec::new();
        for resp in responses {
            match resp {
                WorkerResponse::InsertComplete { shard_checksum } => {
                    shard_checksums.push(shard_checksum);
                }
                _ => return Err(anyhow!("Unexpected response type")),
            }
        }
        Ok(StoreChecksum::from_shards(shard_checksums))
    }

    async fn evict_queries(&self, query_ids: Vec<QueryId>) -> Result<()> {
        // Broadcast to all workers
        self.connections.broadcast(
            WorkerRequest::EvictQueries { query_ids }
        ).await
    }
}

impl RemoteIrisWorkerPool {
    /// Partition serial IDs by worker shard
    fn partition_by_shard(&self, serial_ids: Vec<u32>) -> Vec<(u16, Vec<u32>)> {
        let mut partitions: HashMap<u16, Vec<u32>> = HashMap::new();

        for serial_id in serial_ids {
            let worker_id = (serial_id % self.num_workers as u32) as u16;
            partitions.entry(worker_id).or_default().push(serial_id);
        }

        partitions.into_iter().collect()
    }

    /// Partition targets while maintaining reassembly map for original order
    fn partition_targets_for_scatter(
        &self,
        queries: &[QuerySpec],
        target_serial_ids: Vec<u32>,
    ) -> Vec<(u16, Vec<u32>)> {
        // Partition targets by shard while tracking original position
        // for reassembly
        // ... implementation details ...
    }

    /// Reassemble dot product results from workers into original order
    fn reassemble_dot_products(
        &self,
        responses: Vec<WorkerResponse>,
        partitions: &[(u16, Vec<u32>)],
    ) -> Result<Vec<RingElement<u16>>> {
        // Use partition metadata to reconstruct original ordering
        // ... implementation details ...
    }
}
```

### Worker Node Implementation

Worker binary uses `ampc-common` message handler with iris-specific logic:

```rust
// In iris-mpc-worker crate (or iris-mpc-bins)

struct IrisWorkerRequestHandler {
    shard_index: u16,
    num_shards: u16,
    shared_irises: Arc<RwLock<SharedIrises>>,
    query_cache: Arc<RwLock<HashMap<QueryId, Arc<GaloisRingSharedIris>>>>,
    thread_pool: Arc<ThreadPool>,
}

#[async_trait]
impl RequestHandler<WorkerRequest, WorkerResponse> for IrisWorkerRequestHandler {
    async fn handle_request(
        &self,
        request: WorkerRequest,
    ) -> Result<WorkerResponse, String> {
        match request {
            WorkerRequest::CacheQueries { queries } => {
                let mut cache = self.query_cache.write().await;
                for (query_id, iris) in queries {
                    cache.insert(query_id, Arc::new(iris));
                }
                Ok(WorkerResponse::CacheComplete)
            }

            WorkerRequest::FetchIrises { serial_ids } => {
                let store = self.shared_irises.read().await;
                let mut irises = Vec::new();
                for serial_id in serial_ids {
                    // Verify this serial_id belongs to our shard
                    if (serial_id % self.num_shards as u32) != self.shard_index as u32 {
                        return Err(format!("Serial ID {} not in shard {}",
                            serial_id, self.shard_index));
                    }
                    let iris = store.get(serial_id)
                        .ok_or_else(|| format!("Iris {} not found", serial_id))?;
                    irises.push((serial_id, iris));
                }
                Ok(WorkerResponse::FetchedIrises { irises })
            }

            WorkerRequest::ComputeDotProducts { queries, target_serial_ids } => {
                // Verify all targets belong to this shard
                for &serial_id in &target_serial_ids {
                    if (serial_id % self.num_shards as u32) != self.shard_index as u32 {
                        return Err(format!("Target {} not in shard {}",
                            serial_id, self.shard_index));
                    }
                }

                // Execute dot products using existing computation functions
                let results = self.execute_dot_products(queries, target_serial_ids).await?;
                Ok(WorkerResponse::DotProducts { results })
            }

            WorkerRequest::InsertIrises { inserts } => {
                let mut store = self.shared_irises.write().await;
                let cache = self.query_cache.read().await;

                for (query_id, serial_id, version_id) in inserts {
                    // Verify serial_id belongs to our shard
                    if (serial_id % self.num_shards as u32) != self.shard_index as u32 {
                        return Err(format!("Serial ID {} not in shard {}",
                            serial_id, self.shard_index));
                    }

                    let iris = cache.get(&query_id)
                        .ok_or_else(|| format!("Query ID {:?} not cached", query_id))?;
                    store.insert(serial_id, version_id, iris.clone());
                }

                let shard_checksum = store.compute_checksum();
                Ok(WorkerResponse::InsertComplete { shard_checksum })
            }

            WorkerRequest::EvictQueries { query_ids } => {
                let mut cache = self.query_cache.write().await;
                for query_id in query_ids {
                    cache.remove(&query_id);
                }
                Ok(WorkerResponse::EvictComplete)
            }
        }
    }
}
```

## Implementation Phases

### Phase 1: Generic Networking (ampc-common)

**Deliverables:**
- [ ] `JobRequest<T>` / `JobResponse<T>` envelope types
- [ ] `JobTracker<Req, Resp>` with scatter-gather support
- [ ] `WorkerConnectionPool<Req, Resp>` with TCP framing
- [ ] `WorkerMessageHandler<H, Req, Resp>` for worker-side processing (static dispatch)
- [ ] Unit tests for scatter-gather with mock payloads

**Testing:** Generic scatter-gather tests with simple payload types (e.g., `Vec<u32>`).

### Phase 2: Iris Integration (iris-mpc)

**Deliverables:**
- [ ] `WorkerRequest` / `WorkerResponse` enum types
- [ ] `RemoteIrisWorkerPool` implementing `IrisWorkerPool` trait
- [ ] `IrisWorkerRequestHandler` implementing `RequestHandler` trait
- [ ] Shard partitioning logic and result reassembly
- [ ] Worker binary (`iris-mpc-hawk-worker`) using `WorkerMessageHandler`

**Testing:** Integration tests with local worker processes.

### Phase 3: Deployment & Configuration

**Deliverables:**
- [ ] Configuration for worker addresses (`SMPC__WORKER_ADDRS`)
- [ ] Mode selection: `SMPC__WORKER_MODE=local|remote`
- [ ] Docker-compose setup for multi-worker testing
- [ ] Startup synchronization (workers wait for "DB ready" signal)

### Phase 4: Resilient Error Handling

**Deliverables:**
- [ ] Connection health checking in `WorkerConnectionPool`
- [ ] Automatic reconnection with exponential backoff
- [ ] Per-batch failure recovery (fail batch, continue to next)
- [ ] Metrics for job latency, partition timing, connection health

## Key Design Decisions

### 1. Workers Are Stateless for Job Tracking
Workers don't track jobs or maintain request state. They receive a request, process it, and echo the job_id/partition_id back. This keeps workers simple and avoids state synchronization issues.

### 2. Scatter-Gather Reassembly Metadata
For `compute_dot_products`, the leader must maintain a mapping from partition results back to original target order. This metadata is stored in `PendingJob` and used during reassembly.

### 3. Generic Layer Owns Connection Lifetime
`ampc-common` owns TCP connections, framing, and job correlation. `iris-mpc` only defines message types and business logic. This separation allows reusing the networking layer for other applications.

### 4. Broadcast vs Scatter-Gather
- **Broadcast** (CacheQueries, EvictQueries): Same message to all workers, wait for all acks
- **Scatter-Gather** (ComputeDotProducts, FetchIrises, InsertIrises): Different payloads to different workers based on shard partitioning, reassemble results

The `JobType` enum distinguishes these patterns at the generic layer. Note that even single-worker operations (e.g., fetching one serial_id) use scatter-gather with one partition - no special case needed.

### 5. Static Dispatch for WorkerMessageHandler
`WorkerMessageHandler<H, Req, Resp>` uses static dispatch (generic type parameter `H`) rather than dynamic dispatch (`Arc<dyn RequestHandler>`). This eliminates vtable overhead and enables the compiler to inline `handle_request` calls, which is important for high-throughput request processing on worker nodes.

### 6. u16 for Worker and Partition IDs
Worker and partition IDs use `u16` rather than `u8`. This provides headroom for 65,536 workers (vs 256 with `u8`) with negligible cost - due to struct padding for alignment, `u16` typically occupies the same space as `u8` in the message envelopes. This future-proofs the system for extreme horizontal scaling scenarios without a hard limit.

## Open Questions

### 1. Framing Protocol
Does `ampc-common` already have a framing protocol we should reuse? Or should we define a new one for worker pool communication?

**Recommendation:** If the existing 3MPC framing is length-prefixed binary (e.g., `[u32 len][payload]`), reuse it. If it's tightly coupled to 3MPC message types, define a new generic framing layer.

### 2. Backpressure
Should `WorkerConnectionPool::scatter_gather` have bounded concurrency (e.g., max N in-flight jobs)?

**Recommendation:** Start without bounds for Phase 1 (fail fast). Add bounded concurrency in Phase 4 if needed for stability.

### 3. Partial Failure Handling
In scatter-gather, if one worker fails, should we:
- (A) Fail the entire job immediately
- (B) Wait for other workers, then fail with partial results

**Recommendation:** (A) for Phase 1 - fail fast, match existing MPC error semantics. (B) is for Phase 4 resilience.

### 4. Connection Pooling
Should we support multiple concurrent TCP connections per worker for increased throughput?

**Recommendation:** Start with one connection per worker. The job ID tracking already enables request pipelining over a single connection, which should be sufficient. Add connection pooling only if profiling shows connection saturation.

## Performance Considerations

### Request Pipelining
With job IDs, the leader can issue multiple `compute_dot_products` calls without waiting:

```rust
// Graph search issues overlapping requests
let futures: Vec<_> = candidate_batches.iter()
    .map(|batch| pool.compute_dot_products(batch.queries, batch.targets))
    .collect();

let results = join_all(futures).await;
```

This hides network latency during graph traversal.

### Scatter-Gather Parallelism
Each `compute_dot_products` fans out to `num_workers` workers in parallel. With 8 workers, a single request becomes 8 concurrent network calls.

### Memory Overhead
Each pending job stores:
- Metadata for reassembly (~hundreds of bytes)
- Partial results vec (size = num_workers × response_size)

For typical workloads (10-100 in-flight jobs), overhead is <10MB - negligible compared to iris data.

## Next Steps

1. **Review ampc-common framing protocol** - determine if reusable or need new layer
2. **Define `JobRequest`/`JobResponse` in ampc-common** - generic envelope types
3. **Implement `JobTracker` and `WorkerConnectionPool`** - core scatter-gather infrastructure
4. **Define iris message types in iris-mpc** - `WorkerRequest`/`WorkerResponse` enums
5. **Implement `RemoteIrisWorkerPool`** - bridge from `IrisWorkerPool` trait to networking layer
