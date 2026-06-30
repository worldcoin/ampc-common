Notable README files:  
- [mpc](mpc/handle/README.md)
- [workpool](workpool/README.md)
---

# `network/` layout
```text
network/
  |-- mpc/
  |-- tcp/
  |-- workpool/
  |-- mod.rs
```

`ampc-actor-utils/src/network` provides two networking stacks: `mpc` and `workpool`. They share `tcp/` which establishes connections.

## `tcp/` module

The following traits are defined in `tcp/stream.rs`.

```text
+--------------------------------+----------------+----------------------------+-------------------------------------------------------+
| Trait                          | Implementation | Location                   | Description                                           |
+--------------------------------+----------------+----------------------------+-------------------------------------------------------+
| NetworkConnection              | TcpStreamConn  | tcp/streams.rs             | Wraps tokio::TcpStream and implements                 |
|                                |                |                            | AsyncRead and AsyncWrite                              |
+--------------------------------+----------------+----------------------------+-------------------------------------------------------+
| NetworkConnection              | TlsStreamConn  | tcp/streams.rs             | Wraps tokio::TlsStream<TcpStream> and implements      |
|                                |                |                            | AsyncRead and AsyncWrite                              |
+--------------------------------+----------------+----------------------------+-------------------------------------------------------+
| Client<Output = TcpStreamConn> | TcpClient      | tcp/connection/client.rs   | Initiates a TCP connection                            |
+--------------------------------+----------------+----------------------------+-------------------------------------------------------+
| Client<Output = TlsStreamConn> | TlsClient      | tcp/connection/client.rs   | Initiates a TLS connection                            |
+--------------------------------+----------------+----------------------------+-------------------------------------------------------+
| Server<Output = TcpStreamConn> | TcpServer      | tcp/connection/server.rs   | Listens for and accepts TCP connections.              |
+--------------------------------+----------------+----------------------------+-------------------------------------------------------+
| Server<Output = TlsStreamConn> | TlsServer      | tcp/connection/server.rs   | Listens for and accepts TLS connections.              |
+--------------------------------+----------------+----------------------------+-------------------------------------------------------+
```

### Connection establishment

Connection logic is in `network/tcp/connection/mod.rs`. The `Client` and `Server` traits provide `connect()` and `accept()` functions. The `accept_loop()` for the server is found in `network/tcp/connection/listener.rs`.

The `connection/mod.rs` module is structured to make the connection establishment process retryable and cancellable. Connections do not automatically reconnect because for the `mpc` use case, a failed connection will most likely result in a failed job, which will cause the sessions to be recreated. At this point, new connections will be created for the sessions.

```text
+-------------------------------------------------------------+
|                         connect()                           |
|                                                             |
|   tokio::select! {                                          |
|       _ = cancellation_tokens => Abort and return Err       |
|       stream = connect_loop() => Exit and return Ok(stream) |
|   }                                                         |
+-------------------------------------------------------------+
                              |
                              v
+-------------------------------------------------------------+
|                       connect_loop()                        |
|                                                             |
|   1. Jitters startup (random 0..3s sleep)                   |
|   2. Calls self.connect()                                   |
|   3. Inspects result:                                       |
|       • Success?         -> Pass up to connect()            |
|       • Fatal Error?     -> Exit loop, return Err           |
|       • Transient Error? -> Sleep 2s, retry step 2          |
+-------------------------------------------------------------+
                              |
                              v
+-------------------------------------------------------------+
|                         connect()                           |
|                                                             |
|   Matches connection type and makes an attempt:             |
|       • Client        -> Dial out & Handshake               |
|       • Server        -> Register listener request & await  |
|                          (accept_loop() does handshake)     |
|       • Bidirectional -> High ID dials, Low ID listens      |
+-------------------------------------------------------------+

```

# The `mpc/` module
```
+---------------------------+     +---------------------------+     +---------------------------+     +---------------------------+
|  SENDERS                  |     |  MULTIPLEXER              |     |  DEMULTIPLEXER            |     |  RECEIVERS                |
+---------------------------+     +---------------------------+     +---------------------------+     +---------------------------+
|                           |     |                           |     |                           |     |                           |
|  [Search Job Session 1]   |---\ |                           |     |                           | /-->|  [Search Job Session 1]   |
|   (UnboundedSender.send)  |    \|                           |     |                           |/    |   (UnboundedReceiver)     |
|                           |     \                           |     |                           |     |                           |
|  [Search Job Session 2]   |---->|   Outbound Async Task     |====>|    Inbound Async Task     |---->|  [Search Job Session 2]   |
|   (UnboundedSender.send)  |     /   Loops on `outbound_rx`  | TCP |    Reads SessionID prefix |     |   (UnboundedReceiver)     |
|                           |    /|   Opportunistic `try_recv`| Wire|    Extracts NetworkValue  |\    |                           |
|  [Search Job Session N]   |---/ |   Writes to TCP stream    |     |    Routes to session chan | \-->|  [Search Job Session N]   |
|   (UnboundedSender.send)  |     |                           |     |                           |     |   (UnboundedReceiver)     |
|                           |     |                           |     |                           |     |                           |
+---------------------------+     +---------------------------+     +---------------------------+     +---------------------------+  
```

## Usage

```text
+-------------------------------------------------------+
|              build_network_handle()                   |
|  Entry point function that initializes the stack      |
+-------------------------------------------------------+
                           |
                           v  Instantiates & returns
+-------------------------------------------------------+
|             Box<dyn NetworkHandle>                    |
|                                                       |
|  • make_network_sessions() -> Vec<NetworkSession>    |
|  • make_sessions()         -> Vec<Session>           |
|  • control_channel()       -> Box<dyn ControlChannel>|
+-------------------------------------------------------+
                           |
                           v  Spawns data-plane channels via
                              make_network_sessions()
+-------------------------------------------------------+
|                 NetworkSession                        |
|                                                       |
|  Ring-Topology Protocol Primitives:                   |
|   • send_next(value)    -> Pushes to next party       |
|   • receive_prev()      -> Awaits from prev party     |
|                                                       |
|  Alternative Helper Primitives:                       |
|   • send_prev(value)    • receive_next()              |
+-------------------------------------------------------+
```

## Implementation Details

```text
+-----------------------+------------------------------------------+-------------------------------------------------------------+
| Component             | Path                                     | Description                                                 |
+-----------------------+------------------------------------------+-------------------------------------------------------------+
| Entry Point           | mpc/handle/mod.rs                        | Contains `build_network_handle()`, which returns a          |
|                       |                                          | NetworkHandle, used to create NetworkSessions.              |
|                       |                                          | `NetworkSession` is defined in `execution/session.rs` and   |
|                       |                                          | wraps the channels produced by `make_network_sessions()`    |
+-----------------------+------------------------------------------+-------------------------------------------------------------+
| NetworkHandle         | mpc/handle/network_handle.rs             | Implements the `NetworkHandle` trait; creates Sessions and  |
| Implementation        |                                          | packages them into NetworkSessions                          |
+-----------------------+------------------------------------------+-------------------------------------------------------------+
| Session Factory       | mpc/handle/session/mod.rs                | Implements `session::make_sessions()`, which builds         |
|                       |                                          | TCP/TLS connections and a set of channels per connection    |
+-----------------------+------------------------------------------+-------------------------------------------------------------+
| Connection Manager    | mpc/handle/session/multiplexer.rs        | Contains `multiplexer::run()`. Splits `NetworkConnection`   |
|                       |                                          | into read/write halves via `tokio::io::split` and runs a    |
|                       |                                          | loop selecting on both halves to route traffic.             |
+-----------------------+------------------------------------------+-------------------------------------------------------------+
```

# The `workpool/` module

Shared code is in the module root. `leader/` and `worker/` have their own network handles. The workpool does not have multiple sessions per connection, which simplifies the code considerably.

```text
+---------------------------------------+     +---------+     +---------------------------------------+
|  LEADER                               |     |         |     |  WORKER                               |
+---------------------------------------+     |         |     +---------------------------------------+
|                                       |     |         |     |                                       |
|  broadcast(payload)               ----|---->|         |---->|----  recv() -> Job                    |
|  scatter_gather(msgs) -> JobHandle    |     |   TCP   |     |                                       |
|                                       |     |         |     |  job.take_payload() -> user code      |
|  await JobHandle -> WorkerRsp     <---|-----|         |<----|----  job.send_result(result)          |
|                                       |     |         |     |                                       |
+---------------------------------------+     |         |     +---------------------------------------+
                                              |         |
+---------------------------------------+     |         |     +---------------------------------------+
|  On reconnect (leader_task.rs):       |     |         |     |  (worker_task.rs)                     |
|                                       |     |         |     |                                       |
|  PendingJobsRequest               ----|---->|         |---->|----  pending JobIds                   |
|  validate_pending_jobs()          <---|-----|         |<----|----  PendingJobsReply                 |
|                                       |     |         |     |                                       |
+---------------------------------------+     +---------+     +---------------------------------------+
```

## Implementation Details

```text
+--------------------+--------------------------------+-------------------------------------------------------------+
| Component          | Path                           | Description                                                 |
+--------------------+--------------------------------+-------------------------------------------------------------+
| Leader Entry Point | workpool/leader/mod.rs         | Contains `build_leader_handle()`, which returns a           |
|                    |                                | LeaderHandle. Used to dispatch work to the workpool.        |
+--------------------+--------------------------------+-------------------------------------------------------------+
| Worker Entry Point | workpool/worker/mod.rs         | Contains `build_worker_handle()`, which returns a           |
|                    |                                | WorkerHandle. Used to receive jobs from the leader.         |
+--------------------+--------------------------------+-------------------------------------------------------------+
| Leader Logic       | workpool/leader/leader_task.rs | Manages connection lifecycle from the leader's perspective; |
|                    |                                | maintains a dedicated worker manager for each worker.       |
+--------------------+--------------------------------+-------------------------------------------------------------+
| Job Tracker        | workpool/leader/job_tracker.rs | Tracks dispatched but uncompleted jobs; reconciles worker-  |
|                    |                                | reported pending lists to detect dropped jobs.              |
+--------------------+--------------------------------+-------------------------------------------------------------+
| Worker Logic       | workpool/worker/worker_task.rs | Manages connection lifecycle from the worker's perspective; |
|                    |                                | tracks pending and completed jobs.                          |
+--------------------+--------------------------------+-------------------------------------------------------------+
```

# Appendix: MPC use case and design choices
iris-mpc is processing batches of 96 requests at a time. The resulting number of jobs is BATCH_SIZE * N_EYES * MIRRORED * OVERLAP = BATCH_SIZE * 2 * 2 * 3
(OVERLAP expresses that instead of processing all +- 15 rotations in a single search, 3 searches over smaller, overlapping ranges are performed)

At a batch size of 96, this is 1152 searches. At a batch size of 64, this is 768 searches.

## Objective
Optimize for latency and throughput of inter-party MPC messaging during parallel HNSW graph traversals, without stalling compute cores or blocking the async runtime.

## Design choices
1. Multiplex searches (sessions) over TCP/TLS connections and do not provide send-acknowledgements.
Send receipts would require a channel or similar atomic synchronization primitive to communicate in the reverse direction - from the multiplexer task to the search task. This would add unnecessary context switching and ping-pong the cache lines. The MPC protocol sends messages in a ring pattern (send next, receive prev), which keeps the parties synchronized.
2. TCP/TLS connection multiplexing vs alternative paradigms:
- the naive approach: one task+connection per session - wasteful
- one task for multiple connections (epoll) - not necessary. Tokio already uses epoll under the hood. Using multiplexed connections is simpler than writing a state machine for epoll and provides similar benefits (by restricting the number of threads available to the Tokio runtime).
3. Bake the messaging protocol directly into NetworkValue.
Alternatives, such as using gRPC results in a large message framing overhead: Protobuf + gRPC + HTTP2 are all extra. Serializing the NetworkValue into a length + descriptor byte format allows for fast single-copy deserialization and, for smaller messages, prevents excessive framing overhead.
4. Abstracting the network connection via Tokio's `AsyncRead` and `AsyncWrite` traits decouples the protocol logic from the transport layer. This allows the networking stack to be initialized with `TCP` for development/testing (simpler, does not require certs) and with `TLS` during production, using the same code paths.

