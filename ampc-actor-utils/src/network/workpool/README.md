# WorkPool Design Documentation

## Overview

The WorkPool is a distributed computing system designed for coordinating work between a single leader node and multiple worker nodes. It provides two primary coordination patterns:

- **Broadcast**: Send the same message to all workers and wait for acknowledgments
- **Scatter-Gather**: Send different messages to different workers and collect results (unordered)

The system is designed for high throughput with support for pipelining, automatic reconnection, and job state reconciliation after network disruptions.

---

## Leader Side
- **LeaderHandle**: Public API for submitting broadcast/scatter-gather jobs
- **leader_task**: Coordinates job submission and response collection
- **JobTracker**: Tracks pending jobs and assembles results when complete
- **worker_mgr**: Per-worker connection manager with automatic reconnection


```
┌─────────────────────────────────────────────────────────────┐
│                      LeaderHandle                           │
│  (User-facing API: broadcast(), scatter_gather())           │
└───────────────────────────┬─────────────────────────────────┘
                            │ job_tx channel
                            │                    ┌───────────────────┐
                            ▼                    │    JobHandle      │
                    ┌───────────────┐            │ (Returned to user,│
                    │  leader_task  │────────────│  await or cancel) │
                    │ (Job dispatch │            └─────────▲─────────┘
                    │  + response   │                      │
                    │  collection)  │◄─────────────────────┼─────────┐
                    └───┬───────┬───┘  oneshot channel     │         │
                        │       │      (results)           │         │
                        │       │                          │         │
            ┌───────────▼───┐   │                          │         │
            │  JobTracker   │───┘                          │         │
            │ (Track jobs,  │ complete_job()               │         │
            │  assemble     │                              │         │
            │  results)     │◄─────────────────────────────┼────┐    │
            └───────────────┘  record_response()           │    │    │
                    ▲                                      │    │    │
                    │ validate_pending_jobs()              │    │    │
                    │ (on reconnect)                       │    │    │
                    │                                      │    │    │
            ┌───────┴───────┐                              │    │    │
            │  worker_mgr   │ (one per worker)             │    │    │
            │  ┌──────────┐ │                              │    │    │
            │  │ cmd_rx   │◄├──── worker_cmd_ch ───────────┘    │    │
            │  │ (jobs)   │ │                                   │    │
            │  └──────────┘ │                                   │    │
            │  ┌──────────┐ │                                   │    │
            │  │ rsp_tx   │─├──── worker_rsp_tx ────────────────┘    │
            │  │(responses)│ │                                       │
            │  └──────────┘ │                                        │
            └───────┬───────┘                                        │
                    │                                                │
        ┌───────────┼───────────┐                                    │
        │           │           │                                    │
┌───────▼───────┐ ┌─▼─────────┐ ┌▼──────────┐                        │
│   Worker 0    │ │  Worker 1 │ │ Worker N  │ ───────────────────────┘
└───────────────┘ └───────────┘ └───────────┘   (responses over TCP)
```

**Message Flow:**
1. User calls `broadcast()` or `scatter_gather()` on `LeaderHandle`, receives `JobHandle`
2. Job sent via `job_tx` → `leader_task`
3. `leader_task` registers job with `JobTracker`, dispatches to `worker_mgr`(s) via `worker_cmd_ch`
4. `worker_mgr` sends job over TCP to worker
5. Worker processes and responds over TCP
6. `worker_mgr` receives response, forwards via `worker_rsp_tx`
7. `leader_task` receives response, calls `JobTracker.record_response()`
8. When all partitions complete, `JobTracker` sends result via oneshot channel
9. User's `await` on `JobHandle` resolves with results

### JobTracker

The `JobTracker` maintains the authoritative state for all in-flight jobs on the leader side.
- Each `job_id` maps to exactly one `PendingJob` until completion
- `partitions_pending` contains exactly the set of workers that haven't responded
- A job completes when `partitions_pending` becomes empty
- Results may contain a mix of `Ok` and `Err` payloads (partial failures allowed)
- Once a job is removed from `pending_jobs`, its `job_id` is never reused in that session

**State Transitions:**
```
                register_job()
    (none) ─────────────────────► Pending
                                     │
              ┌──────────────────────┼──────────────────────┐
              │                      │                      │
              ▼                      ▼                      ▼
       record_response()      cancel_job()       check_for_dropped_jobs()
       (per worker)                                  (marks partitions as Err)
              │                      │                      │
              ▼                      │                      │
    partitions_pending.remove()      │                      │
              │                      │                      │
              ▼                      ▼                      ▼
    if empty: complete_job() ──► Results sent via oneshot channel
```

---

## Worker Side
- **WorkerHandle**: Public API for receiving jobs
- **worker_task**: Manages connection to leader and job state tracking
- **Job**: Encapsulates work request with automatic response channel

```
┌─────────────────────────────────────────────────────────────┐
│                      WorkerHandle                           │
│  (User-facing API: recv() returns Jobs to process)          │
└───────────────────────────┬─────────────────────────────────┘
                            ▲
                            │ job_tx channel
                            │
                    ┌───────┴────────┐
                    │  worker_task   │◄──── rsp_tx channel ────┐
                    │ (Connection +  │                         │
                    │  job routing)  │                         │
                    └───────┬────────┘                         │
                            │                                  │
              ┌─────────────┼─────────────┐                    │
              │             │             │                    │
              ▼             ▼             ▼                    │
      ┌─────────────┐ ┌───────────┐ ┌───────────┐              │
      │ accept_loop │ │ pending_  │ │  handle_  │              │
      │ (Incoming   │ │   jobs    │ │ inbound_  │              │
      │ connections)│ │ (Tracking)│ │   msg     │              │
      └─────────────┘ └───────────┘ └───────────┘              │
                                                               │
                    ┌─────────────────────────────┐            │
                    │            Job              │            │
                    │  (User calls send_result()) │────────────┘
                    └─────────────────────────────┘
                                  │
                                  ▼
                          ┌─────────────┐
                          │   Leader    │
                          └─────────────┘
```
**Message Flow:**                                                                                                                                            
1. Leader sends job to worker 
2. `handle_inbound_msg` adds to `pending_jobs` and sends `Job` to `WorkerHandle`
3. User calls `WorkerHandle::recv()` to get the `Job`
4. User processes and calls `Job::send_result()`
5. Response queued in `rsp_tx` → `worker_task` writes to leader
6. Job removed from `pending_jobs` after successful write

